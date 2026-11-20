use std::collections::{BTreeMap, HashMap};
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use arrow::array::{Int64Array, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use bytes::Bytes;
use fsspec_data::manifest::*;
use fsspec_rs::{
    ErrorKind, FileInfo, FileSystem, FsError, FsFile, FsResult, LocalFs, OpenMode, OpenOptions,
    OperationError,
};
use parquet::arrow::{arrow_reader::ParquetRecordBatchReaderBuilder, ArrowWriter};

fn path(g: u64) -> String {
    format!("table/manifests/{g:020}/manifest.json")
}
fn schema() -> Schema {
    Schema::new(vec![Field::new("value", DataType::Int64, false)])
}
fn part(g: u64, start: u64, data: &[u8]) -> Part {
    Part {
        key: format!("parts/{g:020}/pub-{g}/data.parquet"),
        size: data.len() as u64,
        sha256: Checksum::of(data),
        num_rows: 1,
        progress: Span {
            start,
            end: start + 1,
        },
        encoding: "parquet".into(),
        options: BTreeMap::new(),
    }
}
fn create() -> Manifest {
    Manifest {
        format_version: 1,
        namespace_id: "namespace".into(),
        table_id: "table-a".into(),
        generation: 0,
        parent: None,
        publication_id: "pub-0".into(),
        kind: PublicationKind::Create,
        schema: schema(),
        partition_values: BTreeMap::from([("day".into(), "2026-11-20".into())]),
        complete: false,
        deleted: false,
        num_rows: 1,
        progress_unit: "rows".into(),
        progress: 1,
        parts: vec![part(0, 0, b"first")],
        receipts: vec![receipt(0)],
    }
}
fn receipt(start: u64) -> BatchReceipt {
    BatchReceipt {
        key: format!("batch-{start}"),
        progress: Span {
            start,
            end: start + 1,
        },
        fingerprint: Checksum::of(&start.to_le_bytes()),
    }
}
fn append(previous: &Manifest, previous_bytes: &[u8]) -> Manifest {
    let mut m = previous.clone();
    m.generation += 1;
    m.parent = Some(Parent {
        generation: previous.generation,
        sha256: Checksum::of(previous_bytes),
    });
    m.publication_id = format!("pub-{}", m.generation);
    m.kind = PublicationKind::Append;
    m.parts.push(part(m.generation, previous.progress, b"next"));
    m.receipts = vec![receipt(previous.progress)];
    m.num_rows += 1;
    m.progress += 1;
    m
}
fn assert_kind<T>(result: Result<T, OperationError>, kind: ErrorKind) {
    match result {
        Err(e) => assert_eq!(e.kind(), kind, "{e}"),
        Ok(_) => panic!("expected {kind:?}"),
    }
}

#[derive(Default)]
struct TestFs {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    opened: Mutex<Vec<String>>,
    bytes: Arc<AtomicUsize>,
    errors: Mutex<BTreeMap<String, FsError>>,
    after_missing: Mutex<Option<(String, Vec<u8>)>>,
}
impl TestFs {
    fn put(&self, m: &Manifest) -> Vec<u8> {
        let bytes = m.encode().unwrap();
        self.objects
            .lock()
            .unwrap()
            .insert(path(m.generation), bytes.clone());
        bytes
    }
    fn read(self: &Arc<Self>, selection: Selection) -> Result<Snapshot, OperationError> {
        ManifestReader::new(self.clone(), "table", ReadLimits::default())
            .unwrap()
            .read(selection)
    }
}
struct CountedFile {
    cursor: Cursor<Vec<u8>>,
    bytes: Arc<AtomicUsize>,
}
impl Read for CountedFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.cursor.read(buf)?;
        self.bytes.fetch_add(n, Ordering::Relaxed);
        Ok(n)
    }
}
impl Write for CountedFile {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        panic!("reader attempted mutation")
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Seek for CountedFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.cursor.seek(pos)
    }
}
impl FsFile for CountedFile {
    fn info(&self) -> FsResult<FileInfo> {
        panic!("unexpected metadata request")
    }
    fn size(&self) -> FsResult<Option<u64>> {
        Ok(None)
    }
}
impl FileSystem for TestFs {
    fn protocol(&self) -> &[&str] {
        &["test"]
    }
    fn ls(&self, _: &str, _: bool) -> FsResult<Vec<FileInfo>> {
        panic!("reader must not list")
    }
    fn rm_file(&self, _: &str) -> FsResult<()> {
        panic!("reader attempted mutation")
    }
    fn cp_file(&self, _: &str, _: &str) -> FsResult<()> {
        panic!("reader attempted mutation")
    }
    fn info(&self, _: &str) -> FsResult<FileInfo> {
        panic!("reader must not rely on path metadata")
    }
    fn mkdir(&self, _: &str, _: bool) -> FsResult<()> {
        panic!("reader attempted mutation")
    }
    fn rmdir(&self, _: &str) -> FsResult<()> {
        panic!("reader attempted mutation")
    }
    fn open(
        &self,
        name: &str,
        mode: OpenMode,
        _: Option<OpenOptions>,
    ) -> FsResult<Box<dyn FsFile>> {
        assert_eq!(mode, OpenMode::Read);
        self.opened.lock().unwrap().push(name.into());
        if let Some(error) = self.errors.lock().unwrap().remove(name) {
            return Err(error);
        }
        let bytes = self.objects.lock().unwrap().get(name).cloned();
        if bytes.is_none() {
            let mut pending = self.after_missing.lock().unwrap();
            if pending.as_ref().is_some_and(|p| p.0 == name) {
                let (key, value) = pending.take().unwrap();
                self.objects.lock().unwrap().insert(key, value);
            }
        }
        let bytes = bytes.ok_or_else(|| FsError::NotFound(name.into()))?;
        Ok(Box::new(CountedFile {
            cursor: Cursor::new(bytes),
            bytes: self.bytes.clone(),
        }))
    }
}

#[test]
fn wire_round_trip_preserves_schema_metadata_options_and_receipts() {
    let mut m = create();
    m.schema = Schema::new_with_metadata(
        vec![
            Field::new(
                "nested",
                DataType::List(Arc::new(Field::new(
                    "item",
                    DataType::Decimal128(20, 4),
                    true,
                ))),
                true,
            ),
            Field::new(
                "time",
                DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
                false,
            )
            .with_metadata(HashMap::from([("meaning".into(), "event time".into())])),
        ],
        HashMap::from([("source".into(), "sensor".into())]),
    );
    m.parts[0]
        .options
        .insert("compression".into(), "zstd".into());
    let bytes = m.encode().unwrap();
    assert_eq!(Manifest::decode(&bytes).unwrap(), m);
    assert_eq!(
        Checksum::of(b"abc").to_string(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json["parts"][0]["sha256"],
        Checksum::of(b"first").to_string()
    );
    assert_eq!(json["schema"]["fields"][1]["name"], "time");
}

#[test]
fn unsupported_and_malformed_documents_are_distinct_errors() {
    assert_kind(
        Manifest::decode(br#"{"format_version":2,"new_format":true}"#),
        ErrorKind::NotSupported,
    );
    for bytes in [
        b"".as_slice(),
        b"{",
        br#"{"format_version":1,"format_version":1}"#,
    ] {
        assert_kind(Manifest::decode(bytes), ErrorKind::Corrupt);
    }
    let mut json = serde_json::to_value(create()).unwrap();
    json["unexpected"] = true.into();
    assert_kind(
        Manifest::decode(&serde_json::to_vec(&json).unwrap()),
        ErrorKind::Corrupt,
    );
    json.as_object_mut().unwrap().remove("unexpected");
    json["parts"][0]["sha256"] = "not-a-checksum".into();
    assert_kind(
        Manifest::decode(&serde_json::to_vec(&json).unwrap()),
        ErrorKind::Corrupt,
    );
    assert_kind(
        Manifest::decode(&vec![b' '; MAX_MANIFEST_BYTES + 1]),
        ErrorKind::Invalid,
    );
}

#[test]
fn unsafe_part_keys_and_wrong_publication_ownership_are_rejected() {
    for key in [
        "../secret",
        "/absolute",
        "parts/00000000000000000000/pub-0/../secret",
        "parts/00000000000000000000/pub-0/%2e%2e",
        "parts/00000000000000000000/pub-0/a?x",
        "parts/00000000000000000000/pub-0/a\\b",
        "parts/00000000000000000000/pub-0/C:secret",
        "parts/00000000000000000000/pub-0/..",
        "parts/00000000000000000001/pub-0/data",
        "parts/00000000000000000000/other/data",
        "parts/0/pub-0/data",
    ] {
        let mut m = create();
        m.parts[0].key = key.into();
        assert_kind(m.validate(), ErrorKind::Corrupt);
    }
}

#[test]
fn manifest_validates_progress_totals_and_receipt_coverage() {
    let mut m = create();
    m.num_rows += 1;
    assert_kind(m.validate(), ErrorKind::Corrupt);
    m = create();
    m.parts[0].progress.start = 1;
    assert_kind(m.validate(), ErrorKind::Corrupt);
    m = create();
    m.receipts.clear();
    assert_kind(m.validate(), ErrorKind::Corrupt);
    m = create();
    m.receipts[0].progress.end = 2;
    assert_kind(m.validate(), ErrorKind::Corrupt);
    m = create();
    m.parts.push(m.parts[0].clone());
    assert_kind(m.validate(), ErrorKind::Corrupt);
    m = create();
    m.parts[0].encoding = "unknown".into();
    assert_kind(m.validate(), ErrorKind::NotSupported);
    m = create();
    m.progress_unit = "events".into();
    m.parts[0].num_rows = 40;
    m.num_rows = 40;
    m.validate().unwrap(); // Event progress is independent of output row count.
}

#[test]
fn latest_and_pinned_snapshots_use_only_manifest_keys() {
    let fs = Arc::new(TestFs::default());
    let first = create();
    let bytes = fs.put(&first);
    let second = append(&first, &bytes);
    fs.put(&second);
    fs.objects
        .lock()
        .unwrap()
        .insert("table/parts/orphan".into(), vec![0]);
    let pinned = fs.read(Selection::Generation(0)).unwrap();
    assert_eq!(pinned.manifest(), &first);
    assert_eq!(pinned.checksum(), Checksum::of(&bytes));
    let latest = fs.read(Selection::Latest).unwrap();
    assert_eq!(latest.manifest(), &second);
    assert!(fs
        .opened
        .lock()
        .unwrap()
        .iter()
        .all(|p| p.contains("/manifests/")));
    // Opening metadata does not claim that parts have been checked.
    assert_kind(latest.read_part(0), ErrorKind::Corrupt);
    assert_kind(latest.read_part(99), ErrorKind::Invalid);
    assert_kind(fs.read(Selection::Generation(2)), ErrorKind::NotFound);
}

#[test]
fn latest_rechecks_absence_when_publication_races_discovery() {
    let fs = Arc::new(TestFs::default());
    let first = create();
    let bytes = fs.put(&first);
    let second = append(&first, &bytes);
    *fs.after_missing.lock().unwrap() = Some((path(1), second.encode().unwrap()));
    assert_eq!(fs.read(Selection::Latest).unwrap().manifest(), &second);
}

#[test]
fn discovery_is_logarithmic_and_has_a_request_budget() {
    let fs = Arc::new(TestFs::default());
    let mut m = create();
    let mut bytes = fs.put(&m);
    for _ in 0..128 {
        m = append(&m, &bytes);
        bytes = fs.put(&m);
    }
    assert_eq!(
        fs.read(Selection::Latest).unwrap().manifest().generation,
        128
    );
    assert!(fs.opened.lock().unwrap().len() < 40);
    let reader = ManifestReader::new(
        fs,
        "table",
        ReadLimits {
            max_manifest_reads: 2,
            ..ReadLimits::default()
        },
    )
    .unwrap();
    assert_kind(reader.read(Selection::Latest), ErrorKind::Timeout);
}

#[test]
fn permission_and_transport_failures_never_look_like_missing_generations() {
    let fs = Arc::new(TestFs::default());
    fs.put(&create());
    for (error, kind) in [
        (
            FsError::PermissionDenied("denied".into()),
            ErrorKind::PermissionDenied,
        ),
        (
            FsError::IoError(io::Error::new(io::ErrorKind::TimedOut, "timeout")),
            ErrorKind::Timeout,
        ),
        (
            FsError::IoError(io::Error::new(io::ErrorKind::ConnectionReset, "reset")),
            ErrorKind::Unavailable,
        ),
    ] {
        fs.errors.lock().unwrap().insert(path(1), error);
        assert_kind(fs.read(Selection::Latest), kind);
    }
}

#[test]
fn selected_generation_checks_parent_bytes_namespace_and_state() {
    let fs = Arc::new(TestFs::default());
    let first = create();
    let bytes = fs.put(&first);
    let second = append(&first, &bytes);
    fs.put(&second);
    // Equivalent JSON with changed whitespace is still a different stored object.
    fs.objects
        .lock()
        .unwrap()
        .insert(path(0), [bytes.as_slice(), b"\n"].concat());
    assert_kind(fs.read(Selection::Generation(1)), ErrorKind::Corrupt);
    fs.put(&first);
    let mut broken = second.clone();
    broken.namespace_id = "another".into();
    fs.put(&broken);
    assert_kind(fs.read(Selection::Generation(1)), ErrorKind::Corrupt);
    broken = second.clone();
    broken.table_id = "another".into();
    fs.put(&broken);
    assert_kind(fs.read(Selection::Latest), ErrorKind::Corrupt);
    broken = second.clone();
    broken.parts[0].key = "parts/00000000000000000000/orphan/data".into();
    fs.put(&broken);
    assert_kind(fs.read(Selection::Generation(1)), ErrorKind::Corrupt);
    fs.put(&second);
    fs.objects.lock().unwrap().remove(&path(0));
    assert_kind(fs.read(Selection::Generation(1)), ErrorKind::Corrupt);
}

#[test]
fn missing_parents_and_misplaced_generations_are_corrupt() {
    let fs = Arc::new(TestFs::default());
    let first = create();
    let bytes = fs.put(&first);
    let second = append(&first, &bytes);
    let third = append(&second, &second.encode().unwrap());
    fs.put(&third);
    assert_kind(fs.read(Selection::Generation(2)), ErrorKind::Corrupt);
    fs.objects
        .lock()
        .unwrap()
        .insert(path(1), first.encode().unwrap());
    assert_kind(fs.read(Selection::Latest), ErrorKind::Corrupt);
}

#[test]
fn completion_cleanup_and_recreation_validate_parent_state() {
    let fs = Arc::new(TestFs::default());
    let first = create();
    let bytes = fs.put(&first);
    let mut finished = append(&first, &bytes);
    finished.kind = PublicationKind::Finish;
    finished.complete = true;
    let bytes = fs.put(&finished);
    assert!(fs.read(Selection::Latest).unwrap().manifest().complete);

    let mut reopened = append(&finished, &bytes);
    reopened.complete = false;
    fs.put(&reopened);
    assert_kind(fs.read(Selection::Generation(2)), ErrorKind::Corrupt);

    let mut cleanup = finished.clone();
    cleanup.generation = 2;
    cleanup.publication_id = "pub-2".into();
    cleanup.parent = Some(Parent {
        generation: 1,
        sha256: Checksum::of(&bytes),
    });
    cleanup.kind = PublicationKind::Cleanup;
    cleanup.receipts.clear();
    let bytes = fs.put(&cleanup);
    assert!(fs.read(Selection::Latest).unwrap().manifest().complete);
    let mut changed = cleanup.clone();
    changed.schema = Schema::empty();
    fs.put(&changed);
    assert_kind(fs.read(Selection::Generation(2)), ErrorKind::Corrupt);
    fs.put(&cleanup);

    let mut deleted = cleanup.clone();
    deleted.generation = 3;
    deleted.publication_id = "pub-3".into();
    deleted.parent = Some(Parent {
        generation: 2,
        sha256: Checksum::of(&bytes),
    });
    deleted.kind = PublicationKind::Delete;
    deleted.table_id = "tombstone".into();
    deleted.complete = false;
    deleted.deleted = true;
    deleted.parts.clear();
    deleted.num_rows = 0;
    deleted.progress = 0;
    let bytes = fs.put(&deleted);

    let mut recreated = create();
    recreated.generation = 4;
    recreated.publication_id = "pub-4".into();
    recreated.table_id = "table-b".into();
    recreated.parent = Some(Parent {
        generation: 3,
        sha256: Checksum::of(&bytes),
    });
    recreated.parts = vec![part(4, 0, b"new")];
    fs.put(&recreated);
    assert_eq!(
        fs.read(Selection::Latest).unwrap().manifest().table_id,
        "table-b"
    );
    recreated.kind = PublicationKind::Replace;
    fs.put(&recreated);
    assert_kind(fs.read(Selection::Generation(4)), ErrorKind::Corrupt);
}

#[test]
fn append_receipts_must_cover_only_new_progress() {
    let fs = Arc::new(TestFs::default());
    let first = create();
    let bytes = fs.put(&first);
    let mut second = append(&first, &bytes);
    second.receipts.clear();
    fs.put(&second);
    assert_kind(fs.read(Selection::Generation(1)), ErrorKind::Corrupt);
    second.receipts = vec![receipt(0), receipt(1)];
    fs.put(&second);
    assert_kind(fs.read(Selection::Generation(1)), ErrorKind::Corrupt);
}

#[test]
fn full_part_reads_detect_missing_short_long_and_corrupt_content() {
    let fs = Arc::new(TestFs::default());
    let m = create();
    fs.put(&m);
    let snapshot = fs.read(Selection::Latest).unwrap();
    let key = format!("table/{}", m.parts[0].key);
    assert_kind(snapshot.read_part(0), ErrorKind::Corrupt);
    for bad in [b"firs".as_slice(), b"first-extra", b"other"] {
        fs.objects.lock().unwrap().insert(key.clone(), bad.to_vec());
        assert_kind(snapshot.read_part(0), ErrorKind::Corrupt);
    }
    fs.objects
        .lock()
        .unwrap()
        .insert(key.clone(), b"first".to_vec());
    assert_eq!(snapshot.read_part(0).unwrap(), b"first");
    fs.errors
        .lock()
        .unwrap()
        .insert(key, FsError::PermissionDenied("denied".into()));
    assert_kind(snapshot.read_part(0), ErrorKind::PermissionDenied);
}

#[test]
fn limits_apply_to_unknown_length_files_before_unbounded_reads() {
    let fs = Arc::new(TestFs::default());
    fs.objects
        .lock()
        .unwrap()
        .insert(path(0), vec![b' '; 100_000]);
    let reader = ManifestReader::new(
        fs.clone(),
        "table",
        ReadLimits {
            max_manifest_bytes: 32,
            ..ReadLimits::default()
        },
    )
    .unwrap();
    assert_kind(reader.read(Selection::Generation(0)), ErrorKind::Invalid);
    assert_eq!(fs.bytes.load(Ordering::Relaxed), 33);
    fs.put(&create());
    let reader = ManifestReader::new(
        fs.clone(),
        "table",
        ReadLimits {
            max_part_bytes: 4,
            ..ReadLimits::default()
        },
    )
    .unwrap();
    let snapshot = reader.read(Selection::Latest).unwrap();
    let before = fs.opened.lock().unwrap().len();
    assert_kind(snapshot.read_part(0), ErrorKind::Invalid);
    assert_eq!(fs.opened.lock().unwrap().len(), before);
    assert_kind(
        ManifestReader::new(
            fs,
            "table",
            ReadLimits {
                max_manifest_reads: 0,
                ..ReadLimits::default()
            },
        ),
        ErrorKind::Invalid,
    );
}

#[test]
fn replacements_and_tombstones_leave_pinned_snapshots_unchanged() {
    let fs = Arc::new(TestFs::default());
    let first = create();
    let bytes = fs.put(&first);
    fs.objects
        .lock()
        .unwrap()
        .insert(format!("table/{}", first.parts[0].key), b"first".to_vec());
    let pinned = fs.read(Selection::Latest).unwrap();
    let mut replacement = append(&first, &bytes);
    replacement.kind = PublicationKind::Replace;
    replacement.table_id = "table-b".into();
    replacement.parts.remove(0);
    replacement.parts[0].progress = Span { start: 0, end: 1 };
    replacement.receipts = vec![receipt(0)];
    replacement.progress = 1;
    replacement.num_rows = 1;
    let bytes = fs.put(&replacement);
    assert_eq!(
        fs.read(Selection::Latest).unwrap().manifest().table_id,
        "table-b"
    );
    assert_eq!(pinned.read_part(0).unwrap(), b"first");
    let mut deleted = replacement.clone();
    deleted.generation = 2;
    deleted.publication_id = "pub-2".into();
    deleted.table_id = "deleted".into();
    deleted.kind = PublicationKind::Delete;
    deleted.deleted = true;
    deleted.parent = Some(Parent {
        generation: 1,
        sha256: Checksum::of(&bytes),
    });
    deleted.parts.clear();
    deleted.receipts.clear();
    deleted.progress = 0;
    deleted.num_rows = 0;
    fs.put(&deleted);
    assert!(fs.read(Selection::Latest).unwrap().manifest().deleted);
    assert_eq!(pinned.read_part(0).unwrap(), b"first");
}

#[test]
fn independent_readers_do_not_share_source_state() {
    let a = Arc::new(TestFs::default());
    let b = Arc::new(TestFs::default());
    let first = create();
    a.put(&first);
    let mut other = first.clone();
    other.namespace_id = "other".into();
    b.put(&other);
    assert_eq!(
        a.read(Selection::Latest).unwrap().manifest().namespace_id,
        "namespace"
    );
    assert_eq!(
        b.read(Selection::Latest).unwrap().manifest().namespace_id,
        "other"
    );
}

#[test]
fn local_snapshot_reads_a_real_parquet_part() {
    let batch = RecordBatch::try_new(
        Arc::new(schema()),
        vec![Arc::new(Int64Array::from(vec![42]))],
    )
    .unwrap();
    let mut encoded = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut encoded, batch.schema(), None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
    let directory =
        std::env::temp_dir().join(format!("fsspec-manifest-reader-{}", std::process::id()));
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    std::fs::create_dir(&directory).unwrap();
    let _cleanup = Cleanup(directory.clone());
    let mut manifest = create();
    manifest.parts[0] = part(0, 0, &encoded);
    let part_path = directory.join(&manifest.parts[0].key);
    std::fs::create_dir_all(part_path.parent().unwrap()).unwrap();
    std::fs::write(part_path, &encoded).unwrap();
    let manifest_path = directory.join("manifests/00000000000000000000/manifest.json");
    std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    std::fs::write(manifest_path, manifest.encode().unwrap()).unwrap();
    let reader = ManifestReader::new(
        Arc::new(LocalFs::new()),
        directory.to_string_lossy(),
        ReadLimits::default(),
    )
    .unwrap();
    let bytes = reader
        .read(Selection::Latest)
        .unwrap()
        .read_part(0)
        .unwrap();
    let mut batches = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes))
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(batches.next().unwrap().unwrap(), batch);
    assert!(batches.next().is_none());
}
