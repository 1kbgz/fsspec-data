use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use arrow::array::{ArrayRef, Int64Array, RecordBatch};
use arrow::compute::concat_batches;
use bytes::Bytes;
use fsspec_data::manifest::{ManifestReader, ReadLimits, Selection, Snapshot};
use fsspec_data::sink::{recover, FileDataSink, RecoveryAction, SinkOptions, WriteMode};
use fsspec_rs::{
    Condition, ErrorKind, FileInfo, FileSystem, FsFile, FsResult, IoCapabilities, LocalFs,
    MemoryFs, OpenMode, OpenOptions, OperationError, SharedFileSystem, WriteOptions, WriteResult,
};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use uuid::Uuid;

fn batch(values: &[i64]) -> RecordBatch {
    RecordBatch::try_from_iter([(
        "value",
        Arc::new(Int64Array::from(values.to_vec())) as ArrayRef,
    )])
    .unwrap()
}
fn kind<T>(result: Result<T, OperationError>, expected: ErrorKind) {
    match result {
        Err(e) => assert_eq!(e.kind(), expected, "{e}"),
        Ok(_) => panic!("expected {expected:?}"),
    }
}
fn failure(kind: ErrorKind) -> OperationError {
    OperationError::Detailed {
        kind,
        message: "injected".into(),
        retry_safe: false,
    }
}
#[derive(Clone, Copy)]
enum Fault {
    Delay,
    LostAck,
    Deny,
}
struct FaultFs {
    conditional: AtomicBool,
    inner: MemoryFs,
    root: String,
    faults: Mutex<VecDeque<(String, Fault)>>,
    delayed: Mutex<Vec<(String, Vec<u8>)>>,
    writes: Mutex<Vec<(String, Vec<u8>)>>,
}
impl FaultFs {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            conditional: AtomicBool::new(true),
            inner: MemoryFs::new(),
            root: Uuid::new_v4().to_string(),
            faults: Mutex::new(VecDeque::new()),
            delayed: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
        })
    }
    fn inject(&self, suffix: &str, fault: Fault) {
        self.faults
            .lock()
            .unwrap()
            .push_back((suffix.into(), fault));
    }
    fn deliver(&self) -> Vec<Result<WriteResult, OperationError>> {
        self.delayed
            .lock()
            .unwrap()
            .drain(..)
            .map(|(p, b)| {
                self.inner.write(
                    &p,
                    &b,
                    WriteOptions {
                        condition: Some(Condition::Missing),
                    },
                )
            })
            .collect()
    }
    fn sink(self: &Arc<Self>, mode: WriteMode) -> FileDataSink {
        FileDataSink::open(
            self.clone(),
            &self.root,
            mode,
            batch(&[1]).schema(),
            SinkOptions::default(),
        )
        .unwrap()
    }
    fn snapshot(self: &Arc<Self>) -> Result<Snapshot, OperationError> {
        ManifestReader::new(self.clone(), &self.root, ReadLimits::default())
            .unwrap()
            .read(Selection::Latest)
    }
    fn candidate(&self, token: &fsspec_data::sink::RecoveryToken) -> String {
        format!(
            "{}/parts/{:020}/{}/candidate",
            self.root, token.generation, token.publication_id
        )
    }
}
impl FileSystem for FaultFs {
    fn protocol(&self) -> &[&str] {
        &["fault"]
    }
    fn io_capabilities(&self) -> IoCapabilities {
        if self.conditional.load(Ordering::Relaxed) {
            self.inner.io_capabilities()
        } else {
            IoCapabilities::default()
        }
    }
    fn ls(&self, _: &str, _: bool) -> FsResult<Vec<FileInfo>> {
        panic!("sink must not list directories")
    }
    fn info(&self, p: &str) -> FsResult<FileInfo> {
        self.inner.info(p)
    }
    fn open(&self, p: &str, m: OpenMode, o: Option<OpenOptions>) -> FsResult<Box<dyn FsFile>> {
        self.inner.open(p, m, o)
    }
    fn mkdir(&self, p: &str, parents: bool) -> FsResult<()> {
        self.inner.mkdir(p, parents)
    }
    fn rmdir(&self, p: &str) -> FsResult<()> {
        self.inner.rmdir(p)
    }
    fn rm_file(&self, p: &str) -> FsResult<()> {
        self.inner.rm_file(p)
    }
    fn cp_file(&self, a: &str, b: &str) -> FsResult<()> {
        self.inner.cp_file(a, b)
    }
    fn write(&self, p: &str, b: &[u8], o: WriteOptions) -> Result<WriteResult, OperationError> {
        assert_eq!(o.condition, Some(Condition::Missing));
        self.writes.lock().unwrap().push((p.into(), b.to_vec()));
        let fault = {
            let mut faults = self.faults.lock().unwrap();
            if faults
                .front()
                .is_some_and(|(suffix, _)| p.ends_with(suffix))
            {
                faults.pop_front().map(|(_, f)| f)
            } else {
                None
            }
        };
        match fault {
            Some(Fault::Deny) => Err(failure(ErrorKind::PermissionDenied)),
            Some(Fault::Delay) => {
                self.delayed.lock().unwrap().push((p.into(), b.to_vec()));
                Err(failure(ErrorKind::Timeout))
            }
            Some(Fault::LostAck) => {
                self.inner.write(p, b, o)?;
                Err(failure(ErrorKind::Timeout))
            }
            None => self.inner.write(p, b, o),
        }
    }
}
fn rows(snapshot: &Snapshot) -> RecordBatch {
    let mut batches = Vec::new();
    for i in 0..snapshot.manifest().parts.len() {
        let reader =
            ParquetRecordBatchReaderBuilder::try_new(Bytes::from(snapshot.read_part(i).unwrap()))
                .unwrap()
                .build()
                .unwrap();
        batches.extend(reader.map(|b| b.unwrap()));
    }
    concat_batches(
        &Arc::new(snapshot.manifest().schema.clone()),
        batches.iter(),
    )
    .unwrap()
}

#[test]
fn positioned_writes_commit_finish_and_replay() {
    let fs = FaultFs::new();
    let mut sink = fs.sink(WriteMode::Create);
    let token = sink.recovery_token();
    let b = batch(&[1, 2]);
    let first = sink.write_batch(0, "first", &b).unwrap();
    assert_eq!(sink.write_batch(0, "first", &b).unwrap(), first);
    kind(
        sink.write_batch(0, "first", &batch(&[9, 2])),
        ErrorKind::Conflict,
    );
    kind(sink.write_batch(5, "gap", &b), ErrorKind::Gap);
    kind(sink.write_batch(0, "behind", &b), ErrorKind::Conflict);
    assert_eq!(sink.position(), 2);
    sink.flush().unwrap();
    kind(fs.snapshot(), ErrorKind::NotFound);
    let committed = sink.commit().unwrap();
    assert_eq!(committed.recovery, token);
    assert!(!committed.complete);
    assert_eq!(sink.commit().unwrap().recovery, token);
    assert_eq!(sink.recovery_token().generation, 1);
    assert_eq!(sink.write_batch(0, "first", &b).unwrap(), first);
    sink.write_batch(2, "second", &batch(&[3])).unwrap();
    assert_eq!(sink.commit().unwrap().progress, 3);
    let finished = sink.finish().unwrap();
    assert!(finished.complete);
    assert!(finished.marker_error.is_none());
    assert_eq!(sink.finish().unwrap().recovery, finished.recovery);
    assert_eq!(rows(&fs.snapshot().unwrap()), batch(&[1, 2, 3]));
    kind(
        FileDataSink::open(
            fs.clone(),
            &fs.root,
            WriteMode::Append,
            b.schema(),
            SinkOptions::default(),
        ),
        ErrorKind::Conflict,
    );
}

#[test]
fn abort_drop_and_empty_tables_preserve_visibility_rules() {
    let fs = FaultFs::new();
    let mut sink = fs.sink(WriteMode::Create);
    let empty = sink.commit().unwrap();
    assert_eq!(empty.progress, 0);
    sink.write_batch(0, "discard", &batch(&[9])).unwrap();
    sink.flush().unwrap();
    sink.abort().unwrap();
    assert_eq!(fs.snapshot().unwrap().manifest().progress, 0);
    {
        let mut append = fs.sink(WriteMode::Append);
        append.write_batch(0, "dropped", &batch(&[8])).unwrap();
    }
    assert_eq!(fs.snapshot().unwrap().manifest().generation, 0);
    assert!(!fs.snapshot().unwrap().manifest().complete);
    let mut other = fs.sink(WriteMode::Append);
    kind(
        other.write_batch(0, "empty", &batch(&[])),
        ErrorKind::Invalid,
    );
    other.finish().unwrap();
    assert!(fs.snapshot().unwrap().manifest().complete);
}

#[test]
fn historical_receipts_survive_reopen_and_replacement_changes_instance() {
    let fs = FaultFs::new();
    let mut sink = fs.sink(WriteMode::Create);
    let first = sink.write_batch(0, "first", &batch(&[1])).unwrap();
    let original = sink.commit().unwrap();
    sink.write_batch(1, "second", &batch(&[2])).unwrap();
    sink.commit().unwrap();
    sink.close().unwrap();
    let mut reopened = fs.sink(WriteMode::Append);
    assert_eq!(
        reopened.write_batch(0, "first", &batch(&[1])).unwrap(),
        first
    );
    assert_eq!(reopened.position(), 2);
    let mut replacement = fs.sink(WriteMode::Replace);
    replacement.write_batch(0, "first", &batch(&[7])).unwrap();
    let replaced = replacement.commit().unwrap();
    assert_ne!(replaced.recovery.table_id, original.recovery.table_id);
    reopened.write_batch(2, "third", &batch(&[3])).unwrap();
    kind(reopened.commit(), ErrorKind::Conflict);
    assert_eq!(rows(&fs.snapshot().unwrap()), batch(&[7]));
}

#[test]
fn concurrent_creators_and_recovery_without_loser_descriptor() {
    let fs = FaultFs::new();
    let mut a = fs.sink(WriteMode::Create);
    let mut b = fs.sink(WriteMode::Create);
    a.write_batch(0, "a", &batch(&[1])).unwrap();
    b.write_batch(0, "b", &batch(&[2])).unwrap();
    a.commit().unwrap();
    let token = b.recovery_token();
    kind(b.commit(), ErrorKind::Conflict);
    fs.inner.rm_file(&fs.candidate(&token)).unwrap();
    kind(
        recover(
            fs.clone(),
            &token,
            &SinkOptions::default(),
            RecoveryAction::Observe,
        ),
        ErrorKind::Conflict,
    );
    assert_eq!(rows(&fs.snapshot().unwrap()), batch(&[1]));
}

#[test]
fn delayed_publication_freezes_writer_until_exact_resolution() {
    let fs = FaultFs::new();
    let mut sink = fs.sink(WriteMode::Create);
    sink.write_batch(0, "batch", &batch(&[1])).unwrap();
    let token = sink.recovery_token();
    fs.inject("manifest.json", Fault::Delay);
    kind(sink.commit(), ErrorKind::UnknownOutcome);
    kind(
        sink.write_batch(1, "next", &batch(&[2])),
        ErrorKind::UnknownOutcome,
    );
    kind(sink.abort(), ErrorKind::UnknownOutcome);
    kind(sink.close(), ErrorKind::UnknownOutcome);
    kind(
        sink.resolve(RecoveryAction::Observe),
        ErrorKind::UnknownOutcome,
    );
    kind(fs.snapshot(), ErrorKind::NotFound);
    assert!(fs.deliver().into_iter().all(|r| r.is_ok()));
    assert_eq!(
        sink.resolve(RecoveryAction::Observe).unwrap().recovery,
        token
    );
    assert_eq!(sink.recovery_token().generation, 1);
    sink.write_batch(1, "next", &batch(&[2])).unwrap();
    sink.commit().unwrap();
    assert_eq!(rows(&fs.snapshot().unwrap()), batch(&[1, 2]));
}

#[test]
fn restart_resubmits_only_the_frozen_manifest() {
    let fs = FaultFs::new();
    let mut sink = fs.sink(WriteMode::Create);
    sink.write_batch(0, "batch", &batch(&[1])).unwrap();
    let token = sink.recovery_token();
    fs.inject("manifest.json", Fault::Delay);
    kind(sink.finish(), ErrorKind::UnknownOutcome);
    drop(sink);
    kind(
        recover(
            fs.clone(),
            &token,
            &SinkOptions::default(),
            RecoveryAction::Observe,
        ),
        ErrorKind::UnknownOutcome,
    );
    let result = recover(
        fs.clone(),
        &token,
        &SinkOptions::default(),
        RecoveryAction::Retry,
    )
    .unwrap();
    assert!(result.complete);
    assert_eq!(result.recovery, token);
    let writes = fs.writes.lock().unwrap();
    let manifests: Vec<_> = writes
        .iter()
        .filter(|(p, _)| p.ends_with("manifest.json"))
        .collect();
    assert_eq!(manifests.len(), 2);
    assert_eq!(manifests[0], manifests[1]);
    drop(writes);
    assert!(fs.deliver().into_iter().all(|r| r.is_err()));
    assert_eq!(rows(&fs.snapshot().unwrap()), batch(&[1]));
}

#[test]
fn lost_acknowledgement_resolves_from_stored_bytes() {
    let fs = FaultFs::new();
    let mut sink = fs.sink(WriteMode::Create);
    sink.write_batch(0, "batch", &batch(&[1])).unwrap();
    fs.inject("manifest.json", Fault::LostAck);
    assert_eq!(sink.commit().unwrap().progress, 1);
    assert_eq!(fs.snapshot().unwrap().manifest().generation, 0);
}

#[test]
fn marker_failure_is_separate_and_recovery_repairs_it() {
    let fs = FaultFs::new();
    let mut sink = fs.sink(WriteMode::Create);
    fs.inject("_SUCCESS", Fault::Deny);
    let receipt = sink.finish().unwrap();
    assert_eq!(
        receipt.marker_error.unwrap().kind,
        ErrorKind::PermissionDenied
    );
    assert!(fs.snapshot().unwrap().manifest().complete);
    let repaired = recover(
        fs.clone(),
        &receipt.recovery,
        &SinkOptions::default(),
        RecoveryAction::Observe,
    )
    .unwrap();
    assert!(repaired.marker_error.is_none());
    assert_eq!(fs.snapshot().unwrap().manifest().generation, 0);
    assert!(fs
        .info(&format!(
            "{}/manifests/00000000000000000000/_SUCCESS",
            fs.root
        ))
        .is_ok());
}

#[test]
fn ambiguous_descriptor_never_submits_a_manifest() {
    let fs = FaultFs::new();
    let mut sink = fs.sink(WriteMode::Create);
    let token = sink.recovery_token();
    fs.inject("candidate", Fault::Delay);
    kind(sink.commit(), ErrorKind::UnknownOutcome);
    assert!(fs
        .writes
        .lock()
        .unwrap()
        .iter()
        .all(|(p, _)| !p.ends_with("manifest.json")));
    sink.abort().unwrap(); // No publication request was submitted.
    assert!(fs.deliver().into_iter().all(|r| r.is_ok()));
    kind(fs.snapshot(), ErrorKind::NotFound);
    // The caller can explicitly resume the persisted attempt after a restart.
    recover(
        fs.clone(),
        &token,
        &SinkOptions::default(),
        RecoveryAction::Retry,
    )
    .unwrap();
}

#[test]
fn corrupt_descriptor_and_missing_part_prevent_recovery_publication() {
    let fs = FaultFs::new();
    let mut sink = fs.sink(WriteMode::Create);
    sink.write_batch(0, "batch", &batch(&[1])).unwrap();
    let token = sink.recovery_token();
    fs.inject("manifest.json", Fault::Delay);
    kind(sink.commit(), ErrorKind::UnknownOutcome);
    let path = fs.candidate(&token);
    let good = fs.inner.cat_file(&path, None, None).unwrap();
    let mut corrupt = good.clone();
    *corrupt.last_mut().unwrap() = b'!';
    fs.inner.pipe_file(&path, &corrupt).unwrap();
    kind(
        recover(
            fs.clone(),
            &token,
            &SinkOptions::default(),
            RecoveryAction::Retry,
        ),
        ErrorKind::Corrupt,
    );
    fs.inner.pipe_file(&path, &good).unwrap();
    let part = fs
        .writes
        .lock()
        .unwrap()
        .iter()
        .find(|(p, _)| p.ends_with(".parquet"))
        .unwrap()
        .0
        .clone();
    fs.inner.rm_file(&part).unwrap();
    kind(
        recover(
            fs.clone(),
            &token,
            &SinkOptions::default(),
            RecoveryAction::Retry,
        ),
        ErrorKind::Corrupt,
    );
    kind(fs.snapshot(), ErrorKind::NotFound);
}

#[test]
fn receipt_history_and_metadata_have_explicit_limits() {
    let fs = FaultFs::new();
    let mut sink = fs.sink(WriteMode::Create);
    for i in 0..3 {
        sink.write_batch(i, &i.to_string(), &batch(&[i as i64]))
            .unwrap();
        sink.commit().unwrap();
    }
    let mut bounded = FileDataSink::open(
        fs.clone(),
        &fs.root,
        WriteMode::Append,
        batch(&[1]).schema(),
        SinkOptions {
            max_history_reads: 1,
            ..SinkOptions::default()
        },
    )
    .unwrap();
    kind(
        bounded.write_batch(3, "new", &batch(&[4])),
        ErrorKind::Timeout,
    );
    assert_eq!(bounded.position(), 3);
    let empty_root = format!("{}/limited", fs.root);
    kind(
        FileDataSink::open(
            fs.clone(),
            empty_root,
            WriteMode::Create,
            batch(&[1]).schema(),
            SinkOptions {
                max_manifest_bytes: 1,
                ..SinkOptions::default()
            },
        ),
        ErrorKind::Invalid,
    );
}

#[test]
fn local_storage_publishes_readable_parts_and_recovers_after_reopen() {
    let directory = std::env::temp_dir().join(format!("fsspec-sink-{}", Uuid::new_v4()));
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(directory.clone());
    let fs: SharedFileSystem = Arc::new(LocalFs::new());
    let mut sink = FileDataSink::open(
        fs.clone(),
        directory.to_string_lossy(),
        WriteMode::Create,
        batch(&[1]).schema(),
        SinkOptions::default(),
    )
    .unwrap();
    sink.write_batch(0, "local", &batch(&[1, 2, 3])).unwrap();
    let receipt = sink.finish().unwrap();
    drop(sink);
    assert_eq!(
        recover(
            fs.clone(),
            &receipt.recovery,
            &SinkOptions::default(),
            RecoveryAction::Observe
        )
        .unwrap()
        .sha256,
        receipt.sha256
    );
    let snapshot = ManifestReader::new(fs, directory.to_string_lossy(), ReadLimits::default())
        .unwrap()
        .read(Selection::Latest)
        .unwrap();
    assert_eq!(rows(&snapshot), batch(&[1, 2, 3]));
}

#[test]
fn delayed_loser_cannot_publish_after_another_creator_wins() {
    let fs = FaultFs::new();
    let mut a = fs.sink(WriteMode::Create);
    let mut b = fs.sink(WriteMode::Create);
    a.write_batch(0, "a", &batch(&[1])).unwrap();
    b.write_batch(0, "b", &batch(&[2])).unwrap();
    let token = a.recovery_token();
    fs.inject("manifest.json", Fault::Delay);
    kind(a.commit(), ErrorKind::UnknownOutcome);
    b.commit().unwrap();
    fs.inner.rm_file(&fs.candidate(&token)).unwrap();
    kind(a.resolve(RecoveryAction::Observe), ErrorKind::Conflict);
    kind(
        recover(
            fs.clone(),
            &token,
            &SinkOptions::default(),
            RecoveryAction::Retry,
        ),
        ErrorKind::Conflict,
    );
    assert!(fs.deliver().into_iter().all(|r| r.is_err()));
    a.abort().unwrap();
    assert_eq!(rows(&fs.snapshot().unwrap()), batch(&[2]));
}

#[test]
fn staging_failure_and_metadata_rejection_never_publish() {
    let fs = FaultFs::new();
    let mut limited = FileDataSink::open(
        fs.clone(),
        &fs.root,
        WriteMode::Create,
        batch(&[1]).schema(),
        SinkOptions {
            max_manifest_bytes: 1500,
            ..SinkOptions::default()
        },
    )
    .unwrap();
    kind(
        limited.write_batch(0, "batch", &batch(&[1])),
        ErrorKind::Invalid,
    );
    assert_eq!(limited.position(), 0);
    assert!(fs.writes.lock().unwrap().is_empty());
    // Rejection happened before encoder mutation, so publishing an empty table works.
    limited.commit().unwrap();
    let mut append = fs.sink(WriteMode::Append);
    append.write_batch(0, "batch", &batch(&[1])).unwrap();
    fs.inject(".parquet", Fault::Deny);
    kind(append.flush(), ErrorKind::PermissionDenied);
    kind(append.commit(), ErrorKind::Invalid);
    append.abort().unwrap();
    assert_eq!(fs.snapshot().unwrap().manifest().progress, 0);
}

#[test]
fn stores_without_conditional_create_are_rejected_before_mutation() {
    let fs = FaultFs::new();
    fs.conditional.store(false, Ordering::Relaxed);
    kind(
        FileDataSink::open(
            fs.clone(),
            &fs.root,
            WriteMode::Create,
            batch(&[1]).schema(),
            SinkOptions::default(),
        ),
        ErrorKind::NotSupported,
    );
    assert!(fs.writes.lock().unwrap().is_empty());
}
