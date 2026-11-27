//! Demand-driven table reads. Native async I/O and decoding run on a dedicated worker;
//! local files use blocking I/O on that worker. I/O and decoding stay off the caller's executor.
use std::future::IntoFuture;
use std::io::Read;
use std::sync::Arc;
use std::time::Instant;

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use fsspec_rs::async_files::AsyncFilesystem;
use fsspec_rs::async_io::{Operation, Value};
use fsspec_rs::{ErrorKind, FsError, OpenMode, OperationError};
use futures::channel::{mpsc, oneshot};
use futures::{FutureExt, StreamExt};

use crate::manifest::{ManifestReader, Selection, Snapshot};
use crate::watch::{ManifestWatcher, WatchBatch, WatchEnd, WatchOptions, WatchPosition};
use crate::{
    CancellationToken, DecodedStream, InterchangeError, ScanSelection, SharedFileSystem,
    StreamOptions,
};

/// One encoded part is retained at a time, bounded by `ReadLimits::max_part_bytes`.
/// Output is demand-driven with one outstanding batch. Decoder allocations and batches
/// retained by the caller are outside these limits.
#[derive(Clone, Copy, Debug)]
pub struct AsyncTableOptions {
    pub max_batch_bytes: usize,
    pub read_chunk_size: usize,
    /// Absolute deadline, including discovery, decoding and time between batch requests.
    pub deadline: Option<Instant>,
}
impl Default for AsyncTableOptions {
    fn default() -> Self {
        Self {
            max_batch_bytes: 64 * 1024 * 1024,
            read_chunk_size: 1024 * 1024,
            deadline: None,
        }
    }
}
fn error(kind: ErrorKind, message: &str) -> OperationError {
    OperationError::Detailed {
        kind,
        message: message.into(),
        retry_safe: false,
    }
}
#[derive(Clone)]
struct Control {
    cancel: CancellationToken,
    options: AsyncTableOptions,
}
impl Control {
    fn new(options: AsyncTableOptions, cancel: CancellationToken) -> crate::Result<Self> {
        if options.max_batch_bytes == 0
            || options.read_chunk_size == 0
            || options.read_chunk_size > 8 * 1024 * 1024
        {
            return Err(error(
                ErrorKind::Invalid,
                "positive buffer limits required; read chunks must not exceed 8 MiB",
            )
            .into());
        }
        Ok(Self { cancel, options })
    }
    fn check(&self) -> Result<(), OperationError> {
        if self.cancel.is_cancelled() {
            return Err(error(ErrorKind::Cancelled, "table read cancelled"));
        }
        if self.options.deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(error(ErrorKind::Timeout, "table read deadline exceeded"));
        }
        Ok(())
    }
    async fn stopped(&self) {
        let deadline = async {
            match self.options.deadline {
                Some(d) => {
                    futures_timer::Delay::new(d.saturating_duration_since(Instant::now())).await
                }
                None => futures::future::pending().await,
            }
        };
        futures::pin_mut!(deadline);
        futures::future::select(Box::pin(self.cancel.cancelled()), deadline).await;
    }
    async fn operation(
        &self,
        operation: Result<Operation, OperationError>,
    ) -> Result<Value, OperationError> {
        let operation = operation?;
        let result = futures::select_biased! {
            _ = self.stopped().fuse() => { operation.cancel(); Err(self.check().unwrap_err()) },
            result = operation.clone().into_future().fuse() => result,
        };
        operation.drain().await;
        result
    }
}

#[derive(Clone)]
enum Backend {
    Sync(SharedFileSystem),
    Async(AsyncFilesystem),
}
#[derive(Clone)]
pub(crate) struct ReadSource {
    backend: Backend,
    control: Option<Control>,
}
impl ReadSource {
    pub(crate) fn sync(fs: SharedFileSystem) -> Self {
        Self {
            backend: Backend::Sync(fs),
            control: None,
        }
    }
    pub(crate) fn asynchronous(fs: AsyncFilesystem) -> Self {
        Self {
            backend: Backend::Async(fs),
            control: None,
        }
    }
    pub(crate) fn read(&self, path: &str, limit: usize) -> Result<Vec<u8>, OperationError> {
        if let (Backend::Sync(fs), None) = (&self.backend, &self.control) {
            return crate::manifest::bounded_read(fs, path, limit);
        }
        let control = self.control.clone().unwrap_or(Control {
            cancel: CancellationToken::new(),
            options: AsyncTableOptions::default(),
        });
        control.check()?;
        match &self.backend {
            Backend::Sync(fs) => {
                let mut file = fs.open(path, OpenMode::Read, None)?;
                let mut bytes = Vec::new();
                loop {
                    control.check()?;
                    let count = control.options.read_chunk_size.min(limit + 1 - bytes.len());
                    if count == 0 {
                        break;
                    }
                    let start = bytes.len();
                    bytes.resize(start + count, 0);
                    let read = file.read(&mut bytes[start..]).map_err(FsError::from)?;
                    bytes.truncate(start + read);
                    if read == 0 {
                        break;
                    }
                }
                control.check()?;
                Ok(bytes)
            }
            Backend::Async(fs) => futures::executor::block_on(async {
                let Value::File(file) = control.operation(fs.open_read(path, None)).await? else {
                    unreachable!()
                };
                let result = async {
                    let mut bytes = Vec::new();
                    loop {
                        control.check()?;
                        let count = control.options.read_chunk_size.min(limit + 1 - bytes.len());
                        if count == 0 {
                            break;
                        }
                        let Value::Bytes(chunk) = control.operation(file.read(count, None)).await?
                        else {
                            unreachable!()
                        };
                        if chunk.is_empty() {
                            break;
                        }
                        bytes.extend_from_slice(&chunk);
                    }
                    control.check()?;
                    Ok(bytes)
                }
                .await;
                // Cleanup is deliberately not limited by the request deadline.
                let close = file.close(None)?;
                let closed = close.clone().await;
                close.drain().await;
                result.and_then(|bytes| closed.map(|_| bytes))
            }),
        }
    }
}

struct CancelOnDrop(Option<CancellationToken>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(token) = &self.0 {
            token.cancel();
        }
    }
}

impl ManifestReader {
    /// Resolve a snapshot without occupying the caller's executor. Local I/O cannot be
    /// interrupted; cancellation waits for the outstanding call to finish.
    pub async fn read_async(
        &self,
        selection: Selection,
        options: AsyncTableOptions,
        cancel: CancellationToken,
    ) -> crate::Result<Snapshot> {
        let control = Control::new(options, cancel)?;
        let mut guard = CancelOnDrop(Some(control.cancel.clone()));
        let mut reader = self.clone();
        reader.fs.control = Some(control.clone());
        let (send, receive) = oneshot::channel();
        std::thread::Builder::new()
            .name("manifest-read".into())
            .spawn(move || {
                let result = control
                    .check()
                    .and_then(|()| reader.read(selection))
                    .and_then(|snapshot| {
                        control.check()?;
                        Ok(snapshot)
                    });
                let _ = send.send(result);
            })?;
        let mut snapshot = receive.await.map_err(|_| worker_stopped())??;
        // Each scan has its own cancellation and deadline; a snapshot is reusable.
        snapshot.fs.control = None;
        guard.0 = None;
        Ok(snapshot)
    }
}

fn worker_stopped() -> InterchangeError {
    std::io::Error::other("table worker stopped unexpectedly").into()
}

enum Source {
    Scan(DecodedStream),
    Watch(Box<ManifestWatcher>),
}
struct Event {
    batch: Option<RecordBatch>,
    position: Option<WatchPosition>,
    end: Option<WatchEnd>,
}
struct Opened {
    schema: SchemaRef,
    position: Option<WatchPosition>,
}
struct Worker {
    requests: mpsc::UnboundedSender<()>,
    events: mpsc::UnboundedReceiver<crate::Result<Event>>,
    done: Option<oneshot::Receiver<()>>,
    control: Control,
    finished: bool,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.control.cancel.cancel();
    }
}
impl Worker {
    async fn start(
        control: Control,
        init: impl FnOnce() -> crate::Result<Source> + Send + 'static,
    ) -> crate::Result<(Opened, Self)> {
        let (requests, mut request_rx) = mpsc::unbounded();
        let (events_tx, events) = mpsc::unbounded();
        let (ready_tx, ready) = oneshot::channel();
        let (done_tx, done) = oneshot::channel();
        let worker_control = control.clone();
        std::thread::Builder::new()
            .name("manifest-scan".into())
            .spawn(move || {
                let _done = Done(Some(done_tx));
                let mut source = match worker_control
                    .check()
                    .map_err(Into::into)
                    .and_then(|()| init())
                {
                    Ok(source) => source,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                if let Err(error) = worker_control.check() {
                    let _ = ready_tx.send(Err(error.into()));
                    return;
                }
                let opened = match &source {
                    Source::Scan(scan) => Opened {
                        schema: scan.schema.clone(),
                        position: None,
                    },
                    Source::Watch(watch) => Opened {
                        schema: watch.schema(),
                        position: Some(ManifestWatcher::position(watch).clone()),
                    },
                };
                if ready_tx.send(Ok(opened)).is_err() {
                    return;
                }
                loop {
                    let request = futures::executor::block_on(async {
                        futures::select_biased! {
                            _ = worker_control.stopped().fuse() => None,
                            request = request_rx.next().fuse() => request,
                        }
                    });
                    if request.is_none() {
                        break;
                    }
                    let result = (|| {
                        worker_control.check()?;
                        let event = match &mut source {
                            Source::Scan(scan) => Event {
                                batch: scan.next().transpose()?,
                                position: None,
                                end: None,
                            },
                            Source::Watch(watch) => {
                                let item = watch.next().transpose()?;
                                Event {
                                    batch: item.map(|i| i.batch),
                                    position: Some(ManifestWatcher::position(watch).clone()),
                                    end: watch.end_reason(),
                                }
                            }
                        };
                        worker_control.check()?;
                        if let Some(batch) = &event.batch {
                            let attempted = batch.get_array_memory_size();
                            if attempted > worker_control.options.max_batch_bytes {
                                return Err(InterchangeError::ByteLimitExceeded {
                                    limit: worker_control.options.max_batch_bytes,
                                    attempted,
                                });
                            }
                        }
                        Ok(event)
                    })();
                    let finished = result.as_ref().map_or(true, |event| event.batch.is_none());
                    if events_tx.unbounded_send(result).is_err() || finished {
                        break;
                    }
                }
                drop(source);
            })?;
        let worker = Self {
            requests,
            events,
            done: Some(done),
            control,
            finished: false,
        };
        let opened = ready.await.map_err(|_| worker_stopped())??;
        Ok((opened, worker))
    }
    async fn next(&mut self) -> crate::Result<Option<Event>> {
        if self.finished {
            return Ok(None);
        }
        if let Err(error) = self.control.check() {
            self.finished = true;
            return Err(error.into());
        }
        let mut guard = CancelOnDrop(Some(self.control.cancel.clone()));
        let _ = self.requests.unbounded_send(());
        let result = futures::select_biased! {
            _ = self.control.stopped().fuse() => Err(self.control.check().unwrap_err().into()),
            event = self.events.next().fuse() => event.unwrap_or_else(|| {
                self.control.check().map_err(Into::into).and_then(|()| Err(worker_stopped()))
            }),
        };
        guard.0 = None;
        match result {
            Ok(event) => {
                self.finished = event.batch.is_none();
                Ok(Some(event))
            }
            Err(error) => {
                self.finished = true;
                self.control.cancel.cancel();
                Err(error)
            }
        }
    }
    async fn close(&mut self) -> crate::Result<()> {
        self.control.cancel.cancel();
        self.finished = true;
        self.requests.close_channel();
        self.events.close();
        while self.events.try_recv().is_ok() {}
        if let Some(done) = self.done.as_mut() {
            done.await.map_err(|_| worker_stopped())?;
            self.done = None;
        }
        Ok(())
    }
}
struct Done(Option<oneshot::Sender<()>>);
impl Drop for Done {
    fn drop(&mut self) {
        if let Some(done) = self.0.take() {
            let _ = done.send(());
        }
    }
}

/// Demand-driven batches. `close().await` cancels and drains the worker and file handles.
pub struct AsyncTableScan {
    pub schema: SchemaRef,
    worker: Worker,
}
impl Snapshot {
    pub async fn scan_async(
        &self,
        selection: ScanSelection,
        stream: StreamOptions,
        options: AsyncTableOptions,
        cancel: CancellationToken,
    ) -> crate::Result<AsyncTableScan> {
        let control = Control::new(options, cancel.clone())?;
        let mut snapshot = self.clone();
        snapshot.fs.control = Some(control.clone());
        let (opened, worker) = Worker::start(control, move || {
            Ok(Source::Scan(
                Arc::new(snapshot).scan(selection, stream, cancel)?,
            ))
        })
        .await?;
        Ok(AsyncTableScan {
            schema: opened.schema,
            worker,
        })
    }
}
impl AsyncTableScan {
    pub async fn next(&mut self) -> crate::Result<Option<RecordBatch>> {
        Ok(self.worker.next().await?.and_then(|event| event.batch))
    }
    pub async fn close(&mut self) -> crate::Result<()> {
        self.worker.close().await
    }
}

/// Async watch with the same checkpoints and terminal reasons as `ManifestWatcher`.
pub struct AsyncManifestWatcher {
    pub schema: SchemaRef,
    worker: Worker,
    position: WatchPosition,
    end: Option<WatchEnd>,
    failed: bool,
}
impl AsyncManifestWatcher {
    pub async fn new(
        mut reader: ManifestReader,
        source: String,
        watch: WatchOptions,
        options: AsyncTableOptions,
        cancel: CancellationToken,
    ) -> crate::Result<Self> {
        let control = Control::new(options, cancel.clone())?;
        reader.fs.control = Some(control.clone());
        let (opened, worker) = Worker::start(control, move || {
            Ok(Source::Watch(Box::new(ManifestWatcher::new(
                reader, source, watch, cancel,
            )?)))
        })
        .await?;
        Ok(Self {
            schema: opened.schema,
            worker,
            position: opened.position.unwrap(),
            end: None,
            failed: false,
        })
    }
    pub fn position(&self) -> &WatchPosition {
        &self.position
    }
    pub fn end_reason(&self) -> Option<WatchEnd> {
        self.end
    }
    pub async fn next(&mut self) -> crate::Result<Option<WatchBatch>> {
        match self.worker.next().await {
            Ok(Some(event)) => {
                self.end = event.end;
                if let Some(batch) = event.batch {
                    self.position = event.position.unwrap();
                    Ok(Some(WatchBatch {
                        batch,
                        position: self.position.clone(),
                    }))
                } else {
                    Ok(None)
                }
            }
            Err(InterchangeError::Operation(error)) if error.kind() == ErrorKind::Cancelled => {
                self.end = Some(WatchEnd::Cancelled);
                self.worker.close().await?;
                Ok(None)
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
            Ok(None) => Ok(None),
        }
    }
    pub async fn close(&mut self) -> crate::Result<()> {
        if self.end.is_none() && !self.failed {
            self.end = Some(WatchEnd::Cancelled);
        }
        self.worker.close().await
    }
}
