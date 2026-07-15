use ::fsspec_data::{
    plan_schema as build_plan, CancellationToken, DataFormat, DecodedStream, InterchangeError,
    SchemaPolicy, StreamOptions, DEFAULT_REGISTRY,
};
use arrow::array::RecordBatch;
use arrow::datatypes::Schema;
use arrow::pyarrow::PyArrowType;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

type MappingTuple = (usize, usize, Option<String>, bool);

#[pyclass(unsendable)]
struct NativeBatchStream {
    stream: DecodedStream,
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
        InterchangeError::UnsupportedPolicy(_) | InterchangeError::CodecNotRegistered(_) => {
            pyo3::exceptions::PyNotImplementedError::new_err(error.to_string())
        }
        InterchangeError::StreamCancelled => {
            pyo3::exceptions::PyRuntimeError::new_err(error.to_string())
        }
        _ => pyo3::exceptions::PyValueError::new_err(error.to_string()),
    }
}

#[pymodule]
fn fsspec_data(m: &Bound<PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(codec_capabilities, m)?)?;
    m.add_function(wrap_pyfunction!(decode_batches, m)?)?;
    m.add_function(wrap_pyfunction!(decode_stream, m)?)?;
    m.add_function(wrap_pyfunction!(encode_batches, m)?)?;
    m.add_function(wrap_pyfunction!(plan_schema, m)?)?;
    Ok(())
}
