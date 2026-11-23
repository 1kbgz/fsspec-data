use std::sync::{Arc, Mutex};

use arrow::array::RecordBatch;
use arrow::datatypes::Schema;
use arrow::pyarrow::PyArrowType;
use bytes::Bytes;
use fsspec_data::manifest::{ManifestReader, ReadLimits, Selection, Snapshot};
use fsspec_data::parts::PartLimits;
use fsspec_data::sink::{
    recover, FileDataSink, PublicationReceipt, RecoveryAction, RecoveryToken, SinkOptions,
    WriteMode,
};
use fsspec_data::{CancellationToken, DecodedStream, InterchangeError, StreamOptions};
use fsspec_rs::{ErrorKind, FilesystemRegistry, FsOptions, OperationError, SharedFileSystem};
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use pyo3::exceptions::PyValueError;
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
fn corrupt(error: impl std::fmt::Display) -> InterchangeError {
    OperationError::Detailed {
        kind: ErrorKind::Corrupt,
        message: error.to_string(),
        retry_safe: false,
    }
    .into()
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
            Ok(Self { fs, root, limits })
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
        let schema = &self.snapshot.manifest().schema;
        let indices = match columns {
            Some(columns) => columns
                .iter()
                .map(|name| schema.index_of(name))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| PyValueError::new_err(e.to_string()))?,
            None => (0..schema.fields().len()).collect(),
        };
        let schema = schema
            .project(&indices)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let source = PartBatches {
            snapshot: self.snapshot.clone(),
            index: 0,
            reader: None,
            batch_size,
            indices,
            schema: Arc::new(schema.clone()),
        };
        let stream = DecodedStream::new_with_offset(
            Arc::new(schema.clone()),
            Box::new(source),
            StreamOptions {
                batch_size,
                row_limit,
                byte_limit,
            },
            CancellationToken::new(),
            row_offset,
        )
        .map_err(to_python_error)?;
        Ok((
            PyArrowType(schema),
            Py::new(py, NativeBatchStream::new(stream))?,
        ))
    }
}

struct PartBatches {
    snapshot: Arc<Snapshot>,
    index: usize,
    reader: Option<ParquetRecordBatchReader>,
    batch_size: usize,
    indices: Vec<usize>,
    schema: Arc<Schema>,
}
impl Iterator for PartBatches {
    type Item = fsspec_data::Result<RecordBatch>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(reader) = &mut self.reader {
                if let Some(batch) = reader.next() {
                    return Some(
                        batch
                            .and_then(|b| b.project(&self.indices))
                            .and_then(|b| b.with_schema(self.schema.clone()))
                            .map_err(corrupt),
                    );
                }
                self.reader = None;
            }
            let part = self.snapshot.manifest().parts.get(self.index)?;
            let result = (|| {
                if part.encoding != "parquet" || !part.options.is_empty() {
                    return Err(InterchangeError::Operation(OperationError::Detailed {
                        kind: ErrorKind::NotSupported,
                        message: "table scans require Parquet parts without encoding options"
                            .into(),
                        retry_safe: false,
                    }));
                }
                let bytes = self.snapshot.read_part(self.index)?;
                let builder = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes))
                    .map_err(corrupt)?;
                if builder.schema().as_ref() != &self.snapshot.manifest().schema
                    || builder.metadata().file_metadata().num_rows() as u64 != part.num_rows
                {
                    return Err(InterchangeError::Operation(OperationError::Detailed {
                        kind: ErrorKind::Corrupt,
                        message: "Parquet schema or row count differs from manifest".into(),
                        retry_safe: false,
                    }));
                }
                builder
                    .with_batch_size(self.batch_size)
                    .build()
                    .map_err(corrupt)
            })();
            self.index += 1;
            match result {
                Ok(reader) => self.reader = Some(reader),
                Err(error) => return Some(Err(error)),
            }
        }
    }
}

pub(super) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<NativeManifestTable>()?;
    module.add_class::<NativeTableWriter>()?;
    module.add_class::<NativeTableSnapshot>()?;
    Ok(())
}
