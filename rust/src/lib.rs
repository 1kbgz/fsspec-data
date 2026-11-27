mod async_parquet;
mod avro;
pub mod cleanup;
mod codec;
#[cfg(feature = "confluent")]
mod confluent;
mod converter;
mod describe;
mod error;
mod fingerprint;
mod json_schema;
pub mod manifest;
#[cfg(test)]
mod parquet_range_tests;
mod parquet_ranges;
pub mod parts;
mod plan;
mod scan_identity;
mod schema;
pub mod sink;
mod stream;
mod table_scan;
pub mod watch;

pub use async_parquet::{AsyncDecodedStream, AsyncScanOptions};
pub use avro::{avro_arrow_schema, AvroCodec, AvroFraming, AvroValueDecoder, AvroValueEncoder};
pub use codec::{
    ArrowIpcCodec, Codec, CodecCapabilities, CodecReader, CodecRegistry, CodecWriter, CsvCodec,
    DecodedBatches, JsonLinesCodec, ParquetCodec, DEFAULT_REGISTRY,
};
#[cfg(feature = "confluent")]
pub use confluent::{
    fetch_confluent_schema, fetch_confluent_versions, ConfluentProvider, ConfluentSchemaDocument,
};
pub use converter::{ConversionEndpoint, Converter, ConverterRegistry};
pub use describe::{DataSource, Facet, FileDataSource, Metadata, Node};
pub use error::{InterchangeError, Result};
pub use json_schema::json_schema_to_arrow;
pub use plan::{
    plan, plan_field_descriptors, plan_schema, plan_schema_with, schema_ref, CastKind,
    CastRegistry, DataFormat, FieldAddition, FieldDescriptor, FieldMapping, InterchangePlan,
    InterchangeRequest, PlannedStream, ProjectionOptions, SchemaPolicy, DEFAULT_CAST_REGISTRY,
};
pub use scan_identity::{ScanIdentity, SourceIdentity};
pub use schema::{
    schema_from_fields, ResolvedSchema, SchemaDecoder, SchemaDocument, SchemaFormat,
    SchemaProvenance, SchemaProvider, SchemaReference, SchemaResolverRegistry, SharedFileSystem,
    DEFAULT_SCHEMA_RESOLVERS,
};
pub use stream::{CancellationToken, DecodedStream, ScanSelection, StreamOptions};
