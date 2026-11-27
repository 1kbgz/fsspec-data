use std::sync::{Arc, Mutex};
use std::time::Duration;

use arrow::array::RecordBatch;
use arrow::datatypes::Schema;
use arrow::pyarrow::PyArrowType;
use fsspec_data::cleanup::{CleanupLimits, OrphanCollector};
use fsspec_data::follow::{FollowOptions, FollowPosition, ManifestFollower};
use fsspec_data::manifest::{ManifestReader, ReadLimits, Selection, Snapshot};
use fsspec_data::parts::PartLimits;
use fsspec_data::sink::{
    recover, FileDataSink, PublicationReceipt, RecoveryAction, RecoveryToken, SinkOptions,
    WriteMode,
};
use fsspec_data::{CancellationToken, ScanSelection, StreamOptions};
use fsspec_rs::{ErrorKind, FilesystemRegistry, FsOptions, OperationError, SharedFileSystem};
use pyo3::prelude::*;
use serde_json::json;

use super::async_parquet::storage_error;
use super::{lock, to_python_error, NativeBatchStream};

fn invalid(message: &str) -> PyErr {
    storage_error(OperationError::Detailed {
        kind: ErrorKind::Invalid,
        message: message.into(),
        retry_safe: false,
    })
}
fn action(retry: bool) -> RecoveryAction {
    if retry {
        RecoveryAction::Retry
    } else {
        RecoveryAction::Observe
    }
}
fn receipt(value: PublicationReceipt) -> String {
    json!({
        "recovery": value.recovery, "sha256": value.sha256,
        "progress": value.progress, "num_rows": value.num_rows, "complete": value.complete,
        "marker_error": value.marker_error.map(|e| json!({"kind": e.kind.as_str(), "message": e.message}))
    }).to_string()
}

#[pyclass]
struct NativeManifestTable {
    fs: SharedFileSystem,
    root: String,
    limits: ReadLimits,
    source: String,
}
#[pymethods]
impl NativeManifestTable {
    #[new]
    fn new(
        py: Python<'_>,
        url: String,
        options: FsOptions,
        max_manifest_bytes: usize,
        max_part_bytes: usize,
        max_manifest_reads: usize,
    ) -> PyResult<Self> {
        py.detach(|| {
            // A fresh configured instance avoids process-global credential/client caching.
            let (fs, root) = FilesystemRegistry::default()
                .url_to_fs_uncached(&url, &options)
                .map_err(|e| storage_error(e.into()))?;
            let root = if fs.protocol().contains(&"file") {
                std::path::absolute(root)
                    .map_err(|e| storage_error(fsspec_rs::FsError::IoError(e).into()))?
                    .to_string_lossy()
                    .into_owned()
            } else {
                root
            };
            let limits = ReadLimits {
                max_manifest_bytes,
                max_part_bytes,
                max_manifest_reads,
            };
            ManifestReader::new(fs.clone(), &root, limits).map_err(storage_error)?;
            // Location identity excludes credentials and is stable across process restarts.
            let source = json!([
                fs.unstrip_protocol(&root),
                options.get("endpoint_url"),
                options.get("region")
            ])
            .to_string();
            Ok(Self {
                fs,
                root,
                limits,
                source,
            })
        })
    }

    fn snapshot(&self, py: Python<'_>, generation: Option<u64>) -> PyResult<NativeTableSnapshot> {
        py.detach(|| {
            let snapshot = ManifestReader::new(self.fs.clone(), &self.root, self.limits)
                .and_then(|reader| {
                    reader.read(generation.map_or(Selection::Latest, Selection::Generation))
                })
                .map_err(storage_error)?;
            if snapshot.manifest().deleted {
                return Err(storage_error(OperationError::Detailed {
                    kind: ErrorKind::NotFound,
                    message: "table is deleted".into(),
                    retry_safe: false,
                }));
            }
            Ok(NativeTableSnapshot {
                snapshot: Arc::new(snapshot),
            })
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn follow(
        &self,
        py: Python<'_>,
        start: Option<String>,
        columns: Option<Vec<String>>,
        batch_size: usize,
        poll_interval: f64,
        idle_timeout: Option<f64>,
    ) -> PyResult<NativeTableFollower> {
        let options = FollowOptions {
            start: start
                .map(|s| serde_json::from_str::<FollowPosition>(&s))
                .transpose()
                .map_err(|e| invalid(&e.to_string()))?,
            columns,
            batch_size,
            poll_interval: Duration::try_from_secs_f64(poll_interval)
                .map_err(|_| invalid("poll_interval must be finite and positive"))?,
            idle_timeout: idle_timeout
                .map(Duration::try_from_secs_f64)
                .transpose()
                .map_err(|_| invalid("idle_timeout must be finite and nonnegative"))?,
        };
        let cancellation = CancellationToken::new();
        let follower = py
            .detach(|| {
                let reader = ManifestReader::new(self.fs.clone(), &self.root, self.limits)?;
                ManifestFollower::new(reader, self.source.clone(), options, cancellation.clone())
            })
            .map_err(to_python_error)?;
        Ok(NativeTableFollower {
            follower: Mutex::new(follower),
            cancellation,
        })
    }

    fn orphan_collector(
        &self,
        retention_seconds: f64,
        max_generations: usize,
        max_entries: usize,
    ) -> PyResult<NativeOrphanCollector> {
        let retention = Duration::try_from_secs_f64(retention_seconds)
            .map_err(|_| invalid("retention_seconds must be finite and positive"))?;
        let collector = OrphanCollector::new(
            self.fs.clone(),
            &self.root,
            retention,
            CleanupLimits {
                max_manifest_bytes: self.limits.max_manifest_bytes,
                max_generations,
                max_entries,
            },
        )
        .map_err(storage_error)?;
        Ok(NativeOrphanCollector {
            collector: Mutex::new(collector),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn open_writer(
        &self,
        py: Python<'_>,
        schema: PyArrowType<Schema>,
        mode: &str,
        target_part_bytes: usize,
        memory_limit: usize,
        max_manifest_bytes: usize,
        max_history_reads: usize,
        partition_values: &str,
    ) -> PyResult<NativeTableWriter> {
        let mode = match mode {
            "create" => WriteMode::Create,
            "append" => WriteMode::Append,
            "replace" => WriteMode::Replace,
            _ => return Err(invalid("mode must be create, append, or replace")),
        };
        let options = SinkOptions {
            parts: PartLimits {
                target_part_bytes,
                memory_limit,
            },
            max_manifest_bytes,
            max_history_reads,
            partition_values: serde_json::from_str(partition_values)
                .map_err(|e| invalid(&e.to_string()))?,
        };
        py.detach(|| {
            FileDataSink::open(
                self.fs.clone(),
                &self.root,
                mode,
                Arc::new(schema.0),
                options,
            )
        })
        .map(|sink| NativeTableWriter {
            sink: Mutex::new(sink),
        })
        .map_err(storage_error)
    }

    #[allow(clippy::too_many_arguments)]
    fn recover(
        &self,
        py: Python<'_>,
        token: &str,
        retry: bool,
        target_part_bytes: usize,
        memory_limit: usize,
        max_manifest_bytes: usize,
        max_history_reads: usize,
    ) -> PyResult<String> {
        let token: RecoveryToken =
            serde_json::from_str(token).map_err(|e| invalid(&e.to_string()))?;
        if token.root != self.root {
            return Err(invalid("recovery token belongs to a different table path"));
        }
        let options = SinkOptions {
            parts: PartLimits {
                target_part_bytes,
                memory_limit,
            },
            max_manifest_bytes,
            max_history_reads,
            ..SinkOptions::default()
        };
        py.detach(|| recover(self.fs.clone(), &token, &options, action(retry)))
            .map(receipt)
            .map_err(storage_error)
    }
}

#[pyclass]
struct NativeTableFollower {
    follower: Mutex<ManifestFollower>,
    cancellation: CancellationToken,
}
#[pymethods]
impl NativeTableFollower {
    #[getter]
    fn schema(&self, py: Python<'_>) -> PyArrowType<Schema> {
        PyArrowType(py.detach(|| lock(&self.follower).schema().as_ref().clone()))
    }
    #[getter]
    fn position(&self, py: Python<'_>) -> String {
        py.detach(|| json!(lock(&self.follower).position()).to_string())
    }
    #[getter]
    fn end_reason(&self, py: Python<'_>) -> Option<String> {
        py.detach(|| {
            lock(&self.follower)
                .end_reason()
                .map(|reason| json!(reason).as_str().unwrap().to_owned())
        })
    }
    fn next_batch(&self, py: Python<'_>) -> PyResult<Option<(PyArrowType<RecordBatch>, String)>> {
        py.detach(|| lock(&self.follower).next().transpose())
            .map(|value| {
                value.map(|item| (PyArrowType(item.batch), json!(item.position).to_string()))
            })
            .map_err(to_python_error)
    }
    fn cancel(&self) {
        self.cancellation.cancel();
    }
}

#[pyclass]
struct NativeOrphanCollector {
    collector: Mutex<OrphanCollector>,
}
#[pymethods]
impl NativeOrphanCollector {
    fn collect(&self, py: Python<'_>, dry_run: bool) -> PyResult<String> {
        py.detach(|| lock(&self.collector).collect(dry_run))
            .map(|report| json!(report).to_string())
            .map_err(storage_error)
    }
}

#[pyclass]
struct NativeTableWriter {
    sink: Mutex<FileDataSink>,
}
#[pymethods]
impl NativeTableWriter {
    #[getter]
    fn position(&self, py: Python<'_>) -> u64 {
        py.detach(|| lock(&self.sink).position())
    }
    #[getter]
    fn recovery_token(&self, py: Python<'_>) -> String {
        py.detach(|| json!(lock(&self.sink).recovery_token()).to_string())
    }
    fn write_batch(
        &self,
        py: Python<'_>,
        batch: PyArrowType<RecordBatch>,
        expected_position: u64,
        key: &str,
    ) -> PyResult<String> {
        py.detach(|| lock(&self.sink).write_batch(expected_position, key, &batch.0))
            .map(|value| json!(value).to_string())
            .map_err(storage_error)
    }
    fn flush(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| lock(&self.sink).flush())
            .map_err(storage_error)
    }
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| lock(&self.sink).close())
            .map_err(storage_error)
    }
    fn publish(&self, py: Python<'_>, complete: bool) -> PyResult<String> {
        py.detach(|| {
            let mut sink = lock(&self.sink);
            if complete {
                sink.finish()
            } else {
                sink.commit()
            }
        })
        .map(receipt)
        .map_err(storage_error)
    }
    fn resolve(&self, py: Python<'_>, retry: bool) -> PyResult<String> {
        py.detach(|| lock(&self.sink).resolve(action(retry)))
            .map(receipt)
            .map_err(storage_error)
    }
}

#[pyclass]
struct NativeTableSnapshot {
    snapshot: Arc<Snapshot>,
}
#[pymethods]
impl NativeTableSnapshot {
    #[getter]
    fn schema(&self) -> PyArrowType<Schema> {
        PyArrowType(self.snapshot.manifest().schema.clone())
    }
    #[getter]
    fn state(&self) -> String {
        json!(self.snapshot.state()).to_string()
    }
    #[getter]
    fn metadata(&self) -> String {
        json!(self.snapshot.manifest()).to_string()
    }

    #[allow(clippy::too_many_arguments)]
    fn scan(
        &self,
        py: Python<'_>,
        columns: Option<Vec<String>>,
        batch_size: usize,
        row_limit: Option<usize>,
        byte_limit: Option<usize>,
        row_offset: usize,
    ) -> PyResult<(PyArrowType<Schema>, Py<NativeBatchStream>)> {
        let stream = self
            .snapshot
            .scan(
                ScanSelection {
                    columns,
                    row_offset,
                },
                StreamOptions {
                    batch_size,
                    row_limit,
                    byte_limit,
                },
                CancellationToken::new(),
            )
            .map_err(to_python_error)?;
        Ok((
            PyArrowType(stream.schema.as_ref().clone()),
            Py::new(py, NativeBatchStream::new(stream))?,
        ))
    }
}

pub(super) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<NativeManifestTable>()?;
    module.add_class::<NativeOrphanCollector>()?;
    module.add_class::<NativeTableFollower>()?;
    module.add_class::<NativeTableWriter>()?;
    module.add_class::<NativeTableSnapshot>()?;
    Ok(())
}
