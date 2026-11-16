//! The description contract between a backend and the data views built on it.
//!
//! A backend describes nodes in its own namespace. Each node offers some [`Facet`]s, and a
//! consumer reads a facet through the matching [`DataSource`] method. fsspec-data renders
//! facets as files; the backend never knows which forms are requested.

use std::collections::BTreeSet;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

use arrow::datatypes::SchemaRef;
use fsspec_rs::{FileInfo, FileType, OpenMode, SharedFileSystem};
use serde_json::{Map, Value};

use crate::{
    CancellationToken, CodecReader, DataFormat, DecodedStream, InterchangeError, Result,
    StreamOptions, DEFAULT_REGISTRY,
};

/// Metadata attached to a node.
pub type Metadata = Map<String, Value>;

/// Something a node offers.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum Facet {
    /// A container; [`DataSource::children`] lists it.
    Children,
    /// Rows; [`DataSource::schema`] and [`DataSource::scan`] read them.
    Table,
    /// Native bytes; [`DataSource::open_raw`] reads them.
    Raw,
    /// A JSON object; [`DataSource::metadata`] reads it.
    Metadata,
}

impl Facet {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Children => "children",
            Self::Table => "table",
            Self::Raw => "raw",
            Self::Metadata => "metadata",
        }
    }
}

/// A node in a backend's namespace and the facets it offers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Node {
    /// The node's path in the backend's namespace.
    pub path: String,
    pub facets: BTreeSet<Facet>,
    /// The encoding of the raw bytes when they hold the node's rows, such as a CSV object.
    pub native_format: Option<DataFormat>,
}

impl Node {
    pub fn new(path: impl Into<String>, facets: impl IntoIterator<Item = Facet>) -> Self {
        Self {
            path: path.into(),
            facets: facets.into_iter().collect(),
            native_format: None,
        }
    }

    pub fn with_native_format(mut self, format: DataFormat) -> Self {
        self.native_format = Some(format);
        self
    }

    pub fn has(&self, facet: Facet) -> bool {
        self.facets.contains(&facet)
    }
}

/// A backend that describes its nodes. Methods for facets a backend never offers keep
/// their defaults, which return [`InterchangeError::FacetNotSupported`].
pub trait DataSource: Send + Sync {
    /// Describes the node at `path`, including paths that no listing returns.
    fn describe(&self, path: &str) -> Result<Node>;

    /// The children of a [`Facet::Children`] node. Encoded aliases of other nodes are left
    /// out, so a view does not show the same rows twice.
    fn children(&self, path: &str) -> Result<Vec<Node>> {
        Err(not_supported(path, Facet::Children))
    }

    /// The Arrow schema of a [`Facet::Table`] node.
    fn schema(&self, path: &str) -> Result<SchemaRef> {
        Err(not_supported(path, Facet::Table))
    }

    /// The rows of a [`Facet::Table`] node.
    fn scan(
        &self,
        path: &str,
        options: StreamOptions,
        cancellation: CancellationToken,
    ) -> Result<DecodedStream> {
        let _ = (options, cancellation);
        Err(not_supported(path, Facet::Table))
    }

    /// The bytes of a [`Facet::Raw`] node.
    fn open_raw(&self, path: &str) -> Result<Box<dyn CodecReader>> {
        Err(not_supported(path, Facet::Raw))
    }

    /// The metadata of a [`Facet::Metadata`] node.
    fn metadata(&self, path: &str) -> Result<Metadata> {
        Err(not_supported(path, Facet::Metadata))
    }
}

fn not_supported(path: &str, facet: Facet) -> InterchangeError {
    InterchangeError::FacetNotSupported {
        path: path.to_string(),
        facet: facet.as_str(),
    }
}

/// Describes any filesystem: directories are containers, and files with a recognized data
/// suffix (`.csv`, `.parquet`, ...) are tables whose raw bytes are their native encoding.
/// Every node has metadata from its file information.
pub struct FileDataSource {
    fs: SharedFileSystem,
    /// Bytes read from the start of a CSV or JSONL file to infer its schema.
    pub sample_size: usize,
}

impl FileDataSource {
    pub fn new(fs: SharedFileSystem) -> Self {
        Self {
            fs,
            sample_size: 1024 * 1024,
        }
    }

    fn node(&self, info: &FileInfo) -> Node {
        let path = self.fs.strip_protocol(&info.name);
        if info.file_type == FileType::Directory {
            return Node::new(path, [Facet::Children, Facet::Metadata]);
        }
        match DataFormat::from_path(&path) {
            Some(format) => Node::new(path, [Facet::Table, Facet::Raw, Facet::Metadata])
                .with_native_format(format),
            None => Node::new(path, [Facet::Raw, Facet::Metadata]),
        }
    }

    fn table_format(&self, path: &str) -> Result<DataFormat> {
        let node = self.describe(path)?;
        node.native_format
            .ok_or_else(|| not_supported(path, Facet::Table))
    }

    /// The schema to decode `path` with: inferred from a sample for formats without one.
    fn decode_schema(&self, path: &str, format: DataFormat) -> Result<Option<SchemaRef>> {
        if !matches!(format, DataFormat::Csv | DataFormat::JsonLines) {
            return Ok(None);
        }
        let mut sample = Vec::new();
        let mut file = self.fs.open(path, OpenMode::Read, None)?;
        let read = file
            .by_ref()
            .take(self.sample_size as u64 + 1)
            .read_to_end(&mut sample)?;
        let truncated = read > self.sample_size;
        sample.truncate(self.sample_size);
        let schema = DEFAULT_REGISTRY
            .get(format)?
            .infer_schema(&sample, None, truncated)?;
        Ok(Some(schema))
    }
}

impl DataSource for FileDataSource {
    fn describe(&self, path: &str) -> Result<Node> {
        Ok(self.node(&self.fs.info(path)?))
    }

    fn children(&self, path: &str) -> Result<Vec<Node>> {
        let mut nodes: Vec<_> = self
            .fs
            .ls(path, true)?
            .iter()
            .map(|info| self.node(info))
            .collect();
        nodes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(nodes)
    }

    fn schema(&self, path: &str) -> Result<SchemaRef> {
        let format = self.table_format(path)?;
        if let Some(schema) = self.decode_schema(path, format)? {
            return Ok(schema);
        }
        Ok(self
            .scan(path, StreamOptions::default(), CancellationToken::new())?
            .schema)
    }

    fn scan(
        &self,
        path: &str,
        options: StreamOptions,
        cancellation: CancellationToken,
    ) -> Result<DecodedStream> {
        let format = self.table_format(path)?;
        let schema = self.decode_schema(path, format)?;
        DEFAULT_REGISTRY.get(format)?.decode_reader(
            self.open_raw(path)?,
            schema,
            options,
            cancellation,
        )
    }

    fn open_raw(&self, path: &str) -> Result<Box<dyn CodecReader>> {
        Ok(Box::new(self.fs.open(path, OpenMode::Read, None)?))
    }

    fn metadata(&self, path: &str) -> Result<Metadata> {
        let info = self.fs.info(path)?;
        let mut metadata = Metadata::new();
        if info.file_type != FileType::Directory {
            metadata.insert("size".into(), info.size.into());
        }
        for (key, time) in [("created", info.created), ("mtime", info.modified)] {
            if let Some(seconds) = time.and_then(epoch_seconds) {
                metadata.insert(key.into(), seconds.into());
            }
        }
        for (key, value) in info.extra {
            metadata.entry(key).or_insert(Value::String(value));
        }
        Ok(metadata)
    }
}

fn epoch_seconds(time: SystemTime) -> Option<f64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use arrow::array::{Int64Array, RecordBatch, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use fsspec_rs::{FileSystem, MemoryFs};
    use serde_json::json;

    use super::*;
    use crate::Codec;

    fn files() -> FileDataSource {
        let fs = MemoryFs::new();
        fs.pipe_file("/data/orders.csv", b"id,customer\n1,a\n2,b\n")
            .unwrap();
        fs.pipe_file("/data/notes.txt", b"hello").unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![7, 8]))])
                .unwrap();
        let parquet = crate::ParquetCodec.encode(schema, &[batch]).unwrap();
        fs.pipe_file("/data/nested/ids.parquet", &parquet).unwrap();
        FileDataSource::new(Arc::new(fs))
    }

    fn rows(stream: DecodedStream) -> usize {
        stream
            .collect_batches()
            .unwrap()
            .iter()
            .map(RecordBatch::num_rows)
            .sum()
    }

    #[test]
    fn files_describe_directories_tables_and_other_files() {
        let source = files();
        let children = source.children("/data").unwrap();
        let summary: Vec<_> = children
            .iter()
            .map(|node| (node.path.as_str(), node.facets.clone(), node.native_format))
            .collect();
        assert_eq!(
            summary,
            [
                (
                    "/data/nested",
                    BTreeSet::from([Facet::Children, Facet::Metadata]),
                    None
                ),
                (
                    "/data/notes.txt",
                    BTreeSet::from([Facet::Raw, Facet::Metadata]),
                    None
                ),
                (
                    "/data/orders.csv",
                    BTreeSet::from([Facet::Table, Facet::Raw, Facet::Metadata]),
                    Some(DataFormat::Csv)
                ),
            ]
        );
        assert_eq!(source.describe("/data/orders.csv").unwrap(), children[2]);
        assert_eq!(source.metadata("/data/notes.txt").unwrap()["size"], 5);
    }

    #[test]
    fn files_read_tables_by_native_format() {
        let source = files();
        let csv = source.schema("/data/orders.csv").unwrap();
        assert_eq!(csv.field(0).name(), "id");
        assert_eq!(csv.field(1).data_type(), &DataType::Utf8);
        let scan = |path| {
            source
                .scan(path, StreamOptions::default(), CancellationToken::new())
                .unwrap()
        };
        assert_eq!(rows(scan("/data/orders.csv")), 2);

        let parquet = source.schema("/data/nested/ids.parquet").unwrap();
        assert_eq!(parquet.field(0).name(), "id");
        assert_eq!(rows(scan("/data/nested/ids.parquet")), 2);

        assert!(matches!(
            source.schema("/data/notes.txt"),
            Err(InterchangeError::FacetNotSupported { facet: "table", .. })
        ));
        let mut raw = String::new();
        source
            .open_raw("/data/notes.txt")
            .unwrap()
            .read_to_string(&mut raw)
            .unwrap();
        assert_eq!(raw, "hello");
    }

    #[test]
    fn csv_schemas_are_inferred_from_complete_sample_records() {
        let mut source = files();
        source.sample_size = 14;
        let schema = source.schema("/data/orders.csv").unwrap();
        assert_eq!(schema.fields().len(), 2);
    }

    /// A data catalog: `category/team/datasets/{name}` datasets with tags at every level.
    struct Catalog {
        tags: BTreeMap<&'static str, Value>,
    }

    impl Catalog {
        fn new() -> Self {
            Self {
                tags: BTreeMap::from([
                    ("", json!({"owner": "platform"})),
                    ("finance", json!({"steward": "cfo"})),
                    ("finance/rates", json!({"team": "rates"})),
                    ("finance/rates/datasets", json!({})),
                    ("finance/rates/datasets/curves", json!({"refresh": "daily"})),
                ]),
            }
        }

        fn schema() -> SchemaRef {
            Arc::new(Schema::new(vec![Field::new(
                "tenor",
                DataType::Utf8,
                false,
            )]))
        }
    }

    impl DataSource for Catalog {
        fn describe(&self, path: &str) -> Result<Node> {
            let path = path.trim_matches('/');
            if !self.tags.contains_key(path) {
                return Err(fsspec_rs::FsError::NotFound(path.into()).into());
            }
            Ok(if path.ends_with("datasets/curves") {
                Node::new(path, [Facet::Table, Facet::Metadata])
            } else {
                Node::new(path, [Facet::Children, Facet::Metadata])
            })
        }

        fn children(&self, path: &str) -> Result<Vec<Node>> {
            let path = path.trim_matches('/');
            self.tags
                .keys()
                .filter(|key| {
                    !key.is_empty() && key.rsplit_once('/').map_or("", |(parent, _)| parent) == path
                })
                .map(|key| self.describe(key))
                .collect()
        }

        fn schema(&self, _path: &str) -> Result<SchemaRef> {
            Ok(Self::schema())
        }

        fn scan(
            &self,
            _path: &str,
            options: StreamOptions,
            cancellation: CancellationToken,
        ) -> Result<DecodedStream> {
            let batch = RecordBatch::try_new(
                Self::schema(),
                vec![Arc::new(StringArray::from(vec!["1y", "5y", "10y"]))],
            )?;
            DecodedStream::new(
                Self::schema(),
                Box::new(std::iter::once(Ok(batch))),
                options,
                cancellation,
            )
        }

        fn metadata(&self, path: &str) -> Result<Metadata> {
            match &self.tags[path.trim_matches('/')] {
                Value::Object(tags) => Ok(tags.clone()),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn a_catalog_backend_describes_metadata_at_every_level() {
        let catalog = Catalog::new();
        let root = catalog.describe("/").unwrap();
        assert!(root.has(Facet::Children) && root.has(Facet::Metadata));
        assert_eq!(catalog.metadata("/").unwrap()["owner"], "platform");

        let datasets = catalog.children("finance/rates/datasets").unwrap();
        assert_eq!(datasets.len(), 1);
        let curves = &datasets[0];
        assert!(curves.has(Facet::Table) && !curves.has(Facet::Raw));
        assert_eq!(catalog.metadata(&curves.path).unwrap()["refresh"], "daily");
        let stream = catalog
            .scan(
                &curves.path,
                StreamOptions::default(),
                CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(rows(stream), 3);
        assert!(matches!(
            catalog.open_raw(&curves.path),
            Err(InterchangeError::FacetNotSupported { facet: "raw", .. })
        ));
    }
}
