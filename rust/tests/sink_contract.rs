use std::sync::Arc;

use arrow::array::{Int64Array, RecordBatch};
use fsspec_data::manifest::{ManifestReader, ReadLimits, Selection};
use fsspec_data::sink::{
    PublicationReceipt, RecoveryAction, RecoveryToken, SinkOptions, WriteMode,
};
use fsspec_data::{DataSink, DataWriter, ManifestTableSink, SharedFileSystem};
use fsspec_rs::{ErrorKind, MemoryFs, OperationError};

fn batch(start: i64) -> RecordBatch {
    RecordBatch::try_from_iter([(
        "value",
        Arc::new(Int64Array::from(vec![start, start + 1])) as _,
    )])
    .unwrap()
}

fn write_and_commit<S: DataSink>(
    sink: &S,
    options: S::Options,
    batch: &RecordBatch,
) -> Result<<S::Writer as DataWriter>::PublicationReceipt, OperationError> {
    let mut writer = sink.open_writer(batch.schema(), WriteMode::Create, options)?;
    assert_eq!(writer.schema(), batch.schema());
    assert_eq!(writer.progress_unit(), "rows");
    writer.write_batch(0, "first", batch)?;
    writer.flush()?;
    writer.commit()
}

fn setup() -> (SharedFileSystem, String, ManifestTableSink) {
    let fs: SharedFileSystem = Arc::new(MemoryFs::new());
    let root = format!("/{}", uuid::Uuid::new_v4());
    let sink = ManifestTableSink::new(fs.clone(), &root);
    (fs, root, sink)
}

#[test]
fn generic_sink_preserves_publication_receipt_and_saved_data() {
    let (fs, root, sink) = setup();
    let receipt: PublicationReceipt =
        write_and_commit(&sink, SinkOptions::default(), &batch(0)).unwrap();
    assert_eq!(receipt.progress, 2);
    assert!(!receipt.complete);
    assert_eq!(receipt.recovery.root, root);
    let snapshot = ManifestReader::new(fs, root, ReadLimits::default())
        .unwrap()
        .read(Selection::Latest)
        .unwrap();
    assert_eq!(snapshot.manifest().num_rows, 2);
    assert_eq!(snapshot.checksum(), receipt.sha256);
}

fn exercise_writer<W: DataWriter>(writer: &mut W) {
    assert_eq!(writer.position(), 0);
    writer.write_batch(0, "first", &batch(0)).unwrap();
    writer.write_batch(0, "first", &batch(0)).unwrap();
    assert_eq!(writer.position(), 2);
    assert_eq!(
        writer
            .write_batch(3, "gap", &batch(2))
            .err()
            .unwrap()
            .kind(),
        ErrorKind::Gap
    );
    assert_eq!(
        writer
            .write_batch(0, "first", &batch(3))
            .err()
            .unwrap()
            .kind(),
        ErrorKind::Conflict
    );
    assert_eq!(
        writer
            .write_batch(0, "other", &batch(0))
            .err()
            .unwrap()
            .kind(),
        ErrorKind::Conflict
    );
    let _token = writer.recovery_token();
    writer.commit().unwrap();
    assert_eq!(
        writer
            .resolve(RecoveryAction::Observe)
            .err()
            .unwrap()
            .kind(),
        ErrorKind::Invalid
    );
    writer.write_batch(2, "second", &batch(2)).unwrap();
    writer.close().unwrap();
}

#[test]
fn trait_calls_preserve_retry_keys_and_discard_only_pending_work() {
    let (fs, root, sink) = setup();
    let mut writer = sink
        .open_writer(batch(0).schema(), WriteMode::Create, SinkOptions::default())
        .unwrap();
    exercise_writer(&mut writer);
    let reader = ManifestReader::new(fs, root, ReadLimits::default()).unwrap();
    let snapshot = reader.read(Selection::Latest).unwrap();
    assert_eq!(snapshot.manifest().num_rows, 2);
    assert!(!snapshot.manifest().complete);
    let mut reopened = sink
        .open_writer(batch(0).schema(), WriteMode::Append, SinkOptions::default())
        .unwrap();
    let generic: &mut dyn DataWriter<
        BatchReceipt = fsspec_data::manifest::BatchReceipt,
        PublicationReceipt = PublicationReceipt,
        RecoveryToken = RecoveryToken,
    > = &mut reopened;
    generic.write_batch(0, "first", &batch(0)).unwrap();
    generic.write_batch(2, "second", &batch(2)).unwrap();
    assert_eq!(generic.finish().unwrap().progress, 4);
    assert!(reader.read(Selection::Latest).unwrap().manifest().complete);
}

#[test]
fn abort_drop_and_replace_use_the_bound_filesystem() {
    let (fs, root, sink) = setup();
    let reader = ManifestReader::new(fs, root, ReadLimits::default()).unwrap();
    for abort in [false, true] {
        let mut writer = sink
            .open_writer(batch(0).schema(), WriteMode::Create, SinkOptions::default())
            .unwrap();
        DataWriter::write_batch(&mut writer, 0, "unpublished", &batch(0)).unwrap();
        DataWriter::flush(&mut writer).unwrap();
        if abort {
            DataWriter::abort(&mut writer).unwrap();
        }
        drop(writer);
        assert_eq!(
            reader.read(Selection::Latest).err().unwrap().kind(),
            ErrorKind::NotFound
        );
    }
    write_and_commit(&sink, SinkOptions::default(), &batch(0)).unwrap();
    let saved = reader.read(Selection::Latest).unwrap();
    let mut replacement = sink
        .open_writer(
            batch(5).schema(),
            WriteMode::Replace,
            SinkOptions::default(),
        )
        .unwrap();
    DataWriter::write_batch(&mut replacement, 0, "replace", &batch(5)).unwrap();
    DataWriter::finish(&mut replacement).unwrap();
    assert_ne!(
        reader.read(Selection::Latest).unwrap().manifest().table_id,
        saved.manifest().table_id
    );
    assert!(saved.read_part(0).is_ok());
}
