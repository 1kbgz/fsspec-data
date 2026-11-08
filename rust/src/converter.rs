use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use crate::{InterchangeError, Result, SharedFileSystem};

/// One side of a native conversion: a location, the filesystem that reaches it, and
/// options for that side only. Options are omitted from `Debug` output.
#[derive(Clone, Default)]
pub struct ConversionEndpoint {
    pub url: String,
    pub filesystem: Option<SharedFileSystem>,
    pub options: HashMap<String, String>,
}

impl ConversionEndpoint {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Self::default()
        }
    }

    pub fn with_filesystem(mut self, filesystem: SharedFileSystem) -> Self {
        self.filesystem = Some(filesystem);
        self
    }

    pub fn with_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.insert(key.into(), value.into());
        self
    }
}

impl fmt::Debug for ConversionEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConversionEndpoint")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// Converts between two resource types that are not one tabular file, such as a
/// multidimensional array and a multi-object store.
pub trait Converter: Send + Sync {
    fn source_type(&self) -> &str;
    fn target_type(&self) -> &str;
    fn convert(
        &self,
        source: &ConversionEndpoint,
        target: &ConversionEndpoint,
        options: &HashMap<String, String>,
    ) -> Result<()>;
}

/// Converters keyed by their trimmed, lowercase source and target types. Rust has no entry
/// points, so integrations register their converters explicitly.
#[derive(Default)]
pub struct ConverterRegistry {
    converters: HashMap<(String, String), Arc<dyn Converter>>,
}

impl ConverterRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<C: Converter + 'static>(&mut self, converter: C, replace: bool) -> Result<()> {
        let key = (
            normalize_type(converter.source_type())?,
            normalize_type(converter.target_type())?,
        );
        if !replace && self.converters.contains_key(&key) {
            return Err(InterchangeError::ConverterRegistered {
                source_type: key.0,
                target_type: key.1,
            });
        }
        self.converters.insert(key, Arc::new(converter));
        Ok(())
    }

    pub fn get(&self, source_type: &str, target_type: &str) -> Result<Arc<dyn Converter>> {
        let key = (normalize_type(source_type)?, normalize_type(target_type)?);
        self.converters
            .get(&key)
            .cloned()
            .ok_or(InterchangeError::ConverterNotRegistered {
                source_type: key.0,
                target_type: key.1,
            })
    }

    /// Registered `(source, target)` routes in sorted order.
    pub fn routes(&self) -> Vec<(String, String)> {
        let mut routes: Vec<_> = self.converters.keys().cloned().collect();
        routes.sort();
        routes
    }

    pub fn convert(
        &self,
        source_type: &str,
        target_type: &str,
        source: &ConversionEndpoint,
        target: &ConversionEndpoint,
        options: &HashMap<String, String>,
    ) -> Result<()> {
        self.get(source_type, target_type)?
            .convert(source, target, options)
    }
}

fn normalize_type(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(InterchangeError::ConverterType);
    }
    Ok(value.to_lowercase())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct Recorder {
        source: &'static str,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl Converter for Recorder {
        fn source_type(&self) -> &str {
            self.source
        }
        fn target_type(&self) -> &str {
            "Zarr"
        }
        fn convert(
            &self,
            source: &ConversionEndpoint,
            target: &ConversionEndpoint,
            options: &HashMap<String, String>,
        ) -> Result<()> {
            self.calls.lock().unwrap().push(format!(
                "{} {} -> {} {} ({})",
                source.url,
                source.options["token"],
                target.url,
                target.options["token"],
                options["chunks"]
            ));
            Ok(())
        }
    }

    #[test]
    fn routes_conversions_with_independent_options() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ConverterRegistry::new();
        registry
            .register(
                Recorder {
                    source: " NetCDF ",
                    calls: calls.clone(),
                },
                false,
            )
            .unwrap();
        registry
            .register(
                Recorder {
                    source: "xarray",
                    calls: calls.clone(),
                },
                false,
            )
            .unwrap();

        registry
            .convert(
                "netcdf",
                "ZARR",
                &ConversionEndpoint::new("s3://in/a.nc").with_option("token", "read"),
                &ConversionEndpoint::new("gs://out/a.zarr").with_option("token", "write"),
                &HashMap::from([("chunks".to_string(), "64".to_string())]),
            )
            .unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            ["s3://in/a.nc read -> gs://out/a.zarr write (64)"]
        );
        assert_eq!(
            registry.routes(),
            [
                ("netcdf".to_string(), "zarr".to_string()),
                ("xarray".to_string(), "zarr".to_string())
            ]
        );
        assert!(!format!(
            "{:?}",
            ConversionEndpoint::new("x")
                .with_filesystem(Arc::new(fsspec_rs::LocalFs::new()))
                .with_option("token", "secret")
        )
        .contains("secret"));
    }

    #[test]
    fn rejects_duplicate_missing_and_empty_routes() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ConverterRegistry::new();
        let converter = || Recorder {
            source: "xarray",
            calls: calls.clone(),
        };
        registry.register(converter(), false).unwrap();

        let error = registry.register(converter(), false).unwrap_err();
        assert_eq!(
            error.to_string(),
            "converter from \"xarray\" to \"zarr\" is already registered"
        );
        registry.register(converter(), true).unwrap();
        assert_eq!(
            registry.get("zarr", "xarray").err().unwrap().to_string(),
            "no converter registered from \"zarr\" to \"xarray\""
        );
        assert!(registry
            .register(Recorder { source: " ", calls }, false)
            .is_err());
        assert!(registry.get("xarray", "").is_err());
    }
}
