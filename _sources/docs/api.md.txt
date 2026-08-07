# Interchange API reference

## `DataFileSystem`

Read-only chained filesystem registered as the `fsspec-data` protocol. The outer path names
the requested representation; `fo` names the source object on the target filesystem.

Constructor parameters:

- `fo: str`: source object path or URL.
- `target_protocol: str | None`: protocol used to construct the target filesystem.
- `target_options: dict | None`: options passed to the target filesystem.
- `fs: AbstractFileSystem | None`: existing target filesystem. Mutually exclusive with
  `target_protocol`.
- `provided_format: DataFormat | str | None`: source format. Inferred from `fo` when omitted.
- `requested_format: DataFormat | str | None`: output format. Inferred from the opened path
  when omitted.
- `provided_schema`: source `pyarrow.Schema`, nested schema options, or schema reference.
  Required for CSV and JSONL.
- `requested_schema`: output `pyarrow.Schema`, nested schema options, or schema reference.
  Defaults to the source schema.
- `schema_policy: SchemaPolicy | str`: reconciliation policy. Defaults to `exact`.
- `batch_size: int`: maximum rows per decoded batch. Defaults to `1024`.
- `row_limit: int | None`: maximum decoded rows.
- `byte_limit: int | None`: maximum cumulative Arrow array memory in decoded batches.
- `spool_max_size: int`: converted bytes retained in memory before the seekable output rolls
  over to a temporary file. Defaults to 8 MiB.

Nested schema options have a `fields` list. Each field has `name`, `type`, and optional
`nullable` keys. String types use PyArrow aliases such as `int64`, `string`, and
`timestamp[ms]`.

A schema reference is a `SchemaRef`, URL string, or mapping with `url`, optional `format`, and
optional `storage_options`. Reference storage options apply only to that schema source.
Built-in formats are `arrow-json` for nested field descriptors, `arrow-ipc-schema` for a
serialized Arrow schema or IPC stream, and `json-schema` for the supported structural JSON
Schema subset. `.arrow`, `.ipc`, and `.arrowschema` infer `arrow-ipc-schema`; ambiguous
extensions require an explicit format.

The `confluent://SUBJECT/versions/VERSION` provider accepts `latest`, `-1`, or a positive
32-bit integer as `VERSION`, and these `storage_options`:

- `registry_url: str`: required HTTP or HTTPS Schema Registry base URL.
- `username: str` and `password: str`: optional Basic authentication pair.
- `headers: Mapping[str, str]`: optional additional request headers.
- `timeout: int | float`: optional request timeout in seconds.

The provider supports registry documents with `schemaType` `JSON` and no external schema
references. Avro, Protobuf, and referenced documents raise `ValueError`.

After conversion starts, `provided_schema_provenance` and `requested_schema_provenance`
describe resolved schema sources without exposing storage options.

Recognized suffixes are `.arrow` and `.ipc` for Arrow IPC streams, `.parquet` and `.pq` for
Parquet, `.csv` for CSV, and `.jsonl` and `.ndjson` for line-delimited JSON.

`open` accepts read mode and returns a seekable spooled file. Conversion currently reads the
complete encoded source and produces a complete encoded output before returning the file.

See [How to convert a file through an fsspec chain](how-to-chain-filesystems.md) for usage.

## `SchemaRef`

Identifies an independently resolvable schema.

Attributes:

- `url: str`: schema location.
- `format: SchemaFormat | str | None`: schema representation. Inferred for recognized Arrow
  schema suffixes.
- `storage_options: Mapping[str, Any]`: options passed only to the schema filesystem. Omitted
  from `repr`.

## `ResolvedSchema`

Contains the resolved `pyarrow.Schema` and `SchemaProvenance`. Resolution performs I/O before
interchange planning; the planner continues to receive only Arrow schemas.

## `SchemaProvenance`

Describes a resolved schema without credentials.

Attributes:

- `source: str`: original schema reference URL or `inline`.
- `format: str`: decoded schema representation.
- `provider: str`: `inline`, `fsspec`, `confluent`, or a registered provider name.
- `identifier: str | None`: provider-specific schema identifier.
- `version: str | None`: provider-specific schema version.

## `SchemaDocument`

Contains provider-returned schema bytes, their `SchemaFormat`, and `SchemaProvenance`.

## `SchemaResolverRegistry`

Separates schema transport providers from schema-language decoders.

### `register(format, decoder)`

Registers a decoder accepting schema `bytes` and returning `pyarrow.Schema`.

### `register_provider(protocol, provider)`

Registers a provider accepting `SchemaRef` and returning `SchemaDocument`.

`DEFAULT_SCHEMA_RESOLVERS` includes decoders for `arrow-json`, `arrow-ipc-schema`, and
`json-schema`; a `confluent` provider; and an fsspec provider fallback for other protocols.

Use `resolve_schema` to resolve an inline schema or normalized `SchemaRef` directly.

## `json_schema_to_arrow`

Converts the supported JSON Schema subset directly to `pyarrow.Schema`. The root must have
type `object`. Supported value types are object, array with one `items` schema, string,
boolean, integer as Arrow `int64`, number as Arrow `float64`, and null. A type array may contain
null and one non-null type.

Object `required` entries and null unions determine Arrow field nullability. References,
composition, multiple non-null union members, tuple arrays, schema-valued additional
properties, and `format` annotations raise `ValueError` with a schema path. JSON Schema
validation constraints are not represented or enforced by Arrow.

## `Converter`

Describes one native conversion route.

Attributes:

- `source_type: str`: normalized, case-insensitive source type.
- `target_type: str`: normalized, case-insensitive target type.
- `handler: Callable`: conversion function, omitted from `repr`.

The handler signature is:

```python
handler(
    source,
    target,
    *,
    source_options,
    target_options,
    conversion_options,
)
```

All option arguments are dictionaries. The handler return value is the converter result.

### `convert(source, target=None, *, source_options=None, target_options=None, conversion_options=None)`

Copies the three option mappings and calls the handler. Raises `TypeError` when an option
value is not a mapping.

## `ConverterRegistry`

Maps normalized `(source_type, target_type)` pairs to `Converter` objects. Installed converter
entry points are loaded at most once, on first lookup.

Constructor parameters:

- `entry_point_group: str`: discovery group. Defaults to `fsspec_data.converters`.
- `discover_entry_points: bool`: enables installed entry-point discovery. Defaults to `True`.

### `register(converter, *, replace=False)`

Registers a `Converter`. Raises `ValueError` for a duplicate route unless `replace=True`.

### `load_entry_points()`

Loads entry points from the configured group. Each entry point must load one `Converter`.
Duplicate routes and invalid objects raise errors before discovered converters are added.

### `get(source_type, target_type)`

Returns the converter for a route. Raises `ValueError` when no converter is registered.

### `convert(source_type, target_type, source, target=None, **options)`

Gets the route and calls `Converter.convert`.

## `DEFAULT_CONVERTERS`

Default `ConverterRegistry`. Built-in routes are `zarr` to `xarray` and `xarray` to `zarr`.
They import Xarray only when invoked and require the `fsspec-data[xarray]` optional dependency.

The Zarr-to-Xarray route calls `xarray.open_zarr`. `source_options` become its
`storage_options`; `conversion_options` supply options such as `group` and `chunks`. It returns
an Xarray `Dataset` and rejects a target or target options.

The Xarray-to-Zarr route accepts an Xarray `Dataset` or `DataArray` and calls `to_zarr`.
`target_options` become its `storage_options`; `conversion_options` supply options such as
`mode`, `group`, and `zarr_format`.

## `DataFormat`

Identifies an interchange encoding. Values are `arrow`, `parquet`, `csv`, and `jsonl`.
`arrow` denotes an Arrow IPC stream. `jsonl` denotes line-delimited JSON records.

## `SchemaPolicy`

Controls schema reconciliation.

| Value        | Behavior                                                                 |
| ------------ | ------------------------------------------------------------------------ |
| `exact`      | Requires equal names, order, Arrow types, and nullability.               |
| `projection` | Selects or reorders fields without casts or nullable narrowing.          |
| `compatible` | Preserves field order and permits lossless casts and nullable widening.  |
| `coerce`     | Preserves field order and applies casts registered by the coercion core. |

Compatible casts are signed and unsigned integer widening, unsigned-to-signed integer casts
where the target has more bits, float widening, UTF-8 to large UTF-8, date32 to date64, and
null to a requested type. Coercion additionally supports primitive numeric conversions,
large UTF-8 to UTF-8, and conversions between primitive numeric or boolean types and
strings.

Coercions are classified as `safe`, `lossy`, or `runtime_checked`. Nullable-to-required
coercion performs a runtime null check. Nested-type coercions are not registered.

## `FieldMapping`

Describes one output field.

Attributes:

- `source_index: int`: index in the provided schema.
- `target_index: int`: index in the requested schema.
- `cast: str | None`: `safe`, `lossy`, `runtime_checked`, or `None`.
- `check_nulls: bool`: whether execution must reject null input values.

## `CodecCapabilities`

Describes operations supported by a codec.

Attributes:

- `encode: bool`: accepts Arrow record batches and produces encoded bytes.
- `decode: bool`: accepts encoded bytes and produces Arrow record batches.
- `streaming: bool`: consumes and produces record batches without requiring an Arrow table.

## `DecodedBatches`

Contains buffered decoded output.

Attributes:

- `schema: pyarrow.Schema`: decoded schema.
- `batches: tuple[pyarrow.RecordBatch, ...]`: decoded batches in order.

## `DecodedBatchStream`

Iterates lazily over decoded `pyarrow.RecordBatch` values.

Attributes:

- `schema: pyarrow.Schema`: decoded schema.

Methods:

### `cancel()`

Cancels the stream. Its next iteration raises `RuntimeError`; later iterations stop.

### `collect()`

Consumes the stream and returns `DecodedBatches`.

## `Codec`

Represents a registered format codec.

Attributes:

- `format: DataFormat`: encoding handled by the codec.
- `capabilities: CodecCapabilities`: supported operations.

### `encode_batches(batches, *, schema=None)`

Returns encoded `bytes` for Arrow record batches. `schema` is required when `batches` is
empty. All batches must match the encoding schema.

Raises `TypeError` for non-Arrow inputs and `ValueError` for missing or mismatched schemas.

### `encode_batches_to(batches, output, *, schema=None)`

Consumes an iterable of Arrow record batches and writes encoded data to a binary file-like
`output`. Returns `None` and leaves `output` open. `schema` is required when `batches` is
empty. All batches must match the encoding schema.

Raises `TypeError` for non-Arrow batches or a non-schema `schema`, `ValueError` for missing
or mismatched schemas, and `OSError` when the output writer fails.

### `iter_batches(data, *, schema=None, batch_size=1024, row_limit=None, byte_limit=None)`

Returns `DecodedBatchStream`.

Parameters:

- `data: bytes | bytearray | memoryview | BinaryIO`: encoded input. File-like inputs must
  return `bytes` from `read`; Parquet inputs must also support `seek`.
- `schema: pyarrow.Schema | None`: required for CSV and JSONL; optional for Arrow IPC and
  Parquet.
- `batch_size: int`: maximum rows yielded in one batch. Must be greater than zero.
- `row_limit: int | None`: maximum total rows yielded. The last batch is sliced as needed.
- `byte_limit: int | None`: maximum cumulative Arrow array memory referenced by yielded
  batches. Exceeding it raises `ValueError`.

### `decode_batches(...)`

Calls `iter_batches(...)`, consumes the stream, and returns `DecodedBatches`. Parameters and
errors match `iter_batches`.

## Rust `CodecReader`

A marker trait for reader-backed encoded input. Implementations provide `Read + Seek + Send`.

`Codec::decode_reader` accepts a boxed `CodecReader`. The built-in Arrow IPC, Parquet, CSV,
and JSONL codecs decode directly from the reader. The default implementation for other
codecs buffers the reader and calls `decode_stream`, preserving compatibility with existing
`Codec` implementations.

Parquet decoding uses seekable range reads because file metadata and column chunks may be
located at different offsets.

## Rust `CodecWriter`

A resumable encoded-output session returned by `Codec::start_writer`.

Methods:

- `write_batch(&mut self, batch: &RecordBatch)`: validates the batch against the session
  schema and submits it to the format writer.
- `finish(self: Box<Self>)`: writes the format footer and consumes the session. The borrowed
  sink remains owned by the caller.

Arrow IPC, Parquet, CSV, and JSONL codecs support resumable writers.

Arrow IPC makes encoded batch bytes available to the sink during `write_batch`. The Parquet
writer accepts batches incrementally but may buffer its output until `finish`.

`Codec::encode_stream` uses the same writer session for all four codecs.

Python `Codec.encode_batches_to` uses an owned writer session backed by the supplied binary
file object.

### `Codec::start_writer(schema, output)`

Borrows a `Write + Send` sink and returns a writer session with the same lifetime. This form
supports scoped encoding while preserving direct access to the sink after `finish`.

### `Codec::start_owned_writer(schema, output)`

Takes a boxed `Write + Send` sink and returns a writer session without a borrowed lifetime.
This form supports storing the writer in a long-lived file or stream object. Shared or
cloneable sinks allow the caller to observe output while the session owns its sink handle.

## `CodecRegistry`

### `get(format)`

Returns the registered `Codec` for a `DataFormat` or its string value. Raises `ValueError`
for unknown values and `NotImplementedError` when a known format has no registered codec.

`DEFAULT_REGISTRY` contains Arrow IPC stream, Parquet, CSV, and JSONL codecs.

Arrow IPC preserves input batch boundaries up to `batch_size`. Parquet preserves schema and
row order but may select different output batch boundaries. CSV writes a header row. CSV
and JSONL require an explicit decode schema because neither encoding stores a complete
Arrow schema.

## `InterchangeRequest`

Describes one planned conversion.

Attributes:

- `provided_format: DataFormat`: source encoding.
- `requested_format: DataFormat`: target encoding.
- `provided_schema: pyarrow.Schema`: source schema.
- `requested_schema: pyarrow.Schema`: target schema.
- `policy: SchemaPolicy`: reconciliation policy.

### `plan()`

Validates registered source and target codecs and returns `InterchangePlan`. Planning reads
no data.

## `InterchangePlan`

Represents a validated format and schema conversion.

Attributes:

- `provided_schema: pyarrow.Schema`: schema accepted as input.
- `requested_schema: pyarrow.Schema`: schema produced as output.
- `policy: SchemaPolicy`: applied schema policy.
- `mappings: tuple[FieldMapping, ...]`: ordered field mappings.
- `provided_format: DataFormat`: source encoding.
- `requested_format: DataFormat`: target encoding.

### `apply_table(table)`

Returns a `pyarrow.Table` with mappings, casts, and null checks applied.

### `apply_batch(batch)`

Returns a `pyarrow.RecordBatch` with mappings, casts, and null checks applied.

Both methods raise `ValueError` when the input schema differs from `provided_schema`, a
runtime null check fails, or a runtime-checked cast fails.

### `iter_batches(data, *, batch_size=1024, row_limit=None, byte_limit=None)`

Returns `PlannedBatchStream`. It decodes `provided_format` lazily and applies the schema plan
to each batch. Limits apply to decoded input batches.

### `convert(data, *, batch_size=1024, row_limit=None, byte_limit=None)`

Consumes `iter_batches`, encodes `requested_format`, and returns `bytes`. This method is an
explicit buffering adapter.

## `PlannedBatchStream`

Iterates lazily over planned `pyarrow.RecordBatch` values. `schema` is the requested schema.
`cancel()` cancels the underlying decoded stream.

## `plan_schema(provided_schema, requested_schema, policy)`

Returns an Arrow-to-Arrow `InterchangePlan` without reading data.

Raises `TypeError` for non-PyArrow schemas and `ValueError` when schemas violate the selected
policy or require an unregistered cast.

```python
import pyarrow as pa

from fsspec_data import SchemaPolicy, plan_schema

provided = pa.schema([pa.field("id", pa.int32(), nullable=False)])
requested = pa.schema([pa.field("id", pa.int64(), nullable=True)])
plan = plan_schema(provided, requested, SchemaPolicy.COMPATIBLE)
```
