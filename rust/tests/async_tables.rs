use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::array::{Int64Array, RecordBatch};
use fsspec_data::async_tables::{AsyncManifestWatcher, AsyncTableOptions};
use fsspec_data::manifest::{ManifestReader, ReadLimits, Selection};
use fsspec_data::sink::{FileDataSink, SinkOptions, WriteMode};
use fsspec_data::watch::{WatchEnd, WatchOptions};
use fsspec_data::{CancellationToken, ScanSelection, SharedFileSystem, StreamOptions};
use fsspec_rs::MemoryFs;

fn batch(start: i64) -> RecordBatch {
    RecordBatch::try_from_iter([(
        "value",
        Arc::new(Int64Array::from(vec![start, start + 1, start + 2])) as _,
    )])
    .unwrap()
}
fn setup() -> (SharedFileSystem, String, FileDataSink, ManifestReader) {
    let fs: SharedFileSystem = Arc::new(MemoryFs::new());
    let root = format!("/{}", uuid::Uuid::new_v4());
    let mut sink = FileDataSink::open(
        fs.clone(),
        &root,
        WriteMode::Create,
        batch(0).schema(),
        SinkOptions::default(),
    )
    .unwrap();
    sink.write_batch(0, "first", &batch(0)).unwrap();
    sink.commit().unwrap();
    let reader = ManifestReader::new(fs.clone(), &root, ReadLimits::default()).unwrap();
    (fs, root, sink, reader)
}
#[tokio::test]
async fn snapshot_scan_limits_and_saved_versions() {
    let (_fs, _root, mut sink, reader) = setup();
    let snapshot = reader
        .read_async(
            Selection::Latest,
            AsyncTableOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    sink.write_batch(3, "second", &batch(3)).unwrap();
    sink.finish().unwrap();
    assert_eq!(snapshot.manifest().generation, 0);
    let mut scan = snapshot
        .scan_async(
            ScanSelection {
                columns: Some(vec!["value".into()]),
                row_offset: 1,
            },
            StreamOptions {
                batch_size: 1,
                row_limit: Some(1),
                byte_limit: None,
            },
            AsyncTableOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        scan.next()
            .await
            .unwrap()
            .unwrap()
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap()
            .value(0),
        1
    );
    assert!(scan.next().await.unwrap().is_none());
    scan.close().await.unwrap();
    scan.close().await.unwrap();
    let mut limited = snapshot
        .scan_async(
            ScanSelection::default(),
            StreamOptions::default(),
            AsyncTableOptions {
                max_batch_bytes: 1,
                ..Default::default()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(limited.next().await.is_err());
    assert!(limited.next().await.unwrap().is_none());
    limited.close().await.unwrap();
}
#[tokio::test]
async fn watch_resume_finish_replace_and_checkpoint_on_failure() {
    let (fs, root, mut sink, reader) = setup();
    let mut watch = AsyncManifestWatcher::new(
        reader.clone(),
        root.clone(),
        WatchOptions {
            batch_size: 2,
            ..Default::default()
        },
        AsyncTableOptions::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let first = watch.next().await.unwrap().unwrap();
    assert_eq!(first.position.progress, 2);
    watch.close().await.unwrap();
    sink.write_batch(3, "second", &batch(3)).unwrap();
    sink.finish().unwrap();
    let mut resumed = AsyncManifestWatcher::new(
        reader.clone(),
        root.clone(),
        WatchOptions {
            start: Some(first.position.clone()),
            ..Default::default()
        },
        AsyncTableOptions::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let mut rows = 0;
    while let Some(item) = resumed.next().await.unwrap() {
        rows += item.batch.num_rows();
    }
    assert_eq!(rows, 4);
    assert_eq!(resumed.position().progress, 6);
    assert_eq!(resumed.end_reason(), Some(WatchEnd::Complete));
    resumed.close().await.unwrap();
    let mut fail = AsyncManifestWatcher::new(
        reader.clone(),
        root.clone(),
        WatchOptions {
            start: Some(first.position.clone()),
            ..Default::default()
        },
        AsyncTableOptions {
            max_batch_bytes: 1,
            ..Default::default()
        },
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(fail.next().await.is_err());
    assert_eq!(*fail.position(), first.position);
    assert_eq!(fail.end_reason(), None);
    fail.close().await.unwrap();
    let mut replacement = FileDataSink::open(
        fs,
        &root,
        WriteMode::Replace,
        batch(0).schema(),
        SinkOptions::default(),
    )
    .unwrap();
    replacement.write_batch(0, "new", &batch(10)).unwrap();
    replacement.commit().unwrap();
    let mut current = AsyncManifestWatcher::new(
        reader,
        root,
        WatchOptions {
            idle_timeout: Some(Duration::ZERO),
            ..Default::default()
        },
        AsyncTableOptions::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(current.next().await.unwrap().unwrap().position.progress, 3);
    assert!(current.next().await.unwrap().is_none());
    assert_eq!(current.end_reason(), Some(WatchEnd::IdleTimeout));
    current.close().await.unwrap();
}
#[tokio::test]
async fn cancellation_and_deadline_wake_long_poll_without_advancing_position() {
    let (_fs, root, _sink, reader) = setup();
    for deadline in [false, true] {
        let cancel = CancellationToken::new();
        let mut watch = AsyncManifestWatcher::new(
            reader.clone(),
            root.clone(),
            WatchOptions {
                poll_interval: Duration::from_secs(60),
                ..Default::default()
            },
            AsyncTableOptions {
                deadline: deadline.then(|| Instant::now() + Duration::from_millis(100)),
                ..Default::default()
            },
            cancel.clone(),
        )
        .await
        .unwrap();
        watch.next().await.unwrap().unwrap();
        let saved = watch.position().clone();
        let task = async {
            if !deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
                cancel.cancel();
            }
        };
        let (result, ()) = tokio::join!(watch.next(), task);
        if deadline {
            assert!(result.is_err());
        } else {
            assert!(result.unwrap().is_none());
            assert_eq!(watch.end_reason(), Some(WatchEnd::Cancelled));
        }
        assert_eq!(*watch.position(), saved);
        tokio::time::timeout(Duration::from_secs(1), watch.close())
            .await
            .unwrap()
            .unwrap();
    }
}
#[tokio::test]
async fn skipped_corrupt_part_is_not_read_and_required_part_is_verified() {
    let (fs, root, mut sink, reader) = setup();
    sink.write_batch(3, "second", &batch(3)).unwrap();
    sink.finish().unwrap();
    let snapshot = reader
        .read_async(
            Selection::Latest,
            AsyncTableOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let part = &snapshot.manifest().parts[0];
    fs.pipe_file(&format!("{root}/{}", part.key), b"corrupt")
        .unwrap();
    let mut skipped = snapshot
        .scan_async(
            ScanSelection {
                row_offset: 3,
                columns: None,
            },
            StreamOptions::default(),
            AsyncTableOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(skipped.next().await.unwrap().unwrap().num_rows(), 3);
    skipped.close().await.unwrap();
    let mut required = snapshot
        .scan_async(
            ScanSelection::default(),
            StreamOptions::default(),
            AsyncTableOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(required
        .next()
        .await
        .unwrap_err()
        .to_string()
        .contains("SHA-256"));
    required.close().await.unwrap();
}

mod native_handles {
    use super::*;
    use fsspec_rs::async_files::{
        AsyncContext, AsyncFile, AsyncFilesystem, FileProvider, FilesystemProvider, Limits,
    };
    use fsspec_rs::async_io::{Completion, Job, ManagedDispatcher, Value};
    use fsspec_rs::{FileInfo, OperationError};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(Default)]
    struct State {
        block: AtomicBool,
        started: AtomicBool,
        cancelled: CancellationToken,
        release: CancellationToken,
        active: AtomicBool,
        closed: AtomicUsize,
        reads: AtomicUsize,
    }
    struct File {
        bytes: Vec<u8>,
        offset: Mutex<usize>,
        state: Arc<State>,
    }
    impl FileProvider for File {
        fn read(&self, size: usize, completion: Completion) {
            self.state.reads.fetch_add(1, Ordering::SeqCst);
            if self.state.block.load(Ordering::SeqCst) {
                let state = self.state.clone();
                let cancel = state.cancelled.clone();
                completion
                    .on_cancel(
                        Arc::new(ManagedDispatcher),
                        Job::new(move || cancel.cancel()),
                    )
                    .unwrap();
                state.active.store(true, Ordering::SeqCst);
                std::thread::spawn(move || {
                    state.started.store(true, Ordering::SeqCst);
                    futures::executor::block_on(state.cancelled.cancelled());
                    futures::executor::block_on(state.release.cancelled());
                    state.active.store(false, Ordering::SeqCst);
                    drop(completion);
                });
            } else {
                let mut offset = self.offset.lock().unwrap();
                let end = (*offset + size).min(self.bytes.len());
                let bytes = self.bytes[*offset..end].to_vec();
                *offset = end;
                completion.complete(Ok(Value::Bytes(bytes)));
            }
        }
        fn info(&self, completion: Completion) {
            completion.complete(Ok(Value::Info(FileInfo::file(
                "part",
                self.bytes.len() as u64,
            ))));
        }
        fn close(&self, completion: Completion) {
            assert!(!self.state.active.load(Ordering::SeqCst));
            self.state.closed.fetch_add(1, Ordering::SeqCst);
            completion.complete(Ok(Value::Unit));
        }
    }
    struct Fs {
        fs: SharedFileSystem,
        context: AsyncContext,
        state: Arc<State>,
    }
    impl FilesystemProvider for Fs {
        fn info(&self, path: &str, completion: Completion) {
            completion.complete(self.fs.info(path).map(Value::Info).map_err(Into::into));
        }
        fn open_read(&self, path: &str, completion: Completion) {
            let file = self
                .fs
                .cat_file(path, None, None)
                .map_err(OperationError::from)
                .and_then(|bytes| {
                    AsyncFile::new(
                        &self.context,
                        Arc::new(ManagedDispatcher),
                        Arc::new(File {
                            bytes,
                            offset: Mutex::new(0),
                            state: self.state.clone(),
                        }),
                    )
                });
            completion.complete(file.map(Value::File));
        }
    }
    #[tokio::test]
    async fn cancel_native_read_drains_provider_before_closing_file() {
        let (fs, root, _sink, _reader) = setup();
        let context = AsyncContext::new(Limits::default()).unwrap();
        let state = Arc::new(State::default());
        let filesystem = AsyncFilesystem::new(
            &context,
            Arc::new(ManagedDispatcher),
            Arc::new(Fs {
                fs,
                context: context.clone(),
                state: state.clone(),
            }),
        )
        .unwrap();
        let reader =
            ManifestReader::new_async(filesystem.clone(), root, ReadLimits::default()).unwrap();
        let snapshot = reader
            .read_async(
                Selection::Latest,
                AsyncTableOptions::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let mut scan = snapshot
            .scan_async(
                ScanSelection::default(),
                StreamOptions::default(),
                AsyncTableOptions {
                    read_chunk_size: 17,
                    ..Default::default()
                },
                cancel.clone(),
            )
            .await
            .unwrap();
        let reads = state.reads.load(Ordering::SeqCst);
        tokio::task::yield_now().await;
        assert_eq!(
            state.reads.load(Ordering::SeqCst),
            reads,
            "no part prefetch before demand"
        );
        state.block.store(true, Ordering::SeqCst);
        let waiting = async {
            while !state.started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            cancel.cancel();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(scan.next(), waiting)
        })
        .await
        .unwrap();
        assert!(result.is_err());
        state.cancelled.cancelled().await;
        assert!(state.active.load(Ordering::SeqCst));
        let closed = state.closed.load(Ordering::SeqCst);
        let close = scan.close();
        futures::pin_mut!(close);
        assert!(futures::poll!(&mut close).is_pending());
        state.release.cancel();
        tokio::time::timeout(Duration::from_secs(2), close)
            .await
            .unwrap()
            .unwrap();
        assert!(!state.active.load(Ordering::SeqCst));
        assert_eq!(state.closed.load(Ordering::SeqCst), closed + 1);
        filesystem.close(None).unwrap().await.unwrap();
        context.close(None).unwrap().await.unwrap();
    }
}

#[tokio::test]
async fn discovery_and_initialization_failures_are_typed_and_do_not_poison_reader() {
    let (_fs, root, _sink, reader) = setup();
    for options in [
        AsyncTableOptions {
            max_batch_bytes: 0,
            ..Default::default()
        },
        AsyncTableOptions {
            read_chunk_size: 0,
            ..Default::default()
        },
        AsyncTableOptions {
            read_chunk_size: 8 * 1024 * 1024 + 1,
            ..Default::default()
        },
    ] {
        let error = reader
            .read_async(Selection::Latest, options, CancellationToken::new())
            .await
            .err()
            .unwrap();
        assert!(
            matches!(error, fsspec_data::InterchangeError::Operation(ref e) if e.kind() == fsspec_rs::ErrorKind::Invalid)
        );
    }
    let error = reader
        .read_async(
            Selection::Generation(100),
            AsyncTableOptions::default(),
            CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error, fsspec_data::InterchangeError::Operation(ref e) if e.kind() == fsspec_rs::ErrorKind::NotFound)
    );
    let error = reader
        .read_async(
            Selection::Latest,
            AsyncTableOptions {
                deadline: Some(Instant::now()),
                ..Default::default()
            },
            CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error, fsspec_data::InterchangeError::Operation(ref e) if e.kind() == fsspec_rs::ErrorKind::Timeout)
    );
    let snapshot = reader
        .read_async(
            Selection::Latest,
            AsyncTableOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(snapshot
        .scan_async(
            ScanSelection {
                columns: Some(vec!["absent".into()]),
                row_offset: 0
            },
            StreamOptions::default(),
            AsyncTableOptions::default(),
            CancellationToken::new()
        )
        .await
        .is_err());
    assert!(AsyncManifestWatcher::new(
        reader.clone(),
        root.clone(),
        WatchOptions {
            batch_size: 0,
            ..Default::default()
        },
        AsyncTableOptions::default(),
        CancellationToken::new()
    )
    .await
    .is_err());
    let mut watch = AsyncManifestWatcher::new(
        reader,
        root,
        WatchOptions::default(),
        AsyncTableOptions::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    watch.next().await.unwrap().unwrap();
    let checkpoint = watch.position().clone();
    // Dropping a pending Rust next future requests cancellation, just like task cancellation.
    let mut pending = Box::pin(watch.next());
    assert!(futures::poll!(pending.as_mut()).is_pending());
    drop(pending);
    assert!(watch.next().await.unwrap().is_none());
    assert_eq!(watch.end_reason(), Some(WatchEnd::Cancelled));
    assert_eq!(*watch.position(), checkpoint);
    watch.close().await.unwrap();
    let mut scan = snapshot
        .scan_async(
            ScanSelection::default(),
            StreamOptions::default(),
            AsyncTableOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    scan.close().await.unwrap();
    assert!(scan.next().await.unwrap().is_none());
    assert_eq!(checkpoint.progress, 3);
}
