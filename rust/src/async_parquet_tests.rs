use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use arrow::array::{ArrayRef, Int32Array};
use fsspec_rs::{FileInfo, IoFuture};
use futures::executor::block_on;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;

use super::*;

#[derive(Default)]
struct State {
    reads: AtomicUsize,
    blocked: AtomicBool,
    released: CancellationToken,
    closed: AtomicBool,
    read_dropped: AtomicBool,
    close_blocked: AtomicBool,
    close_started: AtomicBool,
    close_released: CancellationToken,
    max_read: AtomicUsize,
}

struct File {
    bytes: Vec<u8>,
    position: usize,
    state: Arc<State>,
}

impl AsyncFsFile for File {
    fn read(&mut self, size: usize) -> IoFuture<'_, Vec<u8>> {
        Box::pin(async move {
            self.state.reads.fetch_add(1, Ordering::SeqCst);
            self.state.max_read.fetch_max(size, Ordering::SeqCst);
            struct Dropped(Arc<State>);
            impl Drop for Dropped {
                fn drop(&mut self) {
                    self.0.read_dropped.store(true, Ordering::SeqCst);
                }
            }
            if self.state.blocked.load(Ordering::SeqCst) {
                let _guard = Dropped(self.state.clone());
                self.state.released.cancelled().await;
            }
            let end = (self.position + size).min(self.bytes.len());
            let bytes = self.bytes[self.position..end].to_vec();
            self.position = end;
            Ok(bytes)
        })
    }
    fn seek(&mut self, position: SeekFrom) -> IoFuture<'_, u64> {
        Box::pin(async move {
            let SeekFrom::Start(position) = position else {
                panic!("expected absolute seek")
            };
            self.position = position as usize;
            Ok(position)
        })
    }
    fn info(&self) -> IoFuture<'_, FileInfo> {
        Box::pin(async move { Ok(FileInfo::file("data.parquet", self.bytes.len() as u64)) })
    }
    fn close(&mut self) -> IoFuture<'_, ()> {
        Box::pin(async move {
            self.state.close_started.store(true, Ordering::SeqCst);
            if self.state.close_blocked.load(Ordering::SeqCst) {
                self.state.close_released.cancelled().await;
            }
            self.state.closed.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
}

fn file() -> (File, Arc<State>) {
    let batch = RecordBatch::try_from_iter([
        (
            "a",
            Arc::new(Int32Array::from_iter_values(0..12)) as ArrayRef,
        ),
        (
            "b",
            Arc::new(Int32Array::from_iter_values(20..32)) as ArrayRef,
        ),
    ])
    .unwrap();
    let schema = Arc::new(batch.schema().as_ref().clone().with_metadata(
        std::collections::HashMap::from([("origin".to_owned(), "test".to_owned())]),
    ));
    let batch = batch.with_schema(schema.clone()).unwrap();
    let mut bytes = Vec::new();
    let properties = WriterProperties::builder()
        .set_max_row_group_row_count(Some(4))
        .build();
    let mut writer = ArrowWriter::try_new(&mut bytes, schema, Some(properties)).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
    let state = Arc::new(State::default());
    (
        File {
            bytes,
            position: 0,
            state: state.clone(),
        },
        state,
    )
}

async fn until(predicate: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out waiting for worker");
        futures_timer::Delay::new(Duration::from_millis(1)).await;
    }
}

#[test]
fn selected_batches_preserve_order_duplicates_metadata_and_limits() {
    block_on(async {
        let (file, state) = file();
        let mut stream = ParquetCodec
            .decode_async(
                file,
                ScanSelection {
                    columns: Some(vec!["b".into(), "a".into(), "b".into()]),
                    row_offset: 5,
                },
                AsyncScanOptions {
                    stream: StreamOptions {
                        batch_size: 1,
                        row_limit: Some(2),
                        byte_limit: None,
                    },
                    read_chunk_size: 7,
                    ..Default::default()
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(stream.schema.metadata()["origin"], "test");
        let mut values = Vec::new();
        while let Some(batch) = stream.next().await {
            let batch = batch.unwrap();
            assert_eq!(batch.schema(), stream.schema);
            assert_eq!(batch.num_columns(), 3);
            assert_eq!(batch.num_rows(), 1);
            values.push(
                batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<Int32Array>()
                    .unwrap()
                    .value(0),
            );
            assert_eq!(batch.column(0), batch.column(2));
        }
        assert_eq!(values, [25, 26]);
        assert_eq!(state.max_read.load(Ordering::SeqCst), 7);
        assert!(state.closed.load(Ordering::SeqCst));
        assert!(stream.next().await.is_none());
        stream.close().await.unwrap();
    });
}

#[test]
fn no_row_groups_are_read_without_consumer_demand() {
    block_on(async {
        let (file, state) = file();
        let mut stream = ParquetCodec
            .decode_async(
                file,
                ScanSelection::default(),
                AsyncScanOptions {
                    stream: StreamOptions {
                        batch_size: 4,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let metadata_reads = state.reads.load(Ordering::SeqCst);
        futures_timer::Delay::new(Duration::from_millis(20)).await;
        assert_eq!(state.reads.load(Ordering::SeqCst), metadata_reads);
        assert_eq!(stream.next().await.unwrap().unwrap().num_rows(), 4);
        let first_group_reads = state.reads.load(Ordering::SeqCst);
        assert!(first_group_reads > metadata_reads);
        futures_timer::Delay::new(Duration::from_millis(20)).await;
        assert_eq!(state.reads.load(Ordering::SeqCst), first_group_reads);
        stream.close().await.unwrap();
        assert!(state.closed.load(Ordering::SeqCst));
    });
}

#[test]
fn cancellation_interrupts_pending_io_and_close_waits_for_cleanup() {
    block_on(async {
        let (file, state) = file();
        let token = CancellationToken::new();
        let mut stream = ParquetCodec
            .decode_async(
                file,
                ScanSelection::default(),
                AsyncScanOptions::default(),
                token.clone(),
            )
            .await
            .unwrap();
        let reads = state.reads.load(Ordering::SeqCst);
        state.blocked.store(true, Ordering::SeqCst);
        state.close_blocked.store(true, Ordering::SeqCst);
        let cancel = async {
            until(|| state.reads.load(Ordering::SeqCst) > reads).await;
            token.cancel();
        };
        let (batch, ()) = futures::join!(stream.next(), cancel);
        assert!(matches!(
            batch,
            Some(Err(InterchangeError::StreamCancelled))
        ));
        until(|| state.close_started.load(Ordering::SeqCst)).await;
        assert!(state.read_dropped.load(Ordering::SeqCst));
        let close = stream.close();
        futures::pin_mut!(close);
        assert!(futures::poll!(&mut close).is_pending());
        state.close_released.cancel();
        close.await.unwrap();
        assert!(state.closed.load(Ordering::SeqCst));
    });
}

#[test]
fn dropping_stream_or_initialization_cancels_reads_and_closes_file() {
    block_on(async {
        let (file, state) = file();
        state.blocked.store(true, Ordering::SeqCst);
        let mut init = Box::pin(ParquetCodec.decode_async(
            file,
            ScanSelection::default(),
            AsyncScanOptions::default(),
            CancellationToken::new(),
        ));
        assert!(futures::poll!(&mut init).is_pending());
        until(|| state.reads.load(Ordering::SeqCst) > 0).await;
        drop(init);
        until(|| state.closed.load(Ordering::SeqCst)).await;
        assert!(state.read_dropped.load(Ordering::SeqCst));

        let (file, state) = self::file();
        let mut stream = ParquetCodec
            .decode_async(
                file,
                ScanSelection::default(),
                AsyncScanOptions::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let reads = state.reads.load(Ordering::SeqCst);
        state.blocked.store(true, Ordering::SeqCst);
        assert!(futures::poll!(stream.next()).is_pending());
        until(|| state.reads.load(Ordering::SeqCst) > reads).await;
        drop(stream);
        until(|| state.closed.load(Ordering::SeqCst)).await;
        assert!(state.read_dropped.load(Ordering::SeqCst));
    });
}

#[test]
fn deadlines_apply_to_pending_metadata_and_idle_streams() {
    block_on(async {
        let (file, state) = file();
        state.blocked.store(true, Ordering::SeqCst);
        let options = AsyncScanOptions {
            deadline: Some(Instant::now() + Duration::from_millis(50)),
            ..Default::default()
        };
        let result = ParquetCodec
            .decode_async(
                file,
                ScanSelection::default(),
                options,
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(
            result,
            Err(InterchangeError::StreamDeadlineExceeded)
        ));
        until(|| state.closed.load(Ordering::SeqCst)).await;

        let (file, state) = self::file();
        let options = AsyncScanOptions {
            deadline: Some(Instant::now() + Duration::from_millis(100)),
            ..Default::default()
        };
        let mut stream = ParquetCodec
            .decode_async(
                file,
                ScanSelection::default(),
                options,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        until(|| state.closed.load(Ordering::SeqCst)).await;
        assert!(matches!(
            stream.next().await,
            Some(Err(InterchangeError::StreamDeadlineExceeded))
        ));
        stream.close().await.unwrap();
    });
}

#[test]
fn buffer_limits_reject_large_ranges_and_batches() {
    block_on(async {
        let (file, state) = file();
        let result = ParquetCodec
            .decode_async(
                file,
                ScanSelection::default(),
                AsyncScanOptions {
                    max_range_bytes: 8,
                    ..Default::default()
                },
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(
            result,
            Err(InterchangeError::RangeLimitExceeded { .. })
        ));
        assert!(state.max_read.load(Ordering::SeqCst) <= 8);
        until(|| state.closed.load(Ordering::SeqCst)).await;
        for options in [
            AsyncScanOptions {
                max_batch_bytes: 1,
                ..Default::default()
            },
            AsyncScanOptions {
                stream: StreamOptions {
                    byte_limit: Some(1),
                    ..Default::default()
                },
                ..Default::default()
            },
        ] {
            let (file, state) = self::file();
            let mut stream = ParquetCodec
                .decode_async(
                    file,
                    ScanSelection::default(),
                    options,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(matches!(
                stream.next().await,
                Some(Err(InterchangeError::ByteLimitExceeded { .. }))
            ));
            assert!(stream.next().await.is_none());
            stream.close().await.unwrap();
            assert!(state.closed.load(Ordering::SeqCst));
        }
    });
}

#[test]
fn zero_rows_and_empty_projection_preserve_row_count() {
    block_on(async {
        for limit in [0, 3] {
            let (file, _) = file();
            let mut stream = ParquetCodec
                .decode_async(
                    file,
                    ScanSelection {
                        columns: Some(vec![]),
                        row_offset: 5,
                    },
                    AsyncScanOptions {
                        stream: StreamOptions {
                            row_limit: Some(limit),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            let mut rows = 0;
            while let Some(batch) = stream.next().await {
                let batch = batch.unwrap();
                rows += batch.num_rows();
                assert_eq!(batch.num_columns(), 0);
            }
            assert_eq!(rows, limit);
        }
    });
}

#[test]
fn shared_cancellation_wakes_all_scans_without_coupling_close() {
    block_on(async {
        let token = CancellationToken::new();
        let (file, _) = file();
        let first = ParquetCodec
            .decode_async(
                file,
                ScanSelection::default(),
                AsyncScanOptions::default(),
                token.clone(),
            )
            .await
            .unwrap();
        let (file, state) = self::file();
        let mut second = ParquetCodec
            .decode_async(
                file,
                ScanSelection::default(),
                AsyncScanOptions::default(),
                token.clone(),
            )
            .await
            .unwrap();
        first.close().await.unwrap();
        assert!(!token.is_cancelled());
        assert!(second.next().await.unwrap().is_ok());
        let (file, other) = self::file();
        let mut third = ParquetCodec
            .decode_async(
                file,
                ScanSelection::default(),
                AsyncScanOptions::default(),
                token.clone(),
            )
            .await
            .unwrap();
        token.cancel();
        assert!(matches!(
            second.next().await,
            Some(Err(InterchangeError::StreamCancelled))
        ));
        assert!(matches!(
            third.next().await,
            Some(Err(InterchangeError::StreamCancelled))
        ));
        second.close().await.unwrap();
        third.close().await.unwrap();
        assert!(state.closed.load(Ordering::SeqCst) && other.closed.load(Ordering::SeqCst));
    });
}

#[test]
fn invalid_options_and_missing_columns_close_the_file() {
    block_on(async {
        let (file, state) = file();
        let result = ParquetCodec
            .decode_async(
                file,
                ScanSelection::default(),
                AsyncScanOptions {
                    read_chunk_size: 0,
                    ..Default::default()
                },
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(InterchangeError::InvalidScanLimits)));
        until(|| state.closed.load(Ordering::SeqCst)).await;
        assert_eq!(state.reads.load(Ordering::SeqCst), 0);

        let (file, state) = self::file();
        let result = ParquetCodec
            .decode_async(
                file,
                ScanSelection {
                    columns: Some(vec!["missing".into()]),
                    row_offset: 0,
                },
                AsyncScanOptions::default(),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(InterchangeError::Arrow(_))));
        until(|| state.closed.load(Ordering::SeqCst)).await;
    });
}

#[test]
fn range_limit_counts_all_chunks_before_reading() {
    block_on(async {
        let (file, state) = file();
        let mut reader = FileReader {
            size: file.bytes.len() as u64,
            file: Arc::new(Mutex::new(file)),
            options: AsyncScanOptions {
                max_range_bytes: 10,
                ..Default::default()
            },
            control: Arc::new(Control {
                cancellation: CancellationToken::new(),
                stop: CancellationToken::new(),
                deadline: None,
            }),
        };
        assert!(matches!(
            reader.ranges(vec![0..6, 6..12]).await,
            Err(InterchangeError::RangeLimitExceeded {
                attempted: 12,
                limit: 10
            })
        ));
        assert_eq!(state.reads.load(Ordering::SeqCst), 0);
        assert!(reader.ranges(vec![0..6, 6..10]).await.is_ok());
    });
}

#[test]
fn cancellation_releases_queued_batch_memory_before_stream_drop() {
    block_on(async {
        let array = Arc::new(Int32Array::from(vec![1, 2, 3]));
        let allocation = Arc::downgrade(&array);
        let batch = RecordBatch::try_from_iter([("a", array as ArrayRef)]).unwrap();
        let schema = batch.schema();
        let (sender, batches) = mpsc::unbounded();
        sender.unbounded_send(Ok(batch)).unwrap();
        let (requests, _receiver) = mpsc::unbounded();
        let (done, finished) = oneshot::channel();
        let cancellation = CancellationToken::new();
        let mut stream = AsyncDecodedStream {
            schema,
            requests,
            batches,
            done: Some(finished),
            control: Arc::new(Control {
                cancellation: cancellation.clone(),
                stop: CancellationToken::new(),
                deadline: None,
            }),
            pending: true,
            finished: false,
        };
        assert!(allocation.upgrade().is_some());
        cancellation.cancel();
        assert!(matches!(
            stream.next().await,
            Some(Err(InterchangeError::StreamCancelled))
        ));
        assert!(allocation.upgrade().is_none());
        assert!(stream.next().await.is_none());
        done.send(Ok(())).unwrap();
        stream.close().await.unwrap();
    });
}
