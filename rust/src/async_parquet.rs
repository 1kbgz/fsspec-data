use std::future::Future;
use std::io::SeekFrom;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use arrow::array::RecordBatch;
use arrow::datatypes::{Schema, SchemaRef};
use bytes::Bytes;
use fsspec_rs::AsyncFsFile;
use futures::channel::{mpsc, oneshot};
use futures::future::BoxFuture;
use futures::lock::Mutex;
use futures::{FutureExt, Stream, StreamExt};
use parquet::arrow::arrow_reader::ArrowReaderOptions;
use parquet::arrow::async_reader::{AsyncFileReader, ParquetRecordBatchStreamBuilder};
use parquet::errors::ParquetError;
use parquet::file::metadata::{ParquetMetaData, ParquetMetaDataReader};

use crate::{
    CancellationToken, InterchangeError, ParquetCodec, Result, ScanSelection, StreamOptions,
};

/// Buffer limits apply per scan, independently of the file's shared I/O budget.
#[derive(Clone, Copy, Debug)]
pub struct AsyncScanOptions {
    pub stream: StreamOptions,
    /// Maximum aggregate compressed ranges fetched for a row group or footer.
    pub max_range_bytes: usize,
    /// Maximum single decoded output batch, checked after decoding and projection.
    pub max_batch_bytes: usize,
    /// Maximum bytes requested from the file at once.
    pub read_chunk_size: usize,
    /// Absolute deadline, including metadata, decoding, and time between polls.
    pub deadline: Option<Instant>,
}

impl Default for AsyncScanOptions {
    fn default() -> Self {
        Self {
            stream: StreamOptions::default(),
            max_range_bytes: 64 * 1024 * 1024,
            max_batch_bytes: 64 * 1024 * 1024,
            read_chunk_size: 1024 * 1024,
            deadline: None,
        }
    }
}

/// A demand-driven stream. Decoding runs on one worker thread per scan.
/// Drop requests cancellation; `close().await` also waits for file cleanup.
/// Parquet decoder allocations and batches retained by callers are not budgeted.
pub struct AsyncDecodedStream {
    pub schema: SchemaRef,
    requests: mpsc::UnboundedSender<()>,
    batches: mpsc::UnboundedReceiver<Result<RecordBatch>>,
    done: Option<oneshot::Receiver<Result<()>>>,
    control: Arc<Control>,
    pending: bool,
    finished: bool,
}

fn worker_stopped() -> InterchangeError {
    std::io::Error::other("async Parquet worker stopped unexpectedly").into()
}

impl AsyncDecodedStream {
    fn finish(&mut self) {
        self.finished = true;
        self.requests.close_channel();
        self.batches.close();
        while self.batches.try_recv().is_ok() {}
    }

    pub async fn close(mut self) -> Result<()> {
        self.control.stop.cancel();
        self.finish();
        match self.done.take() {
            Some(done) => done.await.map_err(|_| worker_stopped())?,
            None => Ok(()),
        }
    }
}

impl Drop for AsyncDecodedStream {
    fn drop(&mut self) {
        self.control.stop.cancel();
    }
}

impl Stream for AsyncDecodedStream {
    type Item = Result<RecordBatch>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }
        if let Err(error) = self.control.check() {
            self.finish();
            return Poll::Ready(Some(Err(error)));
        }
        if !self.pending {
            // Only this method sends requests, and at most one is outstanding.
            let _ = self.requests.unbounded_send(());
            self.pending = true;
        }
        match self.batches.poll_next_unpin(cx) {
            Poll::Ready(Some(batch)) => {
                self.pending = false;
                if batch.is_err() {
                    self.finish();
                }
                Poll::Ready(Some(batch))
            }
            Poll::Ready(None) => {
                let result = futures::ready!(self.done.as_mut().unwrap().poll_unpin(cx));
                self.finish();
                self.done.take();
                match result.unwrap_or_else(|_| Err(worker_stopped())) {
                    Ok(()) => Poll::Ready(None),
                    Err(error) => Poll::Ready(Some(Err(error))),
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

struct Control {
    cancellation: CancellationToken,
    stop: CancellationToken,
    deadline: Option<Instant>,
}

impl Control {
    fn check(&self) -> Result<()> {
        if self.cancellation.is_cancelled() || self.stop.is_cancelled() {
            return Err(InterchangeError::StreamCancelled);
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(InterchangeError::StreamDeadlineExceeded);
        }
        Ok(())
    }

    async fn wait<T>(&self, future: impl Future<Output = T>) -> Result<T> {
        self.check()?;
        let deadline = async {
            match self.deadline {
                Some(deadline) => {
                    futures_timer::Delay::new(deadline.saturating_duration_since(Instant::now()))
                        .await
                }
                None => futures::future::pending().await,
            }
        };
        futures::pin_mut!(future, deadline);
        let result = futures::select_biased! {
            _ = self.cancellation.cancelled().fuse() => return Err(InterchangeError::StreamCancelled),
            _ = self.stop.cancelled().fuse() => return Err(InterchangeError::StreamCancelled),
            _ = deadline.fuse() => return Err(InterchangeError::StreamDeadlineExceeded),
            result = future.fuse() => result,
        };
        // Decoding may have occupied the worker past the deadline.
        self.check()?;
        Ok(result)
    }
}

struct FileReader<F> {
    file: Arc<Mutex<F>>,
    size: u64,
    options: AsyncScanOptions,
    control: Arc<Control>,
}

fn external(error: InterchangeError) -> ParquetError {
    ParquetError::External(Box::new(error))
}

impl<F: AsyncFsFile> FileReader<F> {
    async fn ranges(&mut self, ranges: Vec<Range<u64>>) -> Result<Vec<Bytes>> {
        let mut total = 0usize;
        for range in &ranges {
            if range.start > range.end || range.end > self.size {
                return Err(ParquetError::General("range outside file bounds".into()).into());
            }
            total = usize::try_from(range.end - range.start)
                .ok()
                .and_then(|length| total.checked_add(length))
                .ok_or(InterchangeError::RangeLimitExceeded {
                    limit: self.options.max_range_bytes,
                    attempted: usize::MAX,
                })?;
        }
        if total > self.options.max_range_bytes {
            return Err(InterchangeError::RangeLimitExceeded {
                limit: self.options.max_range_bytes,
                attempted: total,
            });
        }
        let mut file = self.file.lock().await;
        let mut output = Vec::with_capacity(ranges.len());
        for range in ranges {
            self.control.check()?;
            if file.seek(SeekFrom::Start(range.start)).await? != range.start {
                return Err(ParquetError::General("seek returned wrong position".into()).into());
            }
            let length = (range.end - range.start) as usize;
            let mut bytes = Vec::with_capacity(length);
            while bytes.len() < length {
                self.control.check()?;
                let requested = self.options.read_chunk_size.min(length - bytes.len());
                let chunk = file.read(requested).await?;
                if chunk.is_empty() || chunk.len() > requested {
                    return Err(
                        ParquetError::General("range response length mismatch".into()).into(),
                    );
                }
                bytes.extend_from_slice(&chunk);
            }
            output.push(Bytes::from(bytes));
        }
        Ok(output)
    }
}

impl<F: AsyncFsFile> AsyncFileReader for FileReader<F> {
    fn get_bytes(&mut self, range: Range<u64>) -> BoxFuture<'_, parquet::errors::Result<Bytes>> {
        async move { Ok(self.ranges(vec![range]).await.map_err(external)?.remove(0)) }.boxed()
    }

    fn get_byte_ranges(
        &mut self,
        ranges: Vec<Range<u64>>,
    ) -> BoxFuture<'_, parquet::errors::Result<Vec<Bytes>>> {
        async move { self.ranges(ranges).await.map_err(external) }.boxed()
    }

    fn get_metadata<'a>(
        &'a mut self,
        options: Option<&'a ArrowReaderOptions>,
    ) -> BoxFuture<'a, parquet::errors::Result<Arc<ParquetMetaData>>> {
        async move {
            let size = self.size;
            Ok(Arc::new(
                ParquetMetaDataReader::new()
                    .with_arrow_reader_options(options)
                    .load_and_finish(self, size)
                    .await?,
            ))
        }
        .boxed()
    }
}

impl ParquetCodec {
    /// Read selected Parquet data using a seekable async handle. Takes ownership
    /// and closes it on EOF, error, cancellation, or drop. The provider must pin
    /// a stable object across reads (native fsspec-rs S3 handles do this).
    pub async fn decode_async<F: AsyncFsFile + 'static>(
        &self,
        file: F,
        selection: ScanSelection,
        options: AsyncScanOptions,
        cancellation: CancellationToken,
    ) -> Result<AsyncDecodedStream> {
        let (requests, mut request_rx) = mpsc::unbounded();
        let (batches, batch_rx) = mpsc::unbounded();
        let (ready, initialized) = oneshot::channel();
        let (done, finished) = oneshot::channel();
        let stop = CancellationToken::new();
        let control = Arc::new(Control {
            cancellation,
            stop: stop.clone(),
            deadline: options.deadline,
        });
        let mut stream = AsyncDecodedStream {
            schema: Arc::new(Schema::empty()),
            requests,
            batches: batch_rx,
            done: Some(finished),
            control: control.clone(),
            pending: false,
            finished: false,
        };
        std::thread::Builder::new()
            .name("parquet-scan".into())
            .spawn(move || {
                futures::executor::block_on(async move {
                    let file = Arc::new(Mutex::new(file));
                    let mut ready = Some(ready);
                    let result: Result<()> = async {
                        if options.stream.batch_size == 0 {
                            return Err(InterchangeError::InvalidBatchSize);
                        }
                        if options.max_range_bytes == 0
                            || options.max_batch_bytes == 0
                            || options.read_chunk_size == 0
                        {
                            return Err(InterchangeError::InvalidScanLimits);
                        }
                        let size = control
                            .wait(async { file.lock().await.info().await })
                            .await??
                            .size;
                        let reader = FileReader {
                            file: file.clone(),
                            size,
                            options,
                            control: control.clone(),
                        };
                        let builder = control
                            .wait(ParquetRecordBatchStreamBuilder::new(reader))
                            .await?
                            .map_err(crate::parquet_ranges::interchange_error)?;
                        let metadata = builder.schema().metadata().clone();
                        let builder =
                            crate::codec::select_parquet(builder, &selection, options.stream)?;
                        let mut reader = builder
                            .build()
                            .map_err(crate::parquet_ranges::interchange_error)?;
                        let schema =
                            Arc::new(reader.schema().as_ref().clone().with_metadata(metadata));
                        let indices = selection
                            .columns
                            .as_ref()
                            .map(|columns| {
                                columns
                                    .iter()
                                    .map(|name| schema.index_of(name))
                                    .collect::<std::result::Result<Vec<_>, _>>()
                            })
                            .transpose()?;
                        let output_schema = match &indices {
                            Some(indices) => Arc::new(schema.project(indices)?),
                            None => schema.clone(),
                        };
                        if ready.take().unwrap().send(Ok(output_schema)).is_err() {
                            return Ok(());
                        }
                        let mut emitted_bytes = 0usize;
                        while control.wait(request_rx.next()).await?.is_some() {
                            let Some(batch) = control.wait(reader.next()).await? else {
                                break;
                            };
                            let mut batch = batch
                                .map_err(crate::parquet_ranges::interchange_error)?
                                .with_schema(schema.clone())?;
                            if let Some(indices) = &indices {
                                batch = batch.project(indices)?;
                            }
                            let size = batch.get_array_memory_size();
                            if size > options.max_batch_bytes {
                                return Err(InterchangeError::ByteLimitExceeded {
                                    limit: options.max_batch_bytes,
                                    attempted: size,
                                });
                            }
                            emitted_bytes = emitted_bytes.saturating_add(size);
                            if let Some(limit) = options.stream.byte_limit {
                                if emitted_bytes > limit {
                                    return Err(InterchangeError::ByteLimitExceeded {
                                        limit,
                                        attempted: emitted_bytes,
                                    });
                                }
                            }
                            control.check()?;
                            if batches.unbounded_send(Ok(batch)).is_err() {
                                break;
                            }
                        }
                        Ok(())
                    }
                    .await;
                    if let Err(error) = result {
                        if let Some(ready) = ready {
                            let _ = ready.send(Err(error));
                        } else {
                            let _ = batches.unbounded_send(Err(error));
                        }
                    }
                    drop(batches);
                    // Cancellation interrupts I/O above; cleanup itself must be drained.
                    let result = file.lock().await.close().await.map_err(Into::into);
                    let _ = done.send(result);
                });
            })?;
        stream.schema = initialized.await.map_err(|_| worker_stopped())??;
        stream.control.check()?;
        Ok(stream)
    }
}

#[cfg(test)]
#[path = "async_parquet_tests.rs"]
mod tests;
