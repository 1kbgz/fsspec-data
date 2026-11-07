mod avro;
mod codec;
mod error;
mod plan;
mod stream;

pub use avro::{avro_arrow_schema, AvroCodec, AvroFraming, AvroValueDecoder};
pub use codec::{
    ArrowIpcCodec, Codec, CodecCapabilities, CodecReader, CodecRegistry, CodecWriter, CsvCodec,
    DecodedBatches, JsonLinesCodec, ParquetCodec, DEFAULT_REGISTRY,
};
pub use error::{InterchangeError, Result};
pub use plan::{
    plan, plan_field_descriptors, plan_schema, plan_schema_with, schema_ref, CastKind,
    CastRegistry, DataFormat, FieldAddition, FieldDescriptor, FieldMapping, InterchangePlan,
    InterchangeRequest, ProjectionOptions, SchemaPolicy, DEFAULT_CAST_REGISTRY,
};
pub use stream::{CancellationToken, DecodedStream, StreamOptions};
