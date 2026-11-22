use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::array::RecordBatch;
use arrow::pyarrow::PyArrowType;
use fsspec_data::{
    AsyncDecodedStream, AsyncScanOptions, CancellationToken, InterchangeError, ParquetCodec,
    ScanSelection, StreamOptions,
};
use fsspec_rs::async_files::{AsyncContext, AsyncFilesystem, Limits};
use fsspec_rs::async_io::{Operation, Value};
use fsspec_rs::{ErrorKind, FsOptions, OperationError};
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
    opened: bool,
    closed: bool,
}

struct Session {
    context: AsyncContext,
    fs: AsyncFilesystem,
    state: Mutex<State>,
    cancellation: CancellationToken,
    path: String,
    selection: ScanSelection,
    options: AsyncScanOptions,
}

impl Session {
    async fn close(&self) -> PyResult<()> {
        self.cancellation.cancel();
        let mut state = self.state.lock().await;
        if state.closed {
            return Ok(());
        }
        state.closed = true;
        let stream_result = match state.stream.take() {
            Some(stream) => stream.close().await.map_err(scan_error),
            None => Ok(()),
        };
        let fs_result = finish(self.fs.close(None)).await.map(|_| ());
        let context_result = finish(self.context.close(None)).await.map(|_| ());
        stream_result.and(fs_result).and(context_result)
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
            let timeout = session
                .options
                .deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now()));
            let Value::File(file) = finish(session.fs.open_read(&session.path, timeout)).await?
            else {
                unreachable!()
            };
            let stream = ParquetCodec
                .decode_async(
                    file,
                    session.selection.clone(),
                    session.options,
                    session.cancellation.clone(),
                )
                .await
                .map_err(scan_error)?;
            let schema = PyArrowType(stream.schema.as_ref().clone());
            state.stream = Some(stream);
            guard.0.take();
            Ok(schema)
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
    let limits = Limits::default();
    if batch_size == 0
        || max_range_bytes == 0
        || max_batch_bytes == 0
        || read_chunk_size == 0
        || read_chunk_size > limits.max_read_size
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
    let context = AsyncContext::new(limits).map_err(storage_error)?;
    let fs = AsyncFilesystem::s3(bucket, &storage_options, &context).map_err(storage_error)?;
    Ok(NativeAsyncParquetScan {
        session: Arc::new(Session {
            context,
            fs,
            state: Mutex::new(State {
                stream: None,
                opened: false,
                closed: false,
            }),
            cancellation: CancellationToken::new(),
            path,
            selection: ScanSelection {
                columns,
                row_offset,
            },
            options: AsyncScanOptions {
                stream: StreamOptions {
                    batch_size,
                    row_limit,
                    byte_limit,
                },
                max_range_bytes,
                max_batch_bytes,
                read_chunk_size,
                deadline,
            },
        }),
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
    module.add_function(wrap_pyfunction!(async_parquet_scan, module)?)
}
