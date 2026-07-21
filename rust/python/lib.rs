use std::io::{self, Read, Seek, SeekFrom, Write};

use ::fsspec_data::{
    plan_schema as build_plan, CancellationToken, CodecReader, CodecWriter, DataFormat,
    DecodedStream, InterchangeError, SchemaPolicy, StreamOptions, DEFAULT_REGISTRY,
};
use arrow::array::RecordBatch;
use arrow::datatypes::Schema;
use arrow::pyarrow::PyArrowType;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

type MappingTuple = (usize, usize, Option<String>, bool);

struct PythonReader {
    source: Py<PyAny>,
}

impl Read for PythonReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        Python::with_gil(|py| {
            let data = self
                .source
                .bind(py)
                .call_method1("read", (buffer.len(),))
                .map_err(python_io_error)?;
            let data = data
                .downcast::<PyBytes>()
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
        Python::with_gil(|py| {
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
        Python::with_gil(|py| {
            self.sink
                .bind(py)
                .call_method1("write", (PyBytes::new(py, buffer),))
                .and_then(|written| written.extract::<usize>())
                .map_err(python_io_error)
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Python::with_gil(|py| {
            self.sink
                .bind(py)
                .call_method0("flush")
                .map(|_| ())
                .map_err(python_io_error)
        })
    }
}

#[pyclass(unsendable)]
struct NativeBatchStream {
    stream: DecodedStream,
}

#[pyclass(unsendable)]
struct NativeCodecWriter {
    writer: Option<Box<dyn CodecWriter>>,
}

#[pymethods]
impl NativeCodecWriter {
    fn write_batch(&mut self, batch: PyArrowType<RecordBatch>) -> PyResult<()> {
        self.writer
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("codec writer is finished"))?
            .write_batch(&batch.0)
            .map_err(to_python_error)
    }

    fn finish(&mut self) -> PyResult<()> {
        if let Some(writer) = self.writer.take() {
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

    fn __next__(&mut self) -> PyResult<Option<PyArrowType<RecordBatch>>> {
        self.stream
            .next()
            .transpose()
            .map(|batch| batch.map(PyArrowType))
            .map_err(to_python_error)
    }

    fn cancel(&self) {
        self.stream.cancellation_token().cancel();
    }
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
            writer: Some(writer),
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
#[pyo3(signature = (format, data, schema=None, batch_size=1024, row_limit=None, byte_limit=None))]
fn decode_stream(
    py: Python<'_>,
    format: &str,
    data: &[u8],
    schema: Option<PyArrowType<Schema>>,
    batch_size: usize,
    row_limit: Option<usize>,
    byte_limit: Option<usize>,
) -> PyResult<(PyArrowType<Schema>, Py<NativeBatchStream>)> {
    let format = DataFormat::parse(format).map_err(to_python_error)?;
    let schema = schema.map(|schema| std::sync::Arc::new(schema.0));
    let stream = DEFAULT_REGISTRY
        .get(format)
        .and_then(|codec| {
            codec.decode_stream(
                data.to_vec(),
                schema,
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
    Ok((schema, Py::new(py, NativeBatchStream { stream })?))
}

#[pyfunction]
#[pyo3(signature = (format, reader, schema=None, batch_size=1024, row_limit=None, byte_limit=None))]
fn decode_reader(
    py: Python<'_>,
    format: &str,
    reader: Py<PyAny>,
    schema: Option<PyArrowType<Schema>>,
    batch_size: usize,
    row_limit: Option<usize>,
    byte_limit: Option<usize>,
) -> PyResult<(PyArrowType<Schema>, Py<NativeBatchStream>)> {
    let format = DataFormat::parse(format).map_err(to_python_error)?;
    let schema = schema.map(|schema| std::sync::Arc::new(schema.0));
    let reader: Box<dyn CodecReader> = Box::new(PythonReader { source: reader });
    let stream = DEFAULT_REGISTRY
        .get(format)
        .and_then(|codec| {
            codec.decode_reader(
                reader,
                schema,
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
    Ok((schema, Py::new(py, NativeBatchStream { stream })?))
}

#[pyfunction]
fn plan_schema(
    provided: PyArrowType<Schema>,
    requested: PyArrowType<Schema>,
    policy: &str,
) -> PyResult<Vec<MappingTuple>> {
    let policy = SchemaPolicy::parse(policy).map_err(to_python_error)?;
    build_plan(
        std::sync::Arc::new(provided.0),
        std::sync::Arc::new(requested.0),
        policy,
    )
    .map(|plan| {
        plan.mappings
            .into_iter()
            .map(|mapping| {
                (
                    mapping.source_index,
                    mapping.target_index,
                    mapping.cast.map(|cast| cast.as_str().to_string()),
                    mapping.check_nulls,
                )
            })
            .collect()
    })
    .map_err(to_python_error)
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
        InterchangeError::Io(_) => pyo3::exceptions::PyOSError::new_err(error.to_string()),
        _ => pyo3::exceptions::PyValueError::new_err(error.to_string()),
    }
}

#[pymodule]
fn fsspec_data(m: &Bound<PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(codec_capabilities, m)?)?;
    m.add_function(wrap_pyfunction!(decode_batches, m)?)?;
    m.add_function(wrap_pyfunction!(decode_reader, m)?)?;
    m.add_function(wrap_pyfunction!(decode_stream, m)?)?;
    m.add_function(wrap_pyfunction!(encode_batches, m)?)?;
    m.add_function(wrap_pyfunction!(plan_schema, m)?)?;
    m.add_function(wrap_pyfunction!(start_codec_writer, m)?)?;
    Ok(())
}
