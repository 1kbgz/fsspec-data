use std::collections::HashMap;
use std::fmt;
use std::io::Cursor;
use std::sync::{Arc, LazyLock};

use arrow::datatypes::{DataType, Field, IntervalUnit, Schema, SchemaRef, TimeUnit};
use arrow::ipc::reader::StreamReader;
use fsspec_rs::{FileSystem, LocalFs};
use serde_json::Value;

use crate::json_schema::json_schema_to_arrow;
use crate::{avro_arrow_schema, InterchangeError, Result};

/// A filesystem shared between schema references, resolvers, and converters.
pub type SharedFileSystem = Arc<dyn FileSystem + Send + Sync>;

/// Decodes a schema document into an Arrow schema.
pub type SchemaDecoder = Arc<dyn Fn(&[u8]) -> Result<SchemaRef> + Send + Sync>;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SchemaFormat {
    ArrowIpc,
    ArrowJson,
    JsonSchema,
    Avro,
}

impl SchemaFormat {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "arrow-ipc-schema" => Ok(Self::ArrowIpc),
            "arrow-json" => Ok(Self::ArrowJson),
            "json-schema" => Ok(Self::JsonSchema),
            "avro-schema" => Ok(Self::Avro),
            _ => Err(schema_error(format!("unknown schema format {value:?}"))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ArrowIpc => "arrow-ipc-schema",
            Self::ArrowJson => "arrow-json",
            Self::JsonSchema => "json-schema",
            Self::Avro => "avro-schema",
        }
    }

    fn from_url(url: &str) -> Result<Self> {
        let path = url.split('?').next().unwrap_or(url).to_lowercase();
        if [".arrow", ".ipc", ".arrowschema"]
            .iter()
            .any(|suffix| path.ends_with(suffix))
        {
            return Ok(Self::ArrowIpc);
        }
        if path.ends_with(".avsc") {
            return Ok(Self::Avro);
        }
        Err(schema_error(format!(
            "schema format cannot be inferred from {url:?}; provide format explicitly"
        )))
    }
}

/// A schema stored apart from the data it describes.
///
/// `filesystem` reads `url` with its own credentials. Without one, the resolver uses the
/// filesystem registered for the URL protocol. `options` configure a schema provider, such
/// as a registry URL and credentials, and are omitted from `Debug` output.
#[derive(Clone)]
pub struct SchemaReference {
    pub url: String,
    pub format: Option<SchemaFormat>,
    pub filesystem: Option<SharedFileSystem>,
    pub options: HashMap<String, String>,
}

impl SchemaReference {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            format: None,
            filesystem: None,
            options: HashMap::new(),
        }
    }

    pub fn with_format(mut self, format: SchemaFormat) -> Self {
        self.format = Some(format);
        self
    }

    pub fn with_filesystem(mut self, filesystem: SharedFileSystem) -> Self {
        self.filesystem = Some(filesystem);
        self
    }

    pub fn with_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.insert(key.into(), value.into());
        self
    }

    fn protocol(&self) -> String {
        self.url
            .split_once("://")
            .map_or("file", |(protocol, _)| protocol)
            .to_lowercase()
    }
}

impl fmt::Debug for SchemaReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SchemaReference")
            .field("url", &self.url)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaProvenance {
    pub source: String,
    pub format: String,
    pub provider: String,
    pub identifier: Option<String>,
    pub version: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ResolvedSchema {
    pub schema: SchemaRef,
    pub provenance: SchemaProvenance,
}

#[derive(Clone, Debug)]
pub struct SchemaDocument {
    pub data: Vec<u8>,
    pub format: SchemaFormat,
    pub provenance: SchemaProvenance,
}

/// Fetches schema documents for one URL protocol, such as a schema registry.
pub trait SchemaProvider: Send + Sync {
    fn fetch(&self, reference: &SchemaReference) -> Result<SchemaDocument>;
}

impl<F> SchemaProvider for F
where
    F: Fn(&SchemaReference) -> Result<SchemaDocument> + Send + Sync,
{
    fn fetch(&self, reference: &SchemaReference) -> Result<SchemaDocument> {
        self(reference)
    }
}

/// Resolves schema references to Arrow schemas before planning.
///
/// A reference is fetched by the provider registered for its protocol, or else read through
/// its own filesystem or the filesystem registered for its protocol. The document is then
/// decoded by the decoder registered for its format.
pub struct SchemaResolverRegistry {
    decoders: HashMap<SchemaFormat, SchemaDecoder>,
    providers: HashMap<String, Arc<dyn SchemaProvider>>,
    filesystems: HashMap<String, SharedFileSystem>,
}

impl Default for SchemaResolverRegistry {
    /// Decoders for every [`SchemaFormat`], local files, and the Confluent provider when the
    /// `confluent` feature is enabled.
    fn default() -> Self {
        let mut registry = Self::empty();
        registry.register_decoder(SchemaFormat::ArrowIpc, decode_arrow_ipc);
        registry.register_decoder(SchemaFormat::ArrowJson, decode_arrow_json);
        registry.register_decoder(SchemaFormat::JsonSchema, decode_json_schema);
        registry.register_decoder(SchemaFormat::Avro, decode_avro);
        let local: SharedFileSystem = Arc::new(LocalFs::new());
        registry.register_filesystem("file", local.clone());
        registry.register_filesystem("local", local);
        #[cfg(feature = "confluent")]
        registry.register_provider("confluent", crate::confluent::ConfluentProvider);
        registry
    }
}

impl SchemaResolverRegistry {
    pub fn empty() -> Self {
        Self {
            decoders: HashMap::new(),
            providers: HashMap::new(),
            filesystems: HashMap::new(),
        }
    }

    pub fn register_decoder<F>(&mut self, format: SchemaFormat, decoder: F)
    where
        F: Fn(&[u8]) -> Result<SchemaRef> + Send + Sync + 'static,
    {
        self.decoders.insert(format, Arc::new(decoder));
    }

    pub fn register_provider<P: SchemaProvider + 'static>(&mut self, protocol: &str, provider: P) {
        self.providers
            .insert(protocol.to_lowercase(), Arc::new(provider));
    }

    pub fn register_filesystem(&mut self, protocol: &str, filesystem: SharedFileSystem) {
        self.filesystems.insert(protocol.to_lowercase(), filesystem);
    }

    pub fn resolve(&self, reference: &SchemaReference) -> Result<ResolvedSchema> {
        let protocol = reference.protocol();
        let document = match self.providers.get(&protocol) {
            Some(provider) => provider.fetch(reference)?,
            None => self.read_document(reference, &protocol)?,
        };
        if let Some(format) = reference.format {
            if format != document.format {
                return Err(schema_error(format!(
                    "schema reference requested {:?} but provider returned {:?}",
                    format.as_str(),
                    document.format.as_str()
                )));
            }
        }
        let decoder = self.decoders.get(&document.format).ok_or_else(|| {
            schema_error(format!(
                "no schema decoder registered for {:?}",
                document.format.as_str()
            ))
        })?;
        Ok(ResolvedSchema {
            schema: decoder(&document.data)?,
            provenance: SchemaProvenance {
                format: document.format.as_str().to_string(),
                ..document.provenance
            },
        })
    }

    fn read_document(&self, reference: &SchemaReference, protocol: &str) -> Result<SchemaDocument> {
        let format = match reference.format {
            Some(format) => format,
            None => SchemaFormat::from_url(&reference.url)?,
        };
        let filesystem = reference
            .filesystem
            .as_ref()
            .or_else(|| self.filesystems.get(protocol))
            .ok_or_else(|| {
                schema_error(format!(
                    "no filesystem is registered for {protocol:?}; give the reference a filesystem"
                ))
            })?;
        let data = filesystem.cat_file(&filesystem.strip_protocol(&reference.url), None, None)?;
        Ok(SchemaDocument {
            data,
            format,
            provenance: SchemaProvenance {
                source: reference.url.clone(),
                format: format.as_str().to_string(),
                provider: "fsspec".to_string(),
                identifier: None,
                version: None,
            },
        })
    }
}

pub static DEFAULT_SCHEMA_RESOLVERS: LazyLock<SchemaResolverRegistry> =
    LazyLock::new(SchemaResolverRegistry::default);

/// Builds a schema from Arrow field descriptors: `{"fields": [...]}` or a bare list of
/// `{"name", "type", "nullable"}` objects. Types use PyArrow alias names such as `int64`,
/// `string`, or `timestamp[ms]`.
pub fn schema_from_fields(value: &Value) -> Result<Schema> {
    let fields = match value {
        Value::Object(object) => object.get("fields"),
        Value::Array(_) => Some(value),
        _ => None,
    }
    .and_then(Value::as_array)
    .ok_or_else(|| schema_error("schema options must contain a sequence of fields"))?;
    fields
        .iter()
        .map(|field| {
            let field = field
                .as_object()
                .ok_or_else(|| schema_error("schema fields must be objects"))?;
            let name = field.get("name").and_then(Value::as_str);
            let data_type = field.get("type").and_then(Value::as_str);
            let (Some(name), Some(data_type)) = (name, data_type) else {
                return Err(schema_error("schema field name and type must be strings"));
            };
            let nullable = match field.get("nullable") {
                None => true,
                Some(Value::Bool(nullable)) => *nullable,
                Some(_) => return Err(schema_error("schema field nullable must be a boolean")),
            };
            Ok(Field::new(name, type_for_alias(data_type)?, nullable))
        })
        .collect::<Result<Vec<_>>>()
        .map(Schema::new)
}

fn type_for_alias(alias: &str) -> Result<DataType> {
    let unit = |unit: &str| match unit {
        "s" => Some(TimeUnit::Second),
        "ms" => Some(TimeUnit::Millisecond),
        "us" => Some(TimeUnit::Microsecond),
        "ns" => Some(TimeUnit::Nanosecond),
        _ => None,
    };
    let data_type = match alias {
        "null" => Some(DataType::Null),
        "bool" | "boolean" => Some(DataType::Boolean),
        "i1" | "int8" => Some(DataType::Int8),
        "i2" | "int16" => Some(DataType::Int16),
        "i4" | "int32" => Some(DataType::Int32),
        "i8" | "int64" => Some(DataType::Int64),
        "u1" | "uint8" => Some(DataType::UInt8),
        "u2" | "uint16" => Some(DataType::UInt16),
        "u4" | "uint32" => Some(DataType::UInt32),
        "u8" | "uint64" => Some(DataType::UInt64),
        "f2" | "halffloat" | "float16" => Some(DataType::Float16),
        "f4" | "float" | "float32" => Some(DataType::Float32),
        "f8" | "double" | "float64" => Some(DataType::Float64),
        "string" | "str" | "utf8" => Some(DataType::Utf8),
        "large_string" | "large_str" | "large_utf8" => Some(DataType::LargeUtf8),
        "string_view" => Some(DataType::Utf8View),
        "binary" => Some(DataType::Binary),
        "large_binary" => Some(DataType::LargeBinary),
        "binary_view" => Some(DataType::BinaryView),
        "date32" | "date32[day]" => Some(DataType::Date32),
        "date64" | "date64[ms]" => Some(DataType::Date64),
        "time32[s]" => Some(DataType::Time32(TimeUnit::Second)),
        "time32[ms]" => Some(DataType::Time32(TimeUnit::Millisecond)),
        "time64[us]" => Some(DataType::Time64(TimeUnit::Microsecond)),
        "time64[ns]" => Some(DataType::Time64(TimeUnit::Nanosecond)),
        "month_day_nano_interval" => Some(DataType::Interval(IntervalUnit::MonthDayNano)),
        _ => alias
            .strip_prefix("timestamp[")
            .and_then(|rest| rest.strip_suffix(']'))
            .and_then(unit)
            .map(|unit| DataType::Timestamp(unit, None))
            .or_else(|| {
                alias
                    .strip_prefix("duration[")
                    .and_then(|rest| rest.strip_suffix(']'))
                    .and_then(unit)
                    .map(DataType::Duration)
            }),
    };
    data_type.ok_or_else(|| schema_error(format!("unknown Arrow type alias {alias:?}")))
}

fn decode_arrow_ipc(data: &[u8]) -> Result<SchemaRef> {
    Ok(StreamReader::try_new(Cursor::new(data), None)?.schema())
}

fn decode_arrow_json(data: &[u8]) -> Result<SchemaRef> {
    let value: Value = serde_json::from_slice(data)?;
    if !value.is_object() {
        return Err(schema_error("Arrow JSON schema must contain an object"));
    }
    Ok(Arc::new(schema_from_fields(&value)?))
}

fn decode_json_schema(data: &[u8]) -> Result<SchemaRef> {
    Ok(Arc::new(json_schema_to_arrow(&serde_json::from_slice(
        data,
    )?)?))
}

fn decode_avro(data: &[u8]) -> Result<SchemaRef> {
    let schema =
        std::str::from_utf8(data).map_err(|_| schema_error("Avro schema must be UTF-8 JSON"))?;
    avro_arrow_schema(schema)
}

pub(crate) fn schema_error(message: impl Into<String>) -> InterchangeError {
    InterchangeError::SchemaReference(message.into())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use arrow::ipc::writer::StreamWriter;
    use fsspec_rs::{FileInfo, FsError, FsFile, FsResult, OpenMode, OpenOptions};

    use super::*;

    fn people() -> Schema {
        Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
        ])
    }

    fn ipc_schema(schema: &Schema) -> Vec<u8> {
        let mut data = Vec::new();
        StreamWriter::try_new(&mut data, schema).unwrap();
        data
    }

    /// Serves fixed files and records the paths it is asked to read.
    struct MemoryFs {
        files: HashMap<String, Vec<u8>>,
        reads: Mutex<Vec<String>>,
    }

    impl MemoryFs {
        fn new(files: &[(&str, Vec<u8>)]) -> Arc<Self> {
            Arc::new(Self {
                files: files
                    .iter()
                    .map(|(path, data)| (path.to_string(), data.clone()))
                    .collect(),
                reads: Mutex::new(Vec::new()),
            })
        }
    }

    impl FileSystem for MemoryFs {
        fn protocol(&self) -> &[&str] {
            &["memory"]
        }
        fn ls(&self, _: &str, _: bool) -> FsResult<Vec<FileInfo>> {
            unimplemented!()
        }
        fn rm_file(&self, _: &str) -> FsResult<()> {
            unimplemented!()
        }
        fn cp_file(&self, _: &str, _: &str) -> FsResult<()> {
            unimplemented!()
        }
        fn open(&self, _: &str, _: OpenMode, _: Option<OpenOptions>) -> FsResult<Box<dyn FsFile>> {
            unimplemented!()
        }
        fn info(&self, _: &str) -> FsResult<FileInfo> {
            unimplemented!()
        }
        fn mkdir(&self, _: &str, _: bool) -> FsResult<()> {
            unimplemented!()
        }
        fn rmdir(&self, _: &str) -> FsResult<()> {
            unimplemented!()
        }
        fn cat_file(&self, path: &str, _: Option<i64>, _: Option<i64>) -> FsResult<Vec<u8>> {
            self.reads.lock().unwrap().push(path.to_string());
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| FsError::NotFound(path.to_string()))
        }
    }

    #[test]
    fn resolves_local_arrow_ipc_schema_files() {
        let directory = std::env::temp_dir().join(format!("fsspec-data-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("people.arrowschema");
        std::fs::write(&path, ipc_schema(&people())).unwrap();
        let url = format!("file://{}", path.display());

        let resolved = DEFAULT_SCHEMA_RESOLVERS
            .resolve(&SchemaReference::new(url.clone()))
            .unwrap();

        assert_eq!(resolved.schema.as_ref(), &people());
        assert_eq!(
            resolved.provenance,
            SchemaProvenance {
                source: url,
                format: "arrow-ipc-schema".to_string(),
                provider: "fsspec".to_string(),
                identifier: None,
                version: None,
            }
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn references_read_through_their_own_filesystems() {
        let fields = br#"{"fields": [{"name": "id", "type": "int64", "nullable": false}, {"name": "name", "type": "string"}]}"#;
        let provided = MemoryFs::new(&[("schemas/provided", fields.to_vec())]);
        let requested = MemoryFs::new(&[("schemas/requested", ipc_schema(&people()))]);
        let mut registry = SchemaResolverRegistry::default();
        registry.register_filesystem("memory", requested.clone());

        let first = registry
            .resolve(
                &SchemaReference::new("memory://schemas/provided")
                    .with_format(SchemaFormat::ArrowJson)
                    .with_filesystem(provided.clone()),
            )
            .unwrap();
        let second = registry
            .resolve(
                &SchemaReference::new("memory://schemas/requested")
                    .with_format(SchemaFormat::ArrowIpc),
            )
            .unwrap();

        assert_eq!(first.schema.as_ref(), &people());
        assert_eq!(second.schema.as_ref(), &people());
        assert_eq!(*provided.reads.lock().unwrap(), ["schemas/provided"]);
        assert_eq!(*requested.reads.lock().unwrap(), ["schemas/requested"]);
    }

    #[test]
    fn providers_supply_documents_for_their_protocol() {
        let mut registry = SchemaResolverRegistry::default();
        registry.register_provider("registry", |reference: &SchemaReference| {
            Ok(SchemaDocument {
                data: br#"{"type": "record", "name": "Person", "fields": [{"name": "id", "type": "long"}]}"#.to_vec(),
                format: SchemaFormat::Avro,
                provenance: SchemaProvenance {
                    source: reference.url.clone(),
                    format: String::new(),
                    provider: "registry".to_string(),
                    identifier: Some("7".to_string()),
                    version: reference.options.get("version").cloned(),
                },
            })
        });

        let resolved = registry
            .resolve(&SchemaReference::new("registry://people").with_option("version", "3"))
            .unwrap();

        assert_eq!(resolved.schema.field(0).name(), "id");
        assert_eq!(resolved.schema.field(0).data_type(), &DataType::Int64);
        assert_eq!(resolved.provenance.format, "avro-schema");
        assert_eq!(resolved.provenance.identifier.as_deref(), Some("7"));
        assert_eq!(resolved.provenance.version.as_deref(), Some("3"));

        let error = registry
            .resolve(
                &SchemaReference::new("registry://people").with_format(SchemaFormat::JsonSchema),
            )
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("provider returned \"avro-schema\""));
    }

    #[test]
    fn resolution_errors_name_the_missing_piece() {
        let registry = SchemaResolverRegistry::empty();
        let cases = [
            (SchemaReference::new("people.json"), "cannot be inferred"),
            (
                SchemaReference::new("s3://bucket/people.avsc"),
                "no filesystem is registered for \"s3\"",
            ),
        ];
        for (reference, message) in cases {
            let error = registry.resolve(&reference).unwrap_err().to_string();
            assert!(error.contains(message), "{error}");
        }

        let memory = MemoryFs::new(&[("people.avsc", b"{}".to_vec())]);
        let error = registry
            .resolve(&SchemaReference::new("memory://people.avsc").with_filesystem(memory))
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("no schema decoder registered for \"avro-schema\""));
        assert!(SchemaFormat::parse("xml").is_err());
        assert!(!format!(
            "{:?}",
            SchemaReference::new("x").with_option("password", "secret")
        )
        .contains("secret"));
    }

    #[test]
    fn field_descriptors_use_pyarrow_aliases() {
        let value = serde_json::json!([
            {"name": "a", "type": "timestamp[us]"},
            {"name": "b", "type": "duration[ns]", "nullable": false},
            {"name": "c", "type": "large_binary"},
        ]);
        let schema = schema_from_fields(&value).unwrap();
        assert_eq!(
            schema.field(0).data_type(),
            &DataType::Timestamp(TimeUnit::Microsecond, None)
        );
        assert_eq!(
            schema.field(1),
            &Field::new("b", DataType::Duration(TimeUnit::Nanosecond), false)
        );
        assert_eq!(schema.field(2).data_type(), &DataType::LargeBinary);

        for (value, message) in [
            (serde_json::json!("fields"), "sequence of fields"),
            (serde_json::json!([1]), "must be objects"),
            (serde_json::json!([{"name": "a"}]), "must be strings"),
            (
                serde_json::json!([{"name": "a", "type": "decimal"}]),
                "unknown Arrow type alias",
            ),
            (
                serde_json::json!([{"name": "a", "type": "int8", "nullable": 1}]),
                "must be a boolean",
            ),
            (
                serde_json::json!([{"name": "a", "type": "timestamp[m]"}]),
                "alias",
            ),
        ] {
            let error = schema_from_fields(&value).unwrap_err().to_string();
            assert!(error.contains(message), "{error}");
        }
        assert!(decode_arrow_json(b"[]").is_err());
        assert!(decode_avro(&[0xff]).is_err());
    }
}
