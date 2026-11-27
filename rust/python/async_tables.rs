use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::array::RecordBatch;
use arrow::pyarrow::PyArrowType;
use fsspec_data::async_tables::{AsyncManifestWatcher, AsyncTableOptions, AsyncTableScan};
use fsspec_data::manifest::{ManifestReader, Selection};
use fsspec_data::watch::WatchOptions;
use fsspec_data::{CancellationToken, ScanSelection, StreamOptions};
use fsspec_rs::async_files::{AsyncContext, AsyncFilesystem, Limits};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use tokio::sync::Mutex;

use super::async_parquet::{scan_error, storage_error};
use super::tables::{NativeManifestTable, NativeTableSnapshot};

pub(super) struct Resources {
    fs: AsyncFilesystem,
    context: AsyncContext,
}
impl Resources {
    async fn close(&self) -> PyResult<()> {
        let fs = self.fs.close(None).map_err(storage_error)?;
        let result = fs.clone().await;
        fs.drain().await;
        let context = self.context.close(None).map_err(storage_error)?;
        let closed = context.clone().await;
        context.drain().await;
        result.and(closed).map(|_| ()).map_err(storage_error)
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

pub(super) fn options(
    max_batch_bytes: usize,
    read_chunk_size: usize,
    timeout: Option<f64>,
) -> PyResult<AsyncTableOptions> {
    let deadline = timeout
        .map(|seconds| {
            Duration::try_from_secs_f64(seconds)
                .ok()
                .and_then(|d| Instant::now().checked_add(d))
                .ok_or_else(|| PyValueError::new_err("deadline must be finite and in range"))
        })
        .transpose()?;
    Ok(AsyncTableOptions {
        max_batch_bytes,
        read_chunk_size,
        deadline,
    })
}

fn reader(table: &NativeManifestTable) -> PyResult<(ManifestReader, Option<Arc<Resources>>)> {
    match &table.async_storage {
        Some((bucket, options)) => {
            let context = AsyncContext::new(Limits::default()).map_err(storage_error)?;
            let fs = AsyncFilesystem::s3(bucket, options, &context).map_err(storage_error)?;
            let reader = ManifestReader::new_async(fs.clone(), &table.root, table.limits)
                .map_err(storage_error)?;
            Ok((reader, Some(Arc::new(Resources { fs, context }))))
        }
        None => Ok((
            ManifestReader::new(table.fs.clone(), &table.root, table.limits)
                .map_err(storage_error)?,
            None,
        )),
    }
}
pub(super) fn snapshot<'py>(
    table: &NativeManifestTable,
    py: Python<'py>,
    generation: Option<u64>,
    options: AsyncTableOptions,
) -> PyResult<Bound<'py, PyAny>> {
    let (reader, resources) = reader(table)?;
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let cancellation = CancellationToken::new();
        let mut guard = CancelOnDrop(Some(cancellation.clone()));
        let snapshot = reader
            .read_async(
                generation.map_or(Selection::Latest, Selection::Generation),
                options,
                cancellation,
            )
            .await
            .map_err(scan_error)?;
        if snapshot.manifest().deleted {
            return Err(storage_error(fsspec_rs::OperationError::Detailed {
                kind: fsspec_rs::ErrorKind::NotFound,
                message: "table is deleted".into(),
                retry_safe: false,
            }));
        }
        guard.0 = None;
        Ok(NativeTableSnapshot {
            snapshot: Arc::new(snapshot),
            resources,
        })
    })
}

enum Stream {
    Scan(AsyncTableScan),
    Watch(AsyncManifestWatcher),
}
struct Session {
    stream: Mutex<Option<Stream>>,
    cancellation: CancellationToken,
    resources: Option<Arc<Resources>>,
    owns_resources: bool,
}
impl Session {
    async fn close(&self) -> PyResult<()> {
        self.cancellation.cancel();
        let mut stream = self.stream.lock().await;
        let result = match stream.as_mut() {
            Some(Stream::Scan(scan)) => scan.close().await,
            Some(Stream::Watch(watch)) => watch.close().await,
            None => Ok(()),
        }
        .map_err(scan_error);
        *stream = None;
        let resources = if self.owns_resources {
            match &self.resources {
                Some(resources) => resources.close().await,
                None => Ok(()),
            }
        } else {
            Ok(())
        };
        result.and(resources)
    }
}
#[pyclass]
pub(super) struct NativeAsyncTableStream {
    session: Arc<Session>,
}
impl Drop for NativeAsyncTableStream {
    fn drop(&mut self) {
        self.session.cancellation.cancel();
    }
}
#[pymethods]
impl NativeAsyncTableStream {
    fn next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let session = self.session.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut guard = CancelOnDrop(Some(session.cancellation.clone()));
            let mut stream = session.stream.lock().await;
            let (batch, position, end) = match stream.as_mut() {
                Some(Stream::Scan(scan)) => (scan.next().await.map_err(scan_error)?, None, None),
                Some(Stream::Watch(watch)) => {
                    let batch = watch.next().await.map_err(scan_error)?;
                    (
                        batch.map(|item| item.batch),
                        Some(serde_json::to_string(watch.position()).unwrap()),
                        watch.end_reason().map(|reason| {
                            serde_json::to_value(reason)
                                .unwrap()
                                .as_str()
                                .unwrap()
                                .to_owned()
                        }),
                    )
                }
                None => (None, None, None),
            };
            guard.0 = None;
            Ok((batch.map(PyArrowType::<RecordBatch>), position, end))
        })
    }
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let session = self.session.clone();
        session.cancellation.cancel();
        let cleanup =
            pyo3_async_runtimes::tokio::get_runtime().spawn(async move { session.close().await });
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            cleanup
                .await
                .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
        })
    }
}

pub(super) fn scan_with_resources<'py>(
    py: Python<'py>,
    snapshot: Arc<fsspec_data::manifest::Snapshot>,
    selection: ScanSelection,
    stream: StreamOptions,
    options: AsyncTableOptions,
    resources: Option<Arc<Resources>>,
) -> PyResult<Bound<'py, PyAny>> {
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let cancellation = CancellationToken::new();
        let mut guard = CancelOnDrop(Some(cancellation.clone()));
        let scan = snapshot
            .scan_async(selection, stream, options, cancellation.clone())
            .await
            .map_err(scan_error)?;
        let schema = PyArrowType(scan.schema.as_ref().clone());
        guard.0 = None;
        Ok((
            schema,
            NativeAsyncTableStream {
                session: Arc::new(Session {
                    stream: Mutex::new(Some(Stream::Scan(scan))),
                    cancellation,
                    resources,
                    owns_resources: false,
                }),
            },
        ))
    })
}

pub(super) fn watch<'py>(
    table: &NativeManifestTable,
    py: Python<'py>,
    watch: WatchOptions,
    options: AsyncTableOptions,
) -> PyResult<Bound<'py, PyAny>> {
    let (reader, resources) = reader(table)?;
    let source = table.source.clone();
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let cancellation = CancellationToken::new();
        let mut guard = CancelOnDrop(Some(cancellation.clone()));
        let watch = AsyncManifestWatcher::new(reader, source, watch, options, cancellation.clone())
            .await
            .map_err(scan_error)?;
        let schema = PyArrowType(watch.schema.as_ref().clone());
        let position = serde_json::to_string(watch.position()).unwrap();
        guard.0 = None;
        Ok((
            schema,
            position,
            NativeAsyncTableStream {
                session: Arc::new(Session {
                    stream: Mutex::new(Some(Stream::Watch(watch))),
                    cancellation,
                    resources,
                    owns_resources: true,
                }),
            },
        ))
    })
}
