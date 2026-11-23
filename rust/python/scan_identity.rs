use fsspec_data::{ScanIdentity, ScanSelection, SourceIdentity, StreamOptions};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

#[pyclass(name = "SourceIdentity", frozen, eq, hash)]
#[derive(PartialEq, Eq, Hash)]
struct PythonSourceIdentity(SourceIdentity);

#[pymethods]
impl PythonSourceIdentity {
    #[new]
    fn new() -> Self {
        Self(SourceIdentity::new())
    }

    fn __repr__(&self) -> &'static str {
        "SourceIdentity(...)"
    }
}

#[pyclass(name = "ScanIdentity", frozen, eq, hash)]
#[derive(PartialEq, Eq, Hash)]
pub(super) struct PythonScanIdentity(pub(super) ScanIdentity);

#[pymethods]
impl PythonScanIdentity {
    fn __repr__(&self) -> &'static str {
        "ScanIdentity(...)"
    }
}

#[pyfunction]
#[allow(clippy::too_many_arguments)]
fn scan_identity(
    source: &PythonSourceIdentity,
    path: String,
    snapshot: &str,
    columns: Option<Vec<String>>,
    row_offset: usize,
    batch_size: usize,
    row_limit: Option<usize>,
    byte_limit: Option<usize>,
    filter: &str,
    start_position: &str,
    backend_options: &str,
) -> PyResult<PythonScanIdentity> {
    let json = |value: &str| {
        serde_json::from_str(value).map_err(|_| PyValueError::new_err("invalid identity JSON"))
    };
    ScanIdentity::new(
        source.0.clone(),
        path,
        json(snapshot)?,
        ScanSelection {
            columns,
            row_offset,
        },
        StreamOptions {
            batch_size,
            row_limit,
            byte_limit,
        },
        json(filter)?,
        json(start_position)?,
        json(backend_options)?,
    )
    .map(PythonScanIdentity)
    .map_err(super::to_python_error)
}

pub(super) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PythonSourceIdentity>()?;
    module.add_class::<PythonScanIdentity>()?;
    module.add_function(wrap_pyfunction!(scan_identity, module)?)
}
