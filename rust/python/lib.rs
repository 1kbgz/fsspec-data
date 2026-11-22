mod async_file;
mod async_parquet;

use std::collections::HashMap;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::sync::{Mutex, MutexGuard, PoisonError};

use ::fsspec_data::{
    plan_schema_with as build_plan, CancellationToken, CastKind, CodecReader, CodecWriter,
    DataFormat, DecodedStream, InterchangeError, ProjectionOptions, ScanSelection, SchemaPolicy,
    StreamOptions, DEFAULT_REGISTRY,
};
use arrow::array::{make_array, ArrayData, RecordBatch};
use arrow::datatypes::Schema;
use arrow::pyarrow::PyArrowType;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

type MappingTuple = (usize, usize, Option<String>, bool);
type AdditionTuple = (usize, PyArrowType<ArrayData>);

struct PythonReader {
    source: Py<PyAny>,
}

impl Read for PythonReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        Python::attach(|py| {
            let data = self
                .source
                .bind(py)
                .call_method1("read", (buffer.len(),))
                .map_err(python_io_error)?;
            let data = data
                .cast::<PyBytes>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
            let data = data.as_bytes();
            if data.len() > buffer.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Python reader returned more bytes than requested",
                ));
            }
            buffer[..data.len()].copy_from_slice(data);
            Ok(data.len())
        })
    }
}

impl Seek for PythonReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        Python::attach(|py| {
            let source = self.source.bind(py);
            let position = match position {
                SeekFrom::Start(offset) => source.call_method1("seek", (offset, 0)),
                SeekFrom::Current(offset) => source.call_method1("seek", (offset, 1)),
                SeekFrom::End(offset) => source.call_method1("seek", (offset, 2)),
            }
            .map_err(python_io_error)?;
            position.extract::<u64>().map_err(python_io_error)
        })
    }
}

fn python_io_error(error: PyErr) -> io::Error {
    io::Error::other(error.to_string())
}

struct PythonWriter {
    sink: Py<PyAny>,
}

impl Write for PythonWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        Python::attach(|py| {
            self.sink
                .bind(py)
                .call_method1("write", (PyBytes::new(py, buffer),))
                .and_then(|written| written.extract::<usize>())
                .map_err(python_io_error)
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Python::attach(|py| {
            self.sink
                .bind(py)
                .call_method0("flush")
                .map(|_| ())
                .map_err(python_io_error)
        })
    }
}

// The state sits behind a mutex so Python may use these objects from any thread, as Polars
// does when it advances an I/O source from its worker threads.
fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[pyclass]
struct NativeBatchStream {
    stream: Mutex<DecodedStream>,
    // Outside the mutex, so cancel() does not wait for a batch another thread is reading.
    cancellation: CancellationToken,
}

impl NativeBatchStream {
    fn new(stream: DecodedStream) -> Self {
        Self {
            cancellation: stream.cancellation_token(),
            stream: Mutex::new(stream),
        }
    }
}

#[pyclass]
struct NativeCodecWriter {
    writer: Mutex<Option<Box<dyn CodecWriter>>>,
}

#[pymethods]
impl NativeCodecWriter {
    fn write_batch(&self, batch: PyArrowType<RecordBatch>) -> PyResult<()> {
        lock(&self.writer)
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("codec writer is finished"))?
            .write_batch(&batch.0)
            .map_err(to_python_error)
    }

    fn finish(&self) -> PyResult<()> {
        if let Some(writer) = lock(&self.writer).take() {
            writer.finish().map_err(to_python_error)?;
        }
        Ok(())
    }
}

#[pymethods]
impl NativeBatchStream {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<Option<PyArrowType<RecordBatch>>> {
        py.detach(|| lock(&self.stream).next())
            .transpose()
            .map(|batch| batch.map(PyArrowType))
            .map_err(to_python_error)
    }

    fn cancel(&self) {
        self.cancellation.cancel();
    }
}

/// The Arrow schema that values with the Avro schema `schema` (JSON) decode to.
#[pyfunction]
fn avro_arrow_schema(schema: &str) -> PyResult<PyArrowType<Schema>> {
    let schema = ::fsspec_data::avro_arrow_schema(schema).map_err(to_python_error)?;
    Ok(PyArrowType(schema.as_ref().clone()))
}

#[pyfunction]
fn infer_schema(
    format: &str,
    sample: &[u8],
    max_records: Option<usize>,
    truncated: bool,
) -> PyResult<PyArrowType<Schema>> {
    let format = DataFormat::parse(format).map_err(to_python_error)?;
    let schema = DEFAULT_REGISTRY
        .get(format)
        .and_then(|codec| codec.infer_schema(sample, max_records, truncated))
        .map_err(to_python_error)?;
    Ok(PyArrowType(schema.as_ref().clone()))
}

#[pyfunction]
fn codec_capabilities(format: &str) -> PyResult<(bool, bool, bool)> {
    let format = DataFormat::parse(format).map_err(to_python_error)?;
    let capabilities = DEFAULT_REGISTRY
        .get(format)
        .map_err(to_python_error)?
        .capabilities();
    Ok((
        capabilities.encode,
        capabilities.decode,
        capabilities.streaming,
    ))
}

#[pyfunction]
fn encode_batches<'py>(
    py: Python<'py>,
    format: &str,
    schema: PyArrowType<Schema>,
    batches: Vec<PyArrowType<RecordBatch>>,
) -> PyResult<Bound<'py, PyBytes>> {
    let format = DataFormat::parse(format).map_err(to_python_error)?;
    let batches = batches.into_iter().map(|batch| batch.0).collect::<Vec<_>>();
    let encoded = DEFAULT_REGISTRY
        .get(format)
        .and_then(|codec| codec.encode(std::sync::Arc::new(schema.0), &batches))
        .map_err(to_python_error)?;
    Ok(PyBytes::new(py, &encoded))
}

#[pyfunction]
fn start_codec_writer(
    py: Python<'_>,
    format: &str,
    schema: PyArrowType<Schema>,
    sink: Py<PyAny>,
) -> PyResult<Py<NativeCodecWriter>> {
    let format = DataFormat::parse(format).map_err(to_python_error)?;
    let writer = DEFAULT_REGISTRY
        .get(format)
        .and_then(|codec| {
            codec.start_owned_writer(
                std::sync::Arc::new(schema.0),
                Box::new(PythonWriter { sink }),
            )
        })
        .map_err(to_python_error)?;
    Py::new(
        py,
        NativeCodecWriter {
            writer: Mutex::new(Some(writer)),
        },
    )
}

#[pyfunction]
#[pyo3(signature = (format, data, schema=None))]
fn decode_batches(
    format: &str,
    data: &[u8],
    schema: Option<PyArrowType<Schema>>,
) -> PyResult<(PyArrowType<Schema>, Vec<PyArrowType<RecordBatch>>)> {
    let format = DataFormat::parse(format).map_err(to_python_error)?;
    let schema = schema.map(|schema| std::sync::Arc::new(schema.0));
    let decoded = DEFAULT_REGISTRY
        .get(format)
        .and_then(|codec| codec.decode(data, schema))
        .map_err(to_python_error)?;
    Ok((
        PyArrowType(decoded.schema.as_ref().clone()),
        decoded.batches.into_iter().map(PyArrowType).collect(),
    ))
}

#[pyfunction]
#[pyo3(signature = (format, data, schema=None, batch_size=1024, row_limit=None, byte_limit=None, columns=None, row_offset=0))]
#[allow(clippy::too_many_arguments)]
fn decode_stream(
    py: Python<'_>,
    format: &str,
    data: &[u8],
    schema: Option<PyArrowType<Schema>>,
    batch_size: usize,
    row_limit: Option<usize>,
    byte_limit: Option<usize>,
    columns: Option<Vec<String>>,
    row_offset: usize,
) -> PyResult<(PyArrowType<Schema>, Py<NativeBatchStream>)> {
    let format = DataFormat::parse(format).map_err(to_python_error)?;
    let schema = schema.map(|schema| std::sync::Arc::new(schema.0));
    let stream = DEFAULT_REGISTRY
        .get(format)
        .and_then(|codec| {
            let options = StreamOptions {
                batch_size,
                row_limit,
                byte_limit,
            };
            if columns.is_none() && row_offset == 0 {
                codec.decode_stream(data.to_vec(), schema, options, CancellationToken::new())
            } else {
                codec.decode_reader_selected(
                    Box::new(Cursor::new(data.to_vec())),
                    schema,
                    &ScanSelection {
                        columns,
                        row_offset,
                    },
                    options,
                    CancellationToken::new(),
                )
            }
        })
        .map_err(to_python_error)?;
    let schema = PyArrowType(stream.schema.as_ref().clone());
    Ok((schema, Py::new(py, NativeBatchStream::new(stream))?))
}

#[pyfunction]
#[pyo3(signature = (format, reader, schema=None, batch_size=1024, row_limit=None, byte_limit=None, columns=None, row_offset=0))]
#[allow(clippy::too_many_arguments)]
fn decode_reader(
    py: Python<'_>,
    format: &str,
    reader: Py<PyAny>,
    schema: Option<PyArrowType<Schema>>,
    batch_size: usize,
    row_limit: Option<usize>,
    byte_limit: Option<usize>,
    columns: Option<Vec<String>>,
    row_offset: usize,
) -> PyResult<(PyArrowType<Schema>, Py<NativeBatchStream>)> {
    let format = DataFormat::parse(format).map_err(to_python_error)?;
    let schema = schema.map(|schema| std::sync::Arc::new(schema.0));
    let reader: Box<dyn CodecReader> = Box::new(PythonReader { source: reader });
    let stream = DEFAULT_REGISTRY
        .get(format)
        .and_then(|codec| {
            let options = StreamOptions {
                batch_size,
                row_limit,
                byte_limit,
            };
            codec.decode_reader_selected(
                reader,
                schema,
                &ScanSelection {
                    columns,
                    row_offset,
                },
                options,
                CancellationToken::new(),
            )
        })
        .map_err(to_python_error)?;
    let schema = PyArrowType(stream.schema.as_ref().clone());
    Ok((schema, Py::new(py, NativeBatchStream::new(stream))?))
}

#[pyfunction]
#[pyo3(signature = (fetch, batch_size=1024, row_limit=None, byte_limit=None, columns=None, row_offset=0))]
#[allow(clippy::too_many_arguments)]
fn decode_parquet_ranges(
    py: Python<'_>,
    fetch: Py<PyAny>,
    batch_size: usize,
    row_limit: Option<usize>,
    byte_limit: Option<usize>,
    columns: Option<Vec<String>>,
    row_offset: usize,
) -> PyResult<(PyArrowType<Schema>, Py<NativeBatchStream>)> {
    let stream = py
        .detach(|| {
            ::fsspec_data::ParquetCodec.decode_ranges(
                move |ranges| {
                    Python::attach(|py| {
                        let ranges: Vec<_> = ranges
                            .iter()
                            .map(|range| (range.start, range.end))
                            .collect();
                        fetch.call1(py, (ranges,))?.extract::<Vec<Vec<u8>>>(py)
                    })
                    .map_err(|error: PyErr| InterchangeError::Io(io::Error::other(error)))
                },
                &ScanSelection {
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
        })
        .map_err(to_python_error)?;
    let schema = PyArrowType(stream.schema.as_ref().clone());
    Ok((schema, Py::new(py, NativeBatchStream::new(stream))?))
}

struct PythonBatches {
    source: Py<PyAny>,
}

impl Iterator for PythonBatches {
    type Item = ::fsspec_data::Result<RecordBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        Python::attach(|py| match self.source.bind(py).call_method0("__next__") {
            Ok(batch) => Some(
                batch
                    .extract::<PyArrowType<RecordBatch>>()
                    .map(|batch| batch.0)
                    .map_err(|error| python_io_error(error).into()),
            ),
            Err(error) if error.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => None,
            Err(error) => Some(Err(python_io_error(error).into())),
        })
    }
}

#[pyfunction]
#[pyo3(signature = (schema, batches, columns, batch_size=1024, row_limit=None, byte_limit=None, row_offset=0))]
#[allow(clippy::too_many_arguments)]
fn project_batches(
    py: Python<'_>,
    schema: PyArrowType<Schema>,
    batches: Py<PyAny>,
    columns: Option<Vec<String>>,
    batch_size: usize,
    row_limit: Option<usize>,
    byte_limit: Option<usize>,
    row_offset: usize,
) -> PyResult<(PyArrowType<Schema>, Py<NativeBatchStream>)> {
    let indices = match columns {
        Some(columns) => columns
            .iter()
            .map(|name| schema.0.index_of(name))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| to_python_error(error.into()))?,
        None => (0..schema.0.fields().len()).collect(),
    };
    let schema = schema
        .0
        .project(&indices)
        .map_err(|error| to_python_error(error.into()))?;
    let source = PythonBatches { source: batches };
    let stream = DecodedStream::new_with_offset(
        std::sync::Arc::new(schema.clone()),
        Box::new(source.map(move |batch| Ok(batch?.project(&indices)?))),
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

#[pyfunction]
#[pyo3(signature = (provided, requested, policy, renames=None, defaults=None, drop=None, casts=None))]
#[allow(clippy::too_many_arguments)]
fn plan_schema(
    provided: PyArrowType<Schema>,
    requested: PyArrowType<Schema>,
    policy: &str,
    renames: Option<HashMap<String, String>>,
    defaults: Option<HashMap<String, PyArrowType<ArrayData>>>,
    drop: Option<Vec<String>>,
    casts: Option<HashMap<String, String>>,
) -> PyResult<(Vec<MappingTuple>, Vec<AdditionTuple>)> {
    let policy = SchemaPolicy::parse(policy).map_err(to_python_error)?;
    let options = ProjectionOptions {
        renames: renames.unwrap_or_default(),
        defaults: defaults
            .unwrap_or_default()
            .into_iter()
            .map(|(name, value)| (name, make_array(value.0)))
            .collect(),
        drop: drop.unwrap_or_default(),
        casts: casts
            .unwrap_or_default()
            .into_iter()
            .map(|(name, kind)| Ok((name, CastKind::parse(&kind)?)))
            .collect::<Result<_, InterchangeError>>()
            .map_err(to_python_error)?,
    };
    let plan = build_plan(
        std::sync::Arc::new(provided.0),
        std::sync::Arc::new(requested.0),
        policy,
        &options,
    )
    .map_err(to_python_error)?;
    let mappings = plan
        .mappings
        .into_iter()
        .map(|mapping| {
            (
                mapping.source_index,
                mapping.target_index,
                mapping.cast.map(|cast| cast.as_str().to_string()),
                mapping.check_nulls,
            )
        })
        .collect();
    let additions = plan
        .additions
        .into_iter()
        .map(|addition| (addition.target_index, PyArrowType(addition.value.to_data())))
        .collect();
    Ok((mappings, additions))
}

fn to_python_error(error: InterchangeError) -> PyErr {
    match error {
        InterchangeError::UnsupportedPolicy(_)
        | InterchangeError::CodecNotRegistered(_)
        | InterchangeError::CodecWriterNotSupported(_) => {
            pyo3::exceptions::PyNotImplementedError::new_err(error.to_string())
        }
        InterchangeError::StreamCancelled => {
            pyo3::exceptions::PyRuntimeError::new_err(error.to_string())
        }
        InterchangeError::Io(error) => {
            let message = error.to_string();
            match error
                .into_inner()
                .and_then(|error| error.downcast::<PyErr>().ok())
            {
                Some(error) => *error,
                None => pyo3::exceptions::PyOSError::new_err(message),
            }
        }
        _ => pyo3::exceptions::PyValueError::new_err(error.to_string()),
    }
}

#[pymodule]
fn fsspec_data(m: &Bound<PyModule>) -> PyResult<()> {
    async_parquet::register(m)?;
    m.add_function(wrap_pyfunction!(avro_arrow_schema, m)?)?;
    m.add_function(wrap_pyfunction!(codec_capabilities, m)?)?;
    m.add_function(wrap_pyfunction!(decode_batches, m)?)?;
    m.add_function(wrap_pyfunction!(decode_reader, m)?)?;
    m.add_function(wrap_pyfunction!(decode_parquet_ranges, m)?)?;
    m.add_function(wrap_pyfunction!(project_batches, m)?)?;
    m.add_function(wrap_pyfunction!(decode_stream, m)?)?;
    m.add_function(wrap_pyfunction!(encode_batches, m)?)?;
    m.add_function(wrap_pyfunction!(infer_schema, m)?)?;
    m.add_function(wrap_pyfunction!(plan_schema, m)?)?;
    m.add_function(wrap_pyfunction!(start_codec_writer, m)?)?;
    Ok(())
}
