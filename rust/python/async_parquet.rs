use std::future::IntoFuture;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};

use arrow::array::RecordBatch;
use arrow::pyarrow::PyArrowType;
use fsspec_data::{
    AsyncDecodedStream, AsyncScanOptions, CancellationToken, InterchangeError, ParquetCodec,
    ScanIdentity, ScanSelection, SourceIdentity, StreamOptions,
};
use fsspec_rs::async_files::{AsyncContext, AsyncFile, AsyncFilesystem, Limits};
use fsspec_rs::async_io::{Operation, Value};
use fsspec_rs::{AsyncFsFile, ErrorKind, FsError, FsOptions, OperationError};

use super::async_file::PythonAsyncFile;
use super::scan_identity::PythonScanIdentity;
use super::shared_reads::RangeCache;
use futures::StreamExt;
use pyo3::exceptions::{
    PyConnectionError, PyFileExistsError, PyFileNotFoundError, PyNotImplementedError, PyOSError,
    PyPermissionError, PyRuntimeError, PyTimeoutError, PyValueError,
};
use pyo3::prelude::*;
use tokio::sync::Mutex;

struct CancelOnDrop(Option<CancellationToken>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(token) = self.0.take() {
            token.cancel();
        }
    }
}

struct State {
    stream: Option<AsyncDecodedStream>,
    file: Option<AsyncFile>,
    pending: Option<Operation>,
    opened: bool,
    closed: bool,
}

enum Source {
    S3 {
        source: Arc<S3Source>,
        path: String,
        owned: bool,
    },
    Python(PythonAsyncFile),
}

#[derive(Default)]
struct SourceScans {
    closed: bool,
    scans: Vec<Weak<Session>>,
}

struct S3Source {
    cache: Arc<RangeCache>,
    context: AsyncContext,
    fs: AsyncFilesystem,
    identity: SourceIdentity,
    scans: StdMutex<SourceScans>,
    close_lock: Mutex<()>,
}

impl S3Source {
    fn new(bucket: &str, options: &FsOptions, cache_bytes: usize) -> PyResult<Arc<Self>> {
        let context = AsyncContext::new(Limits::default()).map_err(storage_error)?;
        let fs = AsyncFilesystem::s3(bucket, options, &context).map_err(storage_error)?;
        Ok(Arc::new(Self {
            cache: RangeCache::new(
                cache_bytes,
                pyo3_async_runtimes::tokio::get_runtime().handle().clone(),
            ),
            context,
            fs,
            identity: SourceIdentity::new(),
            scans: StdMutex::default(),
            close_lock: Mutex::new(()),
        }))
    }

    async fn close_resources(&self) -> PyResult<()> {
        self.cache.close().await;
        let fs = finish(self.fs.close(None)).await.map(|_| ());
        let context = finish(self.context.close(None)).await.map(|_| ());
        fs.and(context)
    }

    async fn close(&self) -> PyResult<()> {
        let _guard = self.close_lock.lock().await;
        let scans: Vec<_> = {
            let mut state = self.scans.lock().unwrap();
            state.closed = true;
            state.scans.iter().filter_map(Weak::upgrade).collect()
        };
        for session in &scans {
            session.cancellation.cancel();
        }
        let mut result = Ok(());
        for session in scans {
            let closed = session.close().await;
            result = result.and(closed);
        }
        result.and(self.close_resources().await)
    }
}

struct Session {
    source: Source,
    state: Mutex<State>,
    cancellation: CancellationToken,
    selection: ScanSelection,
    options: AsyncScanOptions,
}

impl Session {
    async fn operation(&self, state: &mut State, operation: Operation) -> PyResult<Value> {
        state.pending = Some(operation.clone());
        let result = match futures::future::select(
            Box::pin(operation.clone().into_future()),
            Box::pin(self.cancellation.cancelled()),
        )
        .await
        {
            futures::future::Either::Left((result, _)) => result.map_err(storage_error),
            futures::future::Either::Right(_) => Err(cancelled("scan closed".into())),
        };
        if let Ok(Value::File(file)) = &result {
            state.file = Some(file.clone());
        }
        operation.drain().await;
        state.pending = None;
        result
    }

    async fn decode(&self, file: impl AsyncFsFile + 'static) -> PyResult<AsyncDecodedStream> {
        ParquetCodec
            .decode_async(
                file,
                self.selection.clone(),
                self.options,
                self.cancellation.clone(),
            )
            .await
            .map_err(scan_error)
    }

    async fn close(&self) -> PyResult<()> {
        self.cancellation.cancel();
        let mut state = self.state.lock().await;
        if state.closed {
            return Ok(());
        }
        state.closed = true;
        if let Some(operation) = state.pending.take() {
            operation.cancel();
            operation.drain().await;
        }
        let stream_result = match state.stream.take() {
            Some(stream) => stream.close().await.map_err(scan_error),
            None => Ok(()),
        };
        let file_result = match state.file.take() {
            Some(file) => finish(file.close(None)).await.map(|_| ()),
            None => Ok(()),
        };
        let source_result = match &self.source {
            Source::S3 {
                source,
                owned: true,
                ..
            } => source.close_resources().await,
            Source::S3 { .. } => Ok(()),
            Source::Python(file) => file.clone().close().await.map_err(storage_error),
        };
        stream_result.and(file_result).and(source_result)
    }
}

async fn finish(operation: Result<Operation, OperationError>) -> PyResult<Value> {
    let operation = operation.map_err(storage_error)?;
    let result = operation.clone().await;
    operation.drain().await;
    result.map_err(storage_error)
}

#[pyclass]
struct NativeAsyncParquetScan {
    session: Arc<Session>,
}

impl Drop for NativeAsyncParquetScan {
    fn drop(&mut self) {
        self.session.cancellation.cancel();
    }
}

#[pymethods]
impl NativeAsyncParquetScan {
    fn open<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let session = self.session.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut guard = CancelOnDrop(Some(session.cancellation.clone()));
            let mut state = session.state.lock().await;
            if state.opened || state.closed {
                return Err(PyRuntimeError::new_err("scan is already opened or closed"));
            }
            state.opened = true;
            let (stream, identity) = match &session.source {
                Source::S3 { source, path, .. } => {
                    let timeout = session
                        .options
                        .deadline
                        .map(|deadline| deadline.saturating_duration_since(Instant::now()));
                    let operation = source.fs.open_read(path, timeout).map_err(storage_error)?;
                    let Value::File(file) = session.operation(&mut state, operation).await? else {
                        unreachable!()
                    };
                    let timeout = session
                        .options
                        .deadline
                        .map(|deadline| deadline.saturating_duration_since(Instant::now()));
                    let operation = file.info(timeout).map_err(storage_error)?;
                    let Value::Info(info) = session.operation(&mut state, operation).await? else {
                        unreachable!()
                    };
                    let identity = ScanIdentity::new(
                        source.identity.clone(),
                        info.name.clone(),
                        serde_json::json!({"etag": info.extra["etag"], "size": info.size}),
                        session.selection.clone(),
                        session.options.stream,
                        serde_json::Value::Null,
                        serde_json::Value::Null,
                        serde_json::json!({"codec": "parquet"}),
                    )
                    .map_err(scan_error)?;
                    state.file = None;
                    (
                        session.decode(source.cache.file(file, info)).await?,
                        Some(PythonScanIdentity(identity)),
                    )
                }
                Source::Python(file) => (session.decode(file.clone()).await?, None),
            };
            let schema = PyArrowType(stream.schema.as_ref().clone());
            state.stream = Some(stream);
            guard.0.take();
            Ok((schema, identity))
        })
    }

    fn next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let session = self.session.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut guard = CancelOnDrop(Some(session.cancellation.clone()));
            let mut state = session.state.lock().await;
            let batch = match &mut state.stream {
                Some(stream) => stream.next().await.transpose().map_err(scan_error)?,
                None => None,
            };
            guard.0.take();
            Ok(batch.map(PyArrowType::<RecordBatch>))
        })
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let session = self.session.clone();
        session.cancellation.cancel();
        // Dropping the Python awaiter must not interrupt provider cleanup.
        let cleanup =
            pyo3_async_runtimes::tokio::get_runtime().spawn(async move { session.close().await });
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            cleanup
                .await
                .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
        })
    }
}

#[pyfunction]
#[pyo3(signature = (bucket, path, storage_options, columns, row_offset, batch_size, row_limit, byte_limit, max_range_bytes, max_batch_bytes, read_chunk_size, timeout))]
#[allow(clippy::too_many_arguments)]
fn async_parquet_scan(
    bucket: &str,
    path: String,
    storage_options: FsOptions,
    columns: Option<Vec<String>>,
    row_offset: usize,
    batch_size: usize,
    row_limit: Option<usize>,
    byte_limit: Option<usize>,
    max_range_bytes: usize,
    max_batch_bytes: usize,
    read_chunk_size: usize,
    timeout: Option<f64>,
) -> PyResult<NativeAsyncParquetScan> {
    let options = scan_options(
        batch_size,
        row_limit,
        byte_limit,
        max_range_bytes,
        max_batch_bytes,
        read_chunk_size,
        timeout,
    )?;
    let source = S3Source::new(bucket, &storage_options, 0)?;
    Ok(new_scan(
        Source::S3 {
            source,
            path,
            owned: true,
        },
        columns,
        row_offset,
        options,
    ))
}

#[pyclass]
struct NativeS3ParquetSource {
    source: Arc<S3Source>,
}

#[pymethods]
impl NativeS3ParquetSource {
    #[new]
    fn new(bucket: &str, storage_options: FsOptions, cache_bytes: usize) -> PyResult<Self> {
        Ok(Self {
            source: S3Source::new(bucket, &storage_options, cache_bytes)?,
        })
    }

    #[pyo3(signature = (path, columns, row_offset, batch_size, row_limit, byte_limit, max_range_bytes, max_batch_bytes, read_chunk_size, timeout))]
    #[allow(clippy::too_many_arguments)]
    fn scan(
        &self,
        path: String,
        columns: Option<Vec<String>>,
        row_offset: usize,
        batch_size: usize,
        row_limit: Option<usize>,
        byte_limit: Option<usize>,
        max_range_bytes: usize,
        max_batch_bytes: usize,
        read_chunk_size: usize,
        timeout: Option<f64>,
    ) -> PyResult<NativeAsyncParquetScan> {
        let options = scan_options(
            batch_size,
            row_limit,
            byte_limit,
            max_range_bytes,
            max_batch_bytes,
            read_chunk_size,
            timeout,
        )?;
        let mut state = self.source.scans.lock().unwrap();
        if state.closed {
            return Err(PyRuntimeError::new_err("source is closed"));
        }
        let scan = new_scan(
            Source::S3 {
                source: self.source.clone(),
                path,
                owned: false,
            },
            columns,
            row_offset,
            options,
        );
        state.scans.retain(|scan| scan.strong_count() > 0);
        state.scans.push(Arc::downgrade(&scan.session));
        Ok(scan)
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.source.scans.lock().unwrap().closed = true;
        let source = self.source.clone();
        let cleanup =
            pyo3_async_runtimes::tokio::get_runtime().spawn(async move { source.close().await });
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            cleanup
                .await
                .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
        })
    }
}

#[pyfunction]
#[pyo3(signature = (provider, size, columns, row_offset, batch_size, row_limit, byte_limit, max_range_bytes, max_batch_bytes, read_chunk_size, timeout))]
#[allow(clippy::too_many_arguments)]
fn async_parquet_file(
    py: Python<'_>,
    provider: Py<PyAny>,
    size: u64,
    columns: Option<Vec<String>>,
    row_offset: usize,
    batch_size: usize,
    row_limit: Option<usize>,
    byte_limit: Option<usize>,
    max_range_bytes: usize,
    max_batch_bytes: usize,
    read_chunk_size: usize,
    timeout: Option<f64>,
) -> PyResult<NativeAsyncParquetScan> {
    let options = scan_options(
        batch_size,
        row_limit,
        byte_limit,
        max_range_bytes,
        max_batch_bytes,
        read_chunk_size,
        timeout,
    )?;
    let file = PythonAsyncFile::new(py, provider, size)?;
    Ok(new_scan(Source::Python(file), columns, row_offset, options))
}

fn new_scan(
    source: Source,
    columns: Option<Vec<String>>,
    row_offset: usize,
    options: AsyncScanOptions,
) -> NativeAsyncParquetScan {
    NativeAsyncParquetScan {
        session: Arc::new(Session {
            source,
            state: Mutex::new(State {
                stream: None,
                file: None,
                pending: None,
                opened: false,
                closed: false,
            }),
            cancellation: CancellationToken::new(),
            selection: ScanSelection {
                columns,
                row_offset,
            },
            options,
        }),
    }
}

fn scan_options(
    batch_size: usize,
    row_limit: Option<usize>,
    byte_limit: Option<usize>,
    max_range_bytes: usize,
    max_batch_bytes: usize,
    read_chunk_size: usize,
    timeout: Option<f64>,
) -> PyResult<AsyncScanOptions> {
    if batch_size == 0
        || max_range_bytes == 0
        || max_batch_bytes == 0
        || read_chunk_size == 0
        || read_chunk_size > Limits::default().max_read_size
    {
        return Err(PyValueError::new_err(
            "batch and buffer limits must be positive; read_chunk_size must not exceed 8 MiB",
        ));
    }
    let deadline = timeout
        .map(|seconds| {
            let duration = Duration::try_from_secs_f64(seconds)
                .map_err(|_| PyValueError::new_err("deadline must be finite"))?;
            Instant::now()
                .checked_add(duration)
                .ok_or_else(|| PyValueError::new_err("deadline exceeds supported range"))
        })
        .transpose()?;
    Ok(AsyncScanOptions {
        stream: StreamOptions {
            batch_size,
            row_limit,
            byte_limit,
        },
        max_range_bytes,
        max_batch_bytes,
        read_chunk_size,
        deadline,
    })
}

fn cancelled(message: String) -> PyErr {
    Python::attach(|py| {
        match py
            .import("asyncio")
            .and_then(|module| module.getattr("CancelledError"))
            .and_then(|class| class.call1((message,)))
        {
            Ok(error) => PyErr::from_value(error),
            Err(error) => error,
        }
    })
}

fn scan_error(error: InterchangeError) -> PyErr {
    match error {
        InterchangeError::StreamCancelled => cancelled(error.to_string()),
        InterchangeError::StreamDeadlineExceeded => PyTimeoutError::new_err(error.to_string()),
        InterchangeError::Operation(error) => storage_error(error),
        error => super::to_python_error(error),
    }
}

fn storage_error(error: OperationError) -> PyErr {
    if let OperationError::FileSystem(FsError::IoError(ref io)) = error {
        if let Some(error) = io.get_ref().and_then(|inner| inner.downcast_ref::<PyErr>()) {
            return Python::attach(|py| error.clone_ref(py));
        }
    }
    let message = error.to_string();
    let exception = match error.kind() {
        ErrorKind::NotFound => PyFileNotFoundError::new_err(message),
        ErrorKind::AlreadyExists => PyFileExistsError::new_err(message),
        ErrorKind::PermissionDenied => PyPermissionError::new_err(message),
        ErrorKind::Timeout => PyTimeoutError::new_err(message),
        ErrorKind::Unavailable => PyConnectionError::new_err(message),
        ErrorKind::Invalid => PyValueError::new_err(message),
        ErrorKind::NotSupported => PyNotImplementedError::new_err(message),
        ErrorKind::Cancelled => cancelled(message),
        _ => PyOSError::new_err(message),
    };
    Python::attach(|py| {
        let value = exception.value(py);
        let _ = value.setattr("kind", error.kind().as_str());
        let _ = value.setattr("retry_safe", error.retry_safe());
    });
    exception
}

pub(super) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<NativeS3ParquetSource>()?;
    module.add_function(wrap_pyfunction!(async_parquet_scan, module)?)?;
    module.add_function(wrap_pyfunction!(async_parquet_file, module)?)
}
