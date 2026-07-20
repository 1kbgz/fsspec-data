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
    #[error("input schema does not match the schema used to create the plan")]
    InputSchema,
    #[error("batch size must be greater than zero")]
    InvalidBatchSize,
    #[error("batch stream was cancelled")]
    StreamCancelled,
    #[error("decoded batch memory exceeds byte limit {limit}: attempted {attempted}")]
    ByteLimitExceeded { limit: usize, attempted: usize },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Arrow(#[from] arrow::error::ArrowError),
    #[error(transparent)]
    Parquet(#[from] parquet::errors::ParquetError),
}

pub type Result<T> = std::result::Result<T, InterchangeError>;
