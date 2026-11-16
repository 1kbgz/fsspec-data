# Interchange API reference

## `DataViewFileSystem`

Read-only view of a [data source](#data-sources) registered as the `dataview` protocol. It
renders each node's facets as files, so ordinary fsspec tools and mounts can read a backend's
rows and metadata. Open a view with an outer path and the wrapped URL, such as
`dataview://orders.parquet::s3://bucket/data`, or construct it directly.

Constructor parameters:

- `fo: str`: the root of the view on the wrapped filesystem: a directory, or a single
  object whose renderings make up the view.
- `target_protocol: str | None` and `target_options: dict | None`: build the wrapped
  filesystem.
- `fs: AbstractFileSystem | None`: an existing wrapped filesystem. If it is a `DataSource`,
  the view uses its `describe`; otherwise it wraps it in `FileDataSource`.
- `source: DataSource | None`: a data source to view directly instead of a filesystem.
- `mapping`: the forms to show for each facet. Defaults to `DEFAULT_MAPPING`.
- `provided_schema`: the schema to decode CSV and JSONL sources with instead of inferring
  one, and to check self-describing sources against: a `pyarrow.Schema`, nested schema
  options, or a schema reference.
- `requested_schema`: the output schema, in the same forms. Defaults to the source schema.
- `schema_policy: SchemaPolicy | str`: reconciliation policy. Defaults to `exact`.
- `renames`, `defaults`, `drop`, `casts`: field operations passed to
  [`plan_schema`](#plan_schemaprovided_schema-requested_schema-policy--renamesnone-defaultsnone-dropnone-castsnone).
- `batch_size: int`, `row_limit: int | None`, `byte_limit: int | None`: limits on the
  batches read from each table. `batch_size` defaults to `1024`.
- `spool_max_size: int`: rendered bytes kept in memory before spilling to a temporary file.
  Defaults to 8 MiB.

The schema options apply to every table the view converts, and a table's schema file shows
the converted schema. With any of them set, a conversion to the source's own format replaces
the native file of the same name. After a table is converted, `provided_schema_provenance`
and `requested_schema_provenance` describe the resolved schema sources without exposing
storage options.

Nested schema options have a `fields` list. Each field has `name`, `type`, and optional
`nullable` keys. String types use PyArrow aliases such as `int64`, `string`, and
`timestamp[ms]`. Schema references can be `SchemaRef` values, URL strings, or mappings with
`url`, optional `format`, and optional `storage_options`; see
[Schema and converter reference](resources.md).

`fs`, `target_protocol`, and `source` are mutually exclusive.

`DEFAULT_MAPPING` is
`{"table": ["raw", "parquet"], "raw": ["raw"], "metadata": ["json"], "schema": ["json"], "index": ["json"]}`.
A mapping shows only the facets it names. For a node named `N` (a table's native suffix is
dropped, so `orders.csv` gives `orders`):

| Facet      | Forms                                                     | Rendered as                                       |
| ---------- | --------------------------------------------------------- | ------------------------------------------------- |
| children   | always                                                    | directory `N/`                                    |
| `table`    | `raw`, `arrow`, `parquet`, `csv`, `jsonl`, `avro`, `json` | `N.parquet`, ...; `raw` keeps the native file     |
| `raw`      | `raw`                                                     | the file unchanged, for nodes that are not tables |
| `metadata` | `json`                                                    | `N.metadata.json`                                 |
| `schema`   | `json`                                                    | `N.schema.json`, for tables                       |
| `index`    | the table forms except `raw`                              | `N.json`, ..., for containers that are not tables |

A table format equal to the node's native format is not repeated when `raw` is listed. The
root's metadata and index use an empty name: `.metadata.json` and `.json`. An index has one
row per child with its `name` and metadata. A schema file holds `fields`, each with `name`,
PyArrow `type`, and `nullable`, the same shape a requested schema accepts. `json` forms are
indented JSON; a table rendered as `json` is a list of row objects.

Two nodes that render the same name in one directory raise `ValueError` when the directory is
listed. A name that no listing returns is resolved by describing the node it would come from,
so unlisted backend paths still open.

`ls` reports `None` as the size of files that are not rendered yet; `info` renders a file to
find its size. Listings are cached on the instance until `invalidate_cache()`; instances are
not shared between calls.

### `resolve(path)`

Returns the `ViewEntry` a view path renders: `name`, the source `node`, `facet` (`children`,
`table`, `raw`, `metadata`, `schema`, or `index`), `form` (`dir`, `raw`, or a format), and
`url`, the node's URL on the wrapped filesystem when the view wraps one.

## `DataFormat`

Identifies an interchange encoding. Values are `arrow`, `parquet`, `csv`, `jsonl`, and
`avro`. `arrow` denotes an Arrow IPC stream. `jsonl` denotes line-delimited JSON records.
`avro` denotes an Avro object container file.

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

## `FieldAddition`

Describes one output field filled with a constant instead of a provided field.

Attributes:

- `target_index: int`: index in the requested schema.
- `value: pyarrow.Array`: one value of the requested field type, or one null.

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

### `infer_schema(sample, *, max_records=None, truncated=False)`

Returns a `pyarrow.Schema` inferred from the start of an encoded source, for decoding CSV or
JSONL without a provided schema. `max_records` limits how many records are read. Set
`truncated` when `sample` is the first bytes of a larger source; inference then ignores
the partial record after the last newline.

CSV inference reads a header row and infers integers, floats, booleans, dates, timestamps,
and strings. JSONL inference also infers lists and nested objects. Later records may not
fit an inferred schema, in which case decoding raises `ValueError`. Other formats raise
`ValueError`; Arrow IPC and Parquet carry their own schema. Rust exposes the same operation
as `Codec::infer_schema(sample, max_records, truncated)`.

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

Arrow IPC, Parquet, CSV, JSONL, and Avro codecs support resumable writers.

Arrow IPC makes encoded batch bytes available to the sink during `write_batch`. The Parquet
writer accepts batches incrementally but may buffer its output until `finish`.

`Codec::encode_stream` uses the same writer session for all five codecs.

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

`DEFAULT_REGISTRY` contains Arrow IPC stream, Parquet, CSV, JSONL, and Avro codecs.

Arrow IPC preserves input batch boundaries up to `batch_size`. Parquet preserves schema and
row order but may select different output batch boundaries. CSV writes a header row. CSV
and JSONL require an explicit decode schema because neither encoding stores a complete
Arrow schema. Avro files carry their schema in the file header.

## Rust Avro values

`AvroValueDecoder` decodes individual Avro values, such as message payloads, rather than
container files. `AvroValueDecoder::new(schema, framing, batch_size)` takes the reader
schema as Avro JSON. `AvroFraming::Unframed` expects bare datums written with that schema;
`AvroFraming::Confluent(writers)` expects the Confluent wire format and holds the writer
schema for each schema ID. Values written with older writer schemas resolve to the reader
schema. `push(value)` decodes exactly one value, and `flush()` returns every row pushed
since the previous flush.

`AvroValueEncoder::new(schema, schema_id)` encodes each row of a batch as one value. With a
schema ID the values use the Confluent wire format; without one they are bare datums.
`schema()` returns the Arrow schema rows must have, which is also the schema values decode to.

`avro_arrow_schema(schema)` returns the Arrow schema that values with an Avro schema decode
to. Python exposes it through the `avro-schema` schema format.

## `InterchangeRequest`

Describes one planned conversion.

Attributes:

- `provided_format: DataFormat`: source encoding.
- `requested_format: DataFormat`: target encoding.
- `provided_schema`: source schema, as a PyArrow schema or a schema reference.
- `requested_schema`: target schema, as a PyArrow schema or a schema reference.
- `policy: SchemaPolicy`: reconciliation policy.
- `renames`, `defaults`, `drop`, `casts`: optional field operations passed to `plan_schema`.
- `schema_resolvers: SchemaResolverRegistry | None`: resolvers for schema references.
  `DEFAULT_SCHEMA_RESOLVERS` is used when it is `None`.

Schema references are `SchemaRef` values, URL strings, or mappings with `url`, `format`, and
`storage_options`, as accepted by `DataViewFileSystem`.

### `plan()`

Resolves schema references, validates registered source and target codecs, and returns
`InterchangePlan`. Resolution may read the schema documents; planning reads no source data.

## `InterchangePlan`

Represents a validated format and schema conversion.

Attributes:

- `provided_schema: pyarrow.Schema`: schema accepted as input.
- `requested_schema: pyarrow.Schema`: schema produced as output.
- `policy: SchemaPolicy`: applied schema policy.
- `mappings: tuple[FieldMapping, ...]`: ordered field mappings.
- `additions: tuple[FieldAddition, ...]`: requested fields filled with constants.
- `provided_format: DataFormat`: source encoding.
- `requested_format: DataFormat`: target encoding.
- `provided_schema_provenance`, `requested_schema_provenance`: `SchemaProvenance` of each
  schema when the plan came from `InterchangeRequest.plan()`, otherwise `None`.

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

## `plan_schema(provided_schema, requested_schema, policy, *, renames=None, defaults=None, drop=None, casts=None)`

Returns an Arrow-to-Arrow `InterchangePlan` without reading data.

Field operations:

- `renames: Mapping[str, str]`: provided field name to requested field name.
- `defaults: Mapping[str, Any]`: requested field name to a constant. The constant is cast to
  the requested type while planning. `None` adds an all-null field, which must be nullable.
- `drop: Sequence[str]`: provided fields to remove.
- `casts: Mapping[str, str]`: requested field name to the most permissive cast allowed for
  that field, in order `safe`, `runtime_checked`, `lossy`. It replaces the policy's limit
  for that field: `compatible` allows `safe`, `coerce` allows `lossy`, and `projection`
  allows no cast.

Without field operations, fields are matched as described for each policy. With any field
operation, fields are matched by name after renames, every provided field must be used or
listed in `drop` unless the policy is `projection`, and `exact` is rejected. A requested
field with both a matching provided field and a default is an error.

Raises `TypeError` for non-PyArrow schemas and `ValueError` when schemas violate the selected
policy, require an unregistered or disallowed cast, or field operations conflict.

```python
import pyarrow as pa

from fsspec_data import SchemaPolicy, plan_schema

provided = pa.schema([pa.field("id", pa.int32(), nullable=False)])
requested = pa.schema([pa.field("id", pa.int64(), nullable=True)])
plan = plan_schema(provided, requested, SchemaPolicy.COMPATIBLE)
```

```python
provided = pa.schema([pa.field("customer_number", pa.int64()), pa.field("legacy", pa.bool_())])
requested = pa.schema([pa.field("customer_id", pa.int64()), pa.field("region", pa.string())])
plan = plan_schema(
    provided,
    requested,
    "compatible",
    renames={"customer_number": "customer_id"},
    defaults={"region": "us"},
    drop=["legacy"],
)
```

## Data sources

`DataSource` is the contract a backend implements so its nodes can be rendered as files. It
exists as a Python base class and a Rust trait with the same methods. `describe(path)`
returns a `Node` with its `path`, a set of `Facet`s, and a `native_format` when the node's raw
bytes already encode its rows. `describe` also resolves paths that no listing returns.

| Facet      | Method            | Returns                                                      |
| ---------- | ----------------- | ------------------------------------------------------------ |
| `CHILDREN` | `children(path)`  | child `Node`s, without encoded aliases                       |
| `TABLE`    | `schema(path)`    | the Arrow schema                                             |
| `TABLE`    | `scan(path, ...)` | a `DecodedBatchStream` (Rust: `DecodedStream`)               |
| `RAW`      | `open_raw(path)`  | a binary file object (Rust: `CodecReader`) over native bytes |
| `METADATA` | `metadata(path)`  | a JSON-compatible mapping (Rust: `Metadata`)                 |

Any node may offer metadata, including directories and the root. A method for a facet the
node does not offer raises `FacetNotSupportedError` (Rust: returns
`InterchangeError::FacetNotSupported`). Rust facet names are `Children`, `Table`, `Raw`, and
`Metadata`. Python `scan` takes keyword arguments `batch_size`, `row_limit`, and
`byte_limit`; Rust `scan` takes `StreamOptions` and a `CancellationToken`.

`FileDataSource(fs, sample_size=1048576)` describes any fsspec filesystem (Rust:
`FileDataSource::new(fs)` over an `fsspec_rs` filesystem, with a public `sample_size` field).
Directories have `CHILDREN` and `METADATA`. Files have `RAW` and `METADATA`, plus `TABLE` when
`DataFormat.from_path` recognizes the suffix. CSV and JSONL schemas are inferred from the
first `sample_size` bytes. Python metadata is the filesystem's `info()` without `name` and
`type`, and without `size` for directories; values that are not JSON scalars become strings.
Rust metadata holds `size` for files, `created` and `mtime` in seconds since the Unix epoch
when known, and the filesystem's extra fields as strings.

`DataFormat.from_path(path)` (Rust: `DataFormat::from_path`) maps `.arrow`, `.ipc`,
`.parquet`, `.pq`, `.csv`, `.jsonl`, `.ndjson`, and `.avro` to a format and returns `None` for
anything else.
