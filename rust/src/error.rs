use thiserror::Error;

#[derive(Debug, Error)]
pub enum InterchangeError {
    #[error("unknown data format: {0}")]
    UnknownFormat(String),
    #[error("no codec is registered for format: {0}")]
    CodecNotRegistered(String),
    #[error("codec does not support resumable encoding: {0}")]
    CodecWriterNotSupported(String),
    #[error("decoding {0} requires an Arrow schema")]
    DecodeSchemaRequired(String),
    #[error("schema policy is not implemented: {0}")]
    UnsupportedPolicy(String),
    #[error("format conversion is not implemented: {provided} to {requested}")]
    UnsupportedFormat { provided: String, requested: String },
    #[error("exact policy requires {provided} fields, requested schema has {requested}")]
    FieldCount { provided: usize, requested: usize },
    #[error("field {index} name mismatch: provided {provided:?}, requested {requested:?}")]
    FieldName {
        index: usize,
        provided: String,
        requested: String,
    },
    #[error("requested field not found: {0}")]
    MissingField(String),
    #[error("requested field is ambiguous in provided schema: {0}")]
    AmbiguousField(String),
    #[error("field {field:?} type mismatch: provided {provided}, requested {requested}")]
    TypeMismatch {
        field: String,
        provided: String,
        requested: String,
    },
    #[error("field {0:?} cannot narrow nullable input to non-nullable output")]
    Nullability(String),
    #[error(
        "field {field:?} requires an unsafe cast for compatible policy: {provided} to {requested}"
    )]
    UnsafeCast {
        field: String,
        provided: String,
        requested: String,
    },
    #[error("field {field:?} has no registered coercion: {provided} to {requested}")]
    UnsupportedCast {
        field: String,
        provided: String,
        requested: String,
    },
    #[error("field {0:?} contains nulls required by the requested schema")]
    RuntimeNullability(String),
    #[error("field {field:?} nullability mismatch: provided {provided}, requested {requested}")]
    ExactNullability {
        field: String,
        provided: bool,
        requested: bool,
    },
    #[error("exact policy does not accept renames, defaults, drops, or casts")]
    ExactOperations,
    #[error("provided field not found: {0}")]
    MissingProvidedField(String),
    #[error("provided field {0:?} is both renamed and dropped")]
    RenameDropConflict(String),
    #[error("{operation} names a field that is not in the requested schema: {field}")]
    UnknownRequestedField {
        operation: &'static str,
        field: String,
    },
    #[error("requested field {0:?} has both a source field and a default")]
    DefaultConflict(String),
    #[error("provided field {0:?} is not used; drop it explicitly")]
    UnmappedField(String),
    #[error("default for field {0:?} must contain exactly one value")]
    DefaultLength(String),
    #[error("default for non-nullable field {0:?} is null")]
    NullDefault(String),
    #[error("default for field {field:?} cannot be cast to {requested}: {source}")]
    DefaultCast {
        field: String,
        requested: String,
        source: arrow::error::ArrowError,
    },
    #[error("field {field:?} requires a {cast} cast, but only {allowed} is allowed")]
    CastNotAllowed {
        field: String,
        cast: &'static str,
        allowed: &'static str,
    },
    #[error("unknown cast kind: {0}")]
    UnknownCastKind(String),
    #[error("input schema does not match the schema used to create the plan")]
    InputSchema,
    #[error("batch size must be greater than zero")]
    InvalidBatchSize,
    #[error("batch stream was cancelled")]
    StreamCancelled,
    #[error("decoded batch memory exceeds byte limit {limit}: attempted {attempted}")]
    ByteLimitExceeded { limit: usize, attempted: usize },
    #[error("schema reference: {0}")]
    SchemaReference(String),
    #[error("JSON Schema: {0}")]
    JsonSchema(String),
    #[error("converter types must be non-empty strings")]
    ConverterType,
    #[error("converter from {source_type:?} to {target_type:?} is already registered")]
    ConverterRegistered {
        source_type: String,
        target_type: String,
    },
    #[error("no converter registered from {source_type:?} to {target_type:?}")]
    ConverterNotRegistered {
        source_type: String,
        target_type: String,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    FileSystem(#[from] fsspec_rs::FsError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Arrow(#[from] arrow::error::ArrowError),
    #[error(transparent)]
    Parquet(#[from] parquet::errors::ParquetError),
}

pub type Result<T> = std::result::Result<T, InterchangeError>;
