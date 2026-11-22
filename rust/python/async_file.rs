//! Calls a Python async file provider on its original asyncio loop and context.
use std::io::{self, SeekFrom};
use std::sync::Arc;

use fsspec_rs::{AsyncFsFile, FileInfo, FsError, IoFuture, OperationError};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pyo3_async_runtimes::TaskLocals;

#[derive(Clone)]
pub(super) struct PythonAsyncFile {
    provider: Arc<Py<PyAny>>,
    locals: TaskLocals,
    size: u64,
}

fn python_error(error: PyErr) -> OperationError {
    OperationError::FileSystem(FsError::IoError(io::Error::other(error)))
}

impl PythonAsyncFile {
    pub(super) fn new(py: Python<'_>, provider: Py<PyAny>, size: u64) -> PyResult<Self> {
        Ok(Self {
            provider: Arc::new(provider),
            locals: TaskLocals::with_running_loop(py)?.copy_context(py)?,
            size,
        })
    }

    async fn call(&self, method: &str, argument: Option<u64>) -> PyResult<Py<PyAny>> {
        let future = Python::attach(|py| {
            let coroutine = match argument {
                Some(argument) => self
                    .provider
                    .bind(py)
                    .call_method1("_call", (method, argument))?,
                None => self.provider.bind(py).call_method0("_close")?,
            };
            pyo3_async_runtimes::into_future_with_locals(&self.locals, coroutine)
        })?;
        future.await
    }
}

impl AsyncFsFile for PythonAsyncFile {
    fn read(&mut self, max_bytes: usize) -> IoFuture<'_, Vec<u8>> {
        Box::pin(async move {
            let result = self
                .call("read", Some(max_bytes as u64))
                .await
                .map_err(python_error)?;
            Python::attach(|py| {
                let bytes = result
                    .bind(py)
                    .cast::<PyBytes>()
                    .map_err(|error| python_error(error.into()))?
                    .as_bytes();
                if bytes.len() > max_bytes {
                    return Err(python_error(PyValueError::new_err(
                        "async read returned more bytes than requested",
                    )));
                }
                Ok(bytes.to_vec())
            })
        })
    }

    fn seek(&mut self, position: SeekFrom) -> IoFuture<'_, u64> {
        Box::pin(async move {
            let SeekFrom::Start(position) = position else {
                return Err(OperationError::FileSystem(FsError::NotSupported(
                    "Parquet reader requires absolute seeks".into(),
                )));
            };
            let result = self
                .call("seek", Some(position))
                .await
                .map_err(python_error)?;
            Python::attach(|py| result.extract(py).map_err(python_error))
        })
    }

    fn info(&self) -> IoFuture<'_, FileInfo> {
        Box::pin(async move { Ok(FileInfo::file("parquet", self.size)) })
    }

    fn close(&mut self) -> IoFuture<'_, ()> {
        Box::pin(async move {
            self.call("close", None)
                .await
                .map(|_| ())
                .map_err(python_error)
        })
    }
}
