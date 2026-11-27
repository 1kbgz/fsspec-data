# Interchange API reference

## `DataViewFileSystem`

View of a [data source](#data-sources) registered as the `dataview` protocol. It renders
each node's facets as files, so ordinary fsspec tools and mounts can read a backend's rows and
metadata. Open a view with an outer path and the wrapped URL, such as
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
the converted schema. `converts` is true when any schema option is set. With any of them set, a conversion to the source's own format replaces
the native file of the same name. After a table is converted, `provided_schema_provenance`
and `requested_schema_provenance` describe the resolved schema sources without exposing
storage options.

Raw files are writable when the view has a wrapped filesystem. Opening a raw file with
`"wb"`, `"ab"`, or `"xb"` opens the node's path on the wrapped filesystem, and `rm_file`
removes it. A new name under a directory of the view maps to the same name under that
directory's node, including parents the view does not list, such as a new key prefix in a
`kafka+records` topic; the wrapped filesystem decides whether it can create them. Closing a
written file clears the view's cached listings and sizes. Writing or removing a rendered file
(a conversion, metadata, schema, or index) raises `PermissionError`, which a mount reports as
"Permission denied". A view built from a `source` alone is read-only.

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

### `iter_batches(data, *, schema=None, batch_size=1024, row_limit=None, byte_limit=None, columns=None, row_offset=0)`

Returns `DecodedBatchStream`.

Parameters:

- `data: bytes | bytearray | memoryview | BinaryIO`: encoded input. File-like inputs must
  return `bytes` from `read`; Parquet inputs must also support `seek`.
- `schema: pyarrow.Schema | None`: required for CSV and JSONL; optional for Arrow IPC and
  Parquet.
- `batch_size: int`: maximum rows yielded in one batch. Must be greater than zero.
- `row_offset: int`: nonnegative number of source rows to skip before applying output limits.
- `row_limit: int | None`: maximum total rows yielded. The last batch is sliced as needed.
- `byte_limit: int | None`: maximum cumulative Arrow array memory referenced by yielded
  batches. Exceeding it raises `ValueError`.

`columns: Sequence[str] | None` selects top-level fields by name. `None` returns all
fields; an empty sequence returns zero-column batches with their row counts preserved.
Requested order and duplicate names are preserved, including nested fields selected by
their top-level name. Unknown names raise `ValueError`. Schema and field metadata are
preserved. Parquet reads only the selected column chunks; other codecs project decoded
batches. Row and byte limits apply after skipping rows and projecting columns. An offset
at or beyond the end, or a zero row limit, returns an empty stream with its selected schema.

Parquet selects only row groups intersecting the requested row interval, then applies the
remaining offset and limit inside the decoder. Metadata is still read; excluded row groups
and column chunks are not read. Boundary row groups may read complete column chunks. Other
formats skip decoded rows. This selection does not include predicate or page-index pruning.

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

`Codec::decode_reader_columns(reader, schema, columns, options, cancellation)` accepts a
`&[String]` of top-level names with the same selection semantics as Python `columns`.
The default implementation projects decoded batches. The Parquet implementation selects
column chunks before decoding. Existing `decode_reader` implementations remain valid.

`Codec::decode_reader_selected(reader, schema, &selection, options, cancellation)` adds
row-offset selection. `ScanSelection { columns: Option<Vec<String>>, row_offset: usize }`
defaults to all columns and offset zero. `StreamOptions.row_limit` limits rows after the
offset. Existing codecs inherit a fallback through `decode_reader` or
`decode_reader_columns`; Parquet prunes row groups and applies offset/limit while decoding.
Both existing Parquet entry points also push their row limit into the decoder.

`DecodedStream::new_with_offset(schema, source, options, cancellation, row_offset)`
skips decoded rows before enforcing output limits. Cancellation is checked while skipping;
skipped rows do not count toward row or byte limits.

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

`scan_columns(path, columns, *, batch_size=1024, row_limit=None, byte_limit=None)` returns
selected top-level columns in requested order, including duplicates. Empty selections
preserve row counts; unknown names raise `ValueError`. The default calls the backend's
existing `scan` and projects batches before enforcing output limits. `FileDataSource`
pushes Parquet selection into decoding. Rust exposes
`DataSource::scan_columns(path, columns, options, cancellation)`, where `columns` is
`&[String]`; existing `scan` and `StreamOptions` signatures are unchanged.

`scan_selected(path, *, columns=None, row_offset=0, batch_size=1024, row_limit=None, byte_limit=None)` combines column selection with a zero-based row offset. The limit applies
after the offset, and cancellation propagates to the source. Existing Python backends
inherit a fallback through their current `scan` or `scan_columns`; they do not need to
accept a new keyword argument. `FileDataSource` delegates selection to the codec.

Rust exposes `DataSource::scan_selected(path, &ScanSelection, options, cancellation)` with
the same behavior and a default implementation for existing backends.

```python
stream = source.scan_selected(
    "events.parquet", columns=["timestamp", "value"], row_offset=100_000, row_limit=1_000
)
```

`FileDataSource(fs, sample_size=1048576)` describes any fsspec filesystem (Rust:
`FileDataSource::new(fs)` over an `fsspec_rs` filesystem, with a public `sample_size` field).
Directories have `CHILDREN` and `METADATA`. Files have `RAW` and `METADATA`, plus `TABLE` when
`DataFormat.from_path` recognizes the suffix. CSV and JSONL schemas are inferred from the
first `sample_size` bytes. Python metadata is the filesystem's `info()` without `name` and
`type`, and without `size` for directories; values that are not JSON scalars become strings.
Rust metadata holds `size` for files, `created` and `mtime` in seconds since the Unix epoch
when known, and the filesystem's extra fields as strings.

Parquet scans read footer metadata by suffix, then fetch selected column chunks together
for each selected row group. No data chunks are fetched until iteration starts. Column
order, duplicate columns, row offsets and output limits retain their scan semantics.
Coalescing may also fetch bytes in gaps between selected chunks. Output byte limits do
not bound compressed row-group buffers.

Rust file sources use `FileSystem::read_ranges`. Python file sources use the typed
`read_ranges` API when available, otherwise fsspec's `cat_ranges(..., on_error="raise")`.
With a validator from the first footer read, subsequent footer and data requests use
`Match` against that validator. Native S3 therefore rejects object changes during a scan;
Python preserves the filesystem's `ConflictError`, and Rust returns
`InterchangeError::Operation`. Results already yielded before a later conflict remain
visible to the caller. Sources without validators provide no snapshot guarantee.

Fetching remains synchronous to callers. Parquet's batch-range interface delegates
request concurrency and size limits to the filesystem. Cancellation stops subsequent
fetches and batches; it does not interrupt a synchronous request already in progress.
`Codec.iter_batches` with a file-like object retains its existing seekable-reader path.

`DataFormat.from_path(path)` (Rust: `DataFormat::from_path`) maps `.arrow`, `.ipc`,
`.parquet`, `.pq`, `.csv`, `.jsonl`, `.ndjson`, and `.avro` to a format and returns `None` for
anything else.

## Shared sink interfaces

`DataSink` represents a configured destination for one logical table. `DataWriter` represents
an opened write session. Both names are exported from Python `fsspec_data` and the Rust crate.

Python defines structural protocols: `DataSink[Options, Writer]` and
`DataWriter[BatchReceipt, PublicationReceipt, RecoveryToken]`. Implementations need no base
class or registration. `ManifestTable` satisfies `DataSink[SinkOptions, TableWriter]`, and its
existing `TableWriter` satisfies the writer protocol. Runtime `isinstance` checks only
interface shape; they do not certify idempotency, durability, or backend correctness.

Rust defines traits with the corresponding associated types. `DataSink::open_writer(schema, mode, options)` returns the backend's writer. `ManifestTableSink::new(fs, root)` binds the
existing filesystem and table root; it implements `DataSink<Options = SinkOptions, Writer = FileDataSink>`. `FileDataSink` implements `DataWriter`; its existing constructor and inherent
methods remain available. Trait objects require explicit associated types.

Options, receipts, and recovery tokens retain their backend types. A database implementation
need not manufacture a manifest generation or checksum. Existing `SinkOptions`, `BatchReceipt`,
`PublicationReceipt`, and `RecoveryToken` describe the manifest backend; see the
[manifest table reference](manifest-tables.md) for their fields.

| Writer operation         | Contract                                                                                                                                                               |
| ------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `schema`                 | Accepted Arrow schema. Rust uses a method; Python uses a property.                                                                                                     |
| `progress_unit`          | Unit of positions, independent of output row counts. Manifest writers use `"rows"`.                                                                                    |
| `position`               | Accepted progress, including unpublished work; not a durable checkpoint.                                                                                               |
| `write_batch`            | Requires expected position and retry key. Exact retries do not duplicate data; forward positions raise gap, conflicting positions or key/payload reuse raise conflict. |
| `flush`                  | Stage pending data without publication or completion.                                                                                                                  |
| `commit`                 | Publish accepted work and keep the writer/table open.                                                                                                                  |
| `finish`                 | Publish accepted work, mark the table complete, and close the writer.                                                                                                  |
| `abort` / `close` / drop | Discard pending work. Earlier successful commits remain visible; closing never marks completion.                                                                       |
| `recovery_token`         | Backend token for the current publication attempt, obtained before commit/finish. It changes after successful publication and contains no credentials.                 |
| `resolve`                | Observe or retry the same uncertain publication. Rust takes `RecoveryAction`; Python takes `retry=False`.                                                              |

Unsupported modes/options must fail before writes. A Python writer's clean context exit
commits and closes; exceptional exit aborts. Unknown publication outcomes preserve recovery
information and must not be treated as proof that publication failed. Recovery tokens and
successful receipts have separate purposes: tokens resolve attempts, while receipts establish
committed progress. Generic code must retain the concrete writer or return its token when a
publication fails; it must not silently reopen and append the same work.

These interfaces are synchronous. Database/Kafka sink adapters, write capabilities on source
facets, schema evolution and the data-sink C ABI are not implemented by these interfaces.

## Rust manifest snapshots

`fsspec_data::manifest` provides version-one JSON manifests and a synchronous snapshot
reader. This module has no Python binding or publication API. It reads encoded parts;
row decoding and schema reconciliation remain codec operations.

### `Manifest`

`Manifest::encode()` validates and serializes a manifest. `Manifest::decode(bytes)` parses
and validates one stored document. `validate()` checks the same local invariants without
serialization. Parent comparisons require `ManifestReader`.

The version-one fields are:

| Fields                                  | Meaning                                                                                                |
| --------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| `format_version`                        | Must be `1`.                                                                                           |
| `namespace_id`, `table_id`              | Partition history and table-instance identities.                                                       |
| `generation`, `parent`                  | Unsigned generation and optional previous generation with its `sha256`. Generation zero has no parent. |
| `publication_id`, `kind`                | Publication identity and `create`, `append`, `replace`, `finish`, `delete`, or `cleanup`.              |
| `schema`, `partition_values`            | Arrow schema and a map of partition names to JSON values.                                              |
| `complete`, `deleted`                   | Completion and tombstone state.                                                                        |
| `num_rows`, `progress_unit`, `progress` | Total visible rows and separate logical write progress.                                                |
| `parts`                                 | Ordered, complete list of visible encoded parts.                                                       |
| `receipts`                              | Batch receipts introduced by this publication. Earlier receipts remain in earlier manifests.           |

The schema uses Arrow 60's serde JSON representation, including field and schema metadata.
Unknown fields in manifest, parent, part, span, and receipt objects are rejected.
Namespace, table, publication, and progress-unit identifiers contain 1–128 ASCII letters,
digits, underscores, or hyphens.

A `Part` has `key`, `size`, `sha256`, `num_rows`, `progress: {start, end}`, `encoding`, and
`options`. Keys have the form `parts/<generation>/<publication_id>/<filename>` relative to
the partition root. Generations use 20 decimal digits. Filenames contain 1–128 ASCII
letters, digits, underscores, hyphens, or periods, excluding `.` and `..`. These checks
reject absolute paths and traversal syntax; filesystem symlinks remain the caller's
responsibility. Part size must be positive. Supported encoding names are `parquet`,
`arrow`, `csv`, `jsonl`, and `avro`; options are retained as JSON values without interpretation.

Part progress spans are half-open, contiguous from zero, and end at the manifest's
`progress`. Their row counts sum to `num_rows`. With `progress_unit = "rows"`, each span's
length must equal its row count. Other units keep progress separate from rows.

A `BatchReceipt` has `key`, `progress: {start, end}`, and `fingerprint`. Keys contain
1–256 UTF-8 bytes and are unique within the document. Receipt spans cover the progress
introduced by a publication, independently of part boundaries. Cleanup publications have
no receipts. The reader does not search historical receipts for duplicate keys.

`Checksum::of(bytes)` computes SHA-256. Checksums serialize as 64 lowercase hexadecimal
digits. Parent checksums refer to the exact stored manifest bytes; equivalent JSON with
different whitespace has a different checksum. Re-encoding a parsed manifest is not a
canonical byte representation. Batch fingerprints identify logical requests and are
separate from encoded part checksums.

### `ManifestReader::new(fs, root, limits)`

Accepts a `SharedFileSystem`, partition root, and `ReadLimits`. The filesystem instance is
retained by snapshots; readers do not share a global cache or serialize credentials.

The store must retain immutable parts and a consecutive sequence of immutable manifests
starting at zero, with consistent reads of missing keys. The reader assumes these
properties; it does not certify backend durability or create-if-absent behavior.

### `ManifestReader::read(selection)`

`Selection::Generation(g)` selects that exact generation.
`Selection::Latest` probes generation keys and uses binary search to locate the newest
manifest. It rechecks the missing successor before returning. Under concurrent
publication, the snapshot was latest at that final missing-key read. Discovery uses
logarithmically many probes when no writer is publishing.

Manifest keys are `manifests/<20-digit-generation>/manifest.json` beneath the root. The
reader does not list directories or consult `_SUCCESS`. Unlisted parts are invisible.
Opening a snapshot reads metadata only. It validates generation zero, each selected or
probed manifest, and its immediate parent's checksum and state transition. It does not
audit the full historical chain or detect arbitrary gaps outside those probes.

Append and finish preserve the table identity, schema, progress unit, and existing part
prefix, and require an incomplete parent. Finish marks the table complete. Replacement
changes the table identity. Delete creates an empty tombstone with a new table identity;
create after a tombstone starts another table identity. Cleanup preserves table state.
Old parts may only be inherited unchanged from the immediate parent.

A selected tombstone returns a snapshot with `deleted = true` and no parts. Replacements
and later publications do not change snapshots already returned.

### `Snapshot`

`manifest()` returns the frozen metadata. `checksum()` returns the SHA-256 of its original
stored bytes. `read_part(index)` returns one complete encoded part after verifying its
size and SHA-256. It does not read other parts or decode rows.

### `ReadLimits` and errors

Defaults are 8 MiB for `max_manifest_bytes`, 64 MiB for `max_part_bytes`, and 256 for
`max_manifest_reads`. All must be positive. Manifest reads cannot exceed the version-one
8 MiB format limit; `max_part_bytes` must be less than `usize::MAX`. The request budget
includes parent reads and missing-key probes.

Reads consume at most the applicable encoded byte limit plus one byte to detect excess
input. These limits do not bound backend prefetching, parsed metadata memory, transport
request duration, or process-wide memory. Part limits are checked against declared size
before opening the part.

Methods return `fsspec_rs::OperationError`. Missing selected generations return `NotFound`.
Malformed metadata, invalid transitions, observed missing parents, missing referenced
parts, and part size/checksum mismatches return `Corrupt`. Unsupported format versions or
encodings return `NotSupported`. Invalid limits, oversized documents, parts exceeding the
configured limit, and invalid part indices return `Invalid`. Exhausting the discovery
request budget returns `Timeout`. Reader-generated errors have `retry_safe = false`.
Permission and transport failures propagate; they never count as missing generations.

## Rust Parquet part writer

`fsspec_data::parts::ParquetPartWriter` encodes Arrow batches into complete Parquet files.
`new(schema, limits, consume)` accepts a nonempty Arrow schema, `PartLimits`, and a
synchronous `FnMut(EncodedPart) -> Result<()>` consumer. Completed parts are passed to the
consumer in row order without copying their bytes. The writer keeps one unfinished part
and no queue of completed parts.

`EncodedPart` contains `bytes: Vec<u8>`, `num_rows: u64`, and `sha256: Checksum`. Each part
has one row group and preserves the input schema, including metadata. Encoding uses
Parquet's default compression and dictionary settings.

### Operations

- `write_batch(&RecordBatch)` accepts a batch with exactly the configured schema. It checks
  input allocation size before encoding, processes at most 1,024 rows between encoder
  memory checks, and emits parts when their estimated encoded size reaches the target or
  encoder memory reaches half the remaining budget. Empty batches emit no parts.
- `flush()` emits the current part and leaves the encoder open. Repeated flushes without
  new rows emit no files.
- `finish(self)` emits remaining rows and closes the encoder.
- `abort(self)` and dropping the writer discard the unfinished part. Neither calls the
  consumer for that part.

These operations do not publish manifests, write storage objects, or mark a table complete.
The consumer owns staging and cleanup. Previously emitted parts remain its responsibility,
including when a later batch or consumer call fails. A batch can span multiple parts; its
emission is not atomic.

### `PartLimits`

`target_part_bytes` defaults to 8 MiB. It is a rotation goal before footer overhead, not
an exact file size. A row slice can exceed the target. Memory pressure can produce smaller
parts. `memory_limit` defaults to 64 MiB. Configuration requires
`0 < target_part_bytes < memory_limit`.

The accounting budget combines the current input arrays' reported allocation sizes,
encoded output capacity, and `ArrowWriter::memory_size()`. Input accounting includes
backing buffer capacity retained by slices; shared buffers may be counted repeatedly.
Input batches are not retained between calls. A previously buffered part may be emitted
before the next input is encoded to make room within the budget.

Output growth is capped at the budget remaining after charging input and the reported
encoder estimate. The pre-flush estimate remains charged while writing the final pages
and footer. Exceeding the output limit fails without handing a partial file to the
consumer. Consumers that retain parts must enforce their own limits.

Parquet reports estimated encoder memory after allocation. Internal allocations can
exceed the budget before a check, and its estimate does not cover all schema, footer,
I/O-buffer, and temporary allocations. This API therefore limits input/output buffers
and checks estimated encoder usage; it does not guarantee a hard total-memory or RSS
limit. It does not retry failed encoding with smaller row slices.

### Errors

Schema mismatches return `InterchangeError::InputSchema`. Configuration, input limits,
encoder-budget checks, and Parquet encoding failures return an `OperationError` with
kind `Invalid` and `retry_safe = false`. Consumer errors propagate unchanged.

Schema and input-limit errors leave previously buffered rows intact. An error after
encoding starts, including a consumer or footer failure, drops the current part and
permanently fails the writer. Subsequent writes, flushes, and finish return `Invalid`.
The caller must discard the unpublished attempt's staged parts before replaying it.

## Rust file data sink

`fsspec_data::sink::FileDataSink` stages immutable Parquet parts and publishes version-one
manifests. This Rust API supports one partition per writer. It has no Python binding,
background collector, cross-partition transaction, or automatic conflict rebase.

### Opening a writer

`FileDataSink::open(fs, root, mode, schema, options)` accepts a `SharedFileSystem`, partition
root, `WriteMode`, Arrow schema, and `SinkOptions`.

- `Create` requires an unused or tombstoned path and starts a new table identity at position
  zero. Recreation advances the existing generation sequence.
- `Append` requires an incomplete table with the exact schema and `rows` progress unit.
- `Replace` requires an existing table and starts a new table identity at position zero.
  The previous table remains visible until publication succeeds.

`SinkOptions.parts` configures the Parquet encoder's `PartLimits`.
`max_manifest_bytes` defaults to 8 MiB and cannot exceed the manifest format limit.
`max_history_reads` defaults to 256 and bounds generations examined for one retry-key
lookup; validating a generation may read its parent and generation zero too.
`partition_values` defaults to an empty map and must match an existing partition.
All limits must be positive, the target part size must be below the part memory limit,
and the part memory limit must be less than `usize::MAX`.

Supported columns are Boolean, signed/unsigned integers, Float32/Float64, Utf8/LargeUtf8,
Binary/LargeBinary, Date32/Date64, Timestamp, valid Time32/Time64 units, Duration, and
Decimal128. Nested, dictionary, and other types return `NotSupported`. Schema changes
require replacement.

The source must advertise `io_capabilities().write_missing`. Correct publication also
requires atomic, durable create-if-absent, immutable objects, and consistent reads of
missing keys. The flag does not certify deployment guarantees. Local integration and
fault-injection tests do not certify AWS S3, on-premises S3, NFS, or power-loss behavior.

### Batches and positions

`write_batch(expected_position, key, batch)` returns a `BatchReceipt`. Keys contain 1–256
UTF-8 bytes; batches must be nonempty and exactly match the writer schema.

Pending and published receipts are checked before comparing positions. The same key,
position, and logical content return the original receipt without writing again. A changed
request with the same key returns `Conflict`. A new key ahead of accepted progress returns
`Gap`; one behind it returns `Conflict`. Keys belong to one table identity and remain
available through its retained manifest history.

`position()` reports accepted progress, which includes unpublished rows. Only a successful
`PublicationReceipt` authorizes checkpointing upstream progress. The receipt includes the
published generation and identity in `recovery`, the manifest `sha256`, `progress`,
`num_rows`, `complete`, and an optional `marker_error`.

Metadata capacity is checked before accepting a new receipt, reserving 1 KiB per possible
new part (one per encoder row slice, plus one previously buffered part). Reservations are
conservative and can reject a batch whose actual encoding would use less metadata.
Schema, position, key, input-limit, history-read, and metadata-preflight errors leave
pending work unchanged. Encoding or staging failure makes the writer unusable; the caller
must replay the unpublished attempt with a new writer.

### Publication and close

- `flush()` finishes and stages parts without making them visible.
- `commit()` publishes pending rows with `complete = false`, then allocates the next attempt
  and leaves the writer open. An empty commit on an existing instance returns its last
  receipt without a new generation. Initial empty Create/Replace publishes an empty table.
- `finish()` publishes with `complete = true` and closes the writer. Repeating a resolved
  finish returns its receipt.
- `abort()` and `close()` discard pending encoder state and close the writer. Earlier
  commits remain visible. Both return `UnknownOutcome` while publication is unresolved.
- Drop performs no publication or deletion. A previously submitted request may still
  complete after the writer is dropped.

Parts use `parts/<generation>/<publication_id>/<part_number>.parquet` keys. Creation is
conditional and followed by an exact-byte read check. A manifest is submitted only after
the parts and immutable attempt descriptor are stored and verified. Competing writers
use the same generation key; a different valid winner returns `Conflict`.

After a complete manifest is published, the sink writes that generation's `_SUCCESS`.
Marker failure is returned separately as `PublicationReceipt.marker_error`; publication
has already succeeded. There is no mutable root `_SUCCESS` marker.

### Recovery

`recovery_token()` returns a serializable `RecoveryToken` containing the partition root,
namespace, table identity, target generation, and publication ID. The caller must persist
it with the source configuration before commit/finish, and persist the newly allocated
token before the next publication. Tokens contain no credentials.

`resolve(action)` resumes an unresolved live writer. After restart,
`recover(fs, &token, &options, action)` uses the same source and credential context.
`RecoveryAction::Observe` checks the target without resubmitting it;
`RecoveryAction::Retry` may submit only the original bytes to the original key using
create-if-absent. Both attempt `_SUCCESS` repair after observing a completed publication.
Standalone recovery returns a publication receipt; further writes require reopening.

The immutable descriptor at `parts/<generation>/<publication_id>/candidate` contains the
ASCII header `fsspec-data/candidate/1` followed by a newline, the 64-character lowercase
SHA-256 and another newline, then the exact manifest JSON bytes. Recovery checks the hash,
identity, parent, and newly staged parts before resubmission. It checks for a different
winner before loading a possibly collected losing descriptor.

Missing data after an ambiguous publication request returns `UnknownOutcome`, since a
request can complete later. The writer freezes its candidate and rejects new batches,
commit/finish, and abort until resolved. An ambiguous descriptor write submits no manifest
and fails the writer; that writer can close safely. Its persisted descriptor, if it later
appears, can only publish through an explicit recovery retry.

### Fingerprint version one

Batch fingerprints hash logical values with SHA-256 before Parquet encoding. The byte
protocol starts with `fsspec-data/batch/1` and a NUL byte, then the expected position,
length-prefixed UTF-8 `rows`, row count, and column count. Integers used as counts, lengths,
and positions are unsigned 64-bit little-endian.

Schema metadata comes next, followed by each field in schema order: length-prefixed UTF-8
name, length-prefixed Arrow 60 serde JSON scalar type (compact JSON with UTF-8 strings),
one nullability byte, and field metadata. Metadata is a count followed by key/value
UTF-8 byte strings sorted by key, each with a length prefix.

Values follow column by column, in row order. Each has a validity byte: zero for null,
one for valid. Nulls have no payload. Booleans use one byte; numeric, temporal, decimal,
and floating-point values use their native-width little-endian representation. Floating
point retains signed zero and NaN payload bits. Strings and binary values use a 64-bit
length followed by their bytes. Array offsets, unused null storage, and buffer padding
are excluded. Rust and independent Python reference code check shared fixtures in
`rust/tests/fixtures/batch_fingerprints.json`.

### Limits and retained objects

Part encoding retains the estimated-memory limitation documented for `ParquetPartWriter`.
Manifest and descriptor reads are bounded individually. Storage adapters may allocate
additional upload/readback buffers; this API does not impose a hard total-memory limit
across the encoder, metadata copies, and backend. Historical receipt searches have a
read budget and no process-global cache.

Abort, failure, and recovery never delete parts or descriptors. Published history remains
readable. Reclaiming orphan attempts requires a separate collector that proves their
publication slots are permanently occupied and observes retention. Generic automatic
retries are disabled for sink-generated errors (`retry_safe = false`).

## Rust asynchronous Parquet scans

`ParquetCodec::decode_async(file, selection, options, cancellation).await` returns an
`AsyncDecodedStream` implementing `futures::Stream<Item = Result<RecordBatch>>`. This is a
Rust API; Python `DataSource` scans remain synchronous.

The method takes ownership of a seekable `fsspec_rs::AsyncFsFile`. It supports the core
async handles and C ABI handles wrapped by `fsspec_sys`. The provider must preserve a
consistent object across reads; native fsspec-rs S3 handles pin the ETag and size.
An object replacement returns the provider's structured conflict error.

`ScanSelection` selects top-level columns, including requested order and duplicates,
and a source row offset. `AsyncScanOptions::stream` supplies batch size, output row limit,
and cumulative output byte limit. Selection skips unneeded row groups and column chunks.
Schema metadata is preserved, including for an empty column selection.

Additional `AsyncScanOptions` fields:

| Field             | Default | Meaning                                                                                                                                                 |
| ----------------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `max_range_bytes` | 64 MiB  | Aggregate compressed ranges requested for one row group or footer; checked before allocation or I/O. Larger requests fail with `RangeLimitExceeded`.    |
| `max_batch_bytes` | 64 MiB  | Maximum projected output batch; checked after decoding with Arrow's `get_array_memory_size`. Larger batches fail with `ByteLimitExceeded`.              |
| `read_chunk_size` | 1 MiB   | Maximum individual file read. Must fit the provider's read limit and byte budget. Native S3 reserves twice this amount for result and staging buffers.  |
| `deadline`        | `None`  | Absolute `std::time::Instant` covering metadata loading, reads, decoding and idle time between consumer polls. Expiry returns `StreamDeadlineExceeded`. |

Each scan uses one worker thread for Parquet metadata parsing and batch decoding. File I/O
uses the provider's async implementation and runtime. The caller needs no Tokio runtime.
One consumer request produces at most one batch; the worker does not prefetch another
batch while the consumer is idle. Separate scans do not share reads or buffers.

Compressed row-group data, parsed metadata, a temporary file-read buffer and a decoded
batch can coexist. These limits are not a total memory cap. Parquet's internal allocations,
provider transport allocations, and batches retained by callers are outside these limits.
The batch limit detects excessive output after allocation; it does not prevent it.

`CancellationToken::cancel()` wakes pending reads and idle scans. Cancellation drops the
pending I/O future and returns `StreamCancelled`; decoding already running on the worker
finishes its current poll before the cancellation or deadline is observed. No subsequent
batch is delivered after cancellation is observed.

EOF, errors, cancellation and dropping the stream close the owned file. Dropping the
initialization future also requests cleanup. `stream.close().await` cancels the scan and
waits for provider cleanup, including after EOF or a scan error. Cleanup is not cut short
by the scan deadline and can wait for the provider. Normal EOF waits for cleanup and
reports a close error; an earlier scan error takes precedence during iteration.

## Python asynchronous Parquet scans

`await scan_parquet_async(url, **options)` returns an `AsyncDecodedBatchStream` with an
Arrow `schema` and async iteration yielding `pyarrow.RecordBatch` values. This API reads
`s3://bucket/key` and `s3-rs://bucket/key` through native fsspec-rs handles by default.
Passing `fs=` uses that configured Python filesystem's `open_async` instead.

`await source.scan_async(path, **options)` provides the same selection, limits and stream
contract on `DataSource`. The base implementation raises `NotImplementedError`.
`FileDataSource` supports Parquet paths through its filesystem's `open_async`; it does not
call synchronous `describe`, `info` or `open`. Other source implementations can override
`scan_async` to resolve their own table paths.

Options:

| Parameter         | Default | Meaning                                                                                                                                                                                          |
| ----------------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `fs`              | `None`  | Configured filesystem with an async `open_async(path, mode="rb")`. Mutually exclusive with `storage_options`; supplied by `FileDataSource`.                                                      |
| `storage_options` | `None`  | Flat fsspec-rs S3 options, such as `anon`, `region`, `endpoint_url`, `key`, `secret` and `token`. Values are strings, booleans or numbers. Environment and credential defaults follow fsspec-rs. |
| `columns`         | `None`  | Top-level column names in output order. Duplicates and an empty selection are supported.                                                                                                         |
| `row_offset`      | `0`     | Source rows to skip before applying output limits.                                                                                                                                               |
| `batch_size`      | `1024`  | Maximum rows per batch. Must be positive.                                                                                                                                                        |
| `row_limit`       | `None`  | Maximum output rows; zero produces no batches.                                                                                                                                                   |
| `byte_limit`      | `None`  | Cumulative Arrow batch memory limit after selection.                                                                                                                                             |
| `max_range_bytes` | 64 MiB  | Aggregate compressed row-group or footer range limit, checked before allocation.                                                                                                                 |
| `max_batch_bytes` | 64 MiB  | Single projected batch limit, checked after decoding.                                                                                                                                            |
| `read_chunk_size` | 1 MiB   | Maximum individual file read; positive and at most 8 MiB.                                                                                                                                        |
| `deadline`        | `None`  | Absolute deadline in `asyncio.get_running_loop().time()` units. Includes opening the file, metadata, reads, decoding and idle consumer time.                                                     |

Buffer accounting and decoder allocation limits follow the
[Rust async scan contract](#rust-asynchronous-parquet-scans). One-shot native S3 scans own an independent
filesystem, file and I/O context. The native context uses the fsspec-rs defaults of 64
operations, 64 MiB of buffered I/O and an 8 MiB maximum read; S3 reserves both output and
staging capacity. Decode work runs on the Rust scan worker. PyO3 connects Rust futures to
the calling asyncio loop, without using Python executor threads for waits.

With `fs=`, the scan owns the opened file and leaves the filesystem open. The file must
provide a cached nonnegative integer `size`, awaitable `read(n)` returning at most `n`
bytes, `seek(offset, 0)`, and awaitable `aclose()` or `close()`. Seeking may be asynchronous
or update a cached cursor synchronously without I/O. Calls run on the opening task's
asyncio loop with its captured context variables. Pending calls are cancelled and awaited
before closing the file. The opener is responsible for cleaning up partially opened files
if opening fails or is cancelled. Providers must preserve the file's size and contents
throughout a scan, using immutable objects or version checks. Provider caches and internal
allocations are outside the scan's buffer limits. There is no synchronous I/O fallback.

`AsyncDecodedBatchStream` supports `async for`, `async with`, and `await stream.aclose()`.
It permits one pending batch request. Overlapping `__anext__` calls raise `RuntimeError`.
The producer does not prefetch another batch while the consumer is idle. Breaking out of
`async for` requires `aclose()` or an enclosing `async with` to wait for cleanup.

EOF and scan errors close the stream. Task cancellation during opening or iteration
cancels native work and raises `asyncio.CancelledError`. `aclose()` cancels pending work,
waits for owned resources to close and is safe to repeat. Cleanup continues if its
awaiter is cancelled; another `aclose()` waits for that same cleanup. Cleanup is not cut
short by the scan deadline. Dropping the stream requests native cancellation without
waiting for cleanup.

Deadlines raise `TimeoutError`; buffer limit failures raise `ValueError`. Storage errors
use standard Python exceptions, including `FileNotFoundError`, `PermissionError`,
`ConnectionError`, `TimeoutError` and `OSError`. Storage exceptions retain fsspec-rs's
`kind` string and `retry_safe` flag. Object replacement raises `OSError` with
`kind == "conflict"`. Python provider exceptions preserve their original type and attributes.
A consumer exception takes precedence over cleanup errors during
context-manager exit.

## Configured S3 Parquet sources

`S3ParquetSource(bucket, storage_options=None, cache_bytes=64 * 1024 * 1024)` retains one native S3 client, I/O context
and opaque source token. Options follow `scan_parquet_async`. Configuration is read when
the source is constructed; mutating the input mapping or environment does not rebuild its
client. Create a new source when changing configuration or effective authorization.

`await source.scan_async(path, **options)` implements the `DataSource` async scan contract
for Parquet. Other data-source operations are unsupported. Paths may be object keys or
`s3://`/`s3-rs://` URLs in the configured bucket. A bucket prefix in a key remains part of
that key. Selection, limits and deadlines match `scan_parquet_async`; each scan owns its
file and cancellation state. Native admission and I/O byte budgets are shared by the
source's context; decoder limits remain per scan.

Use the source within one asyncio loop. It supports `async with` and `await source.aclose()`.
Closing cancels and drains its pending opens and scans, clears shared ranges, then closes the client and context.
Repeated close waits for the same cleanup, including after cancellation of a close waiter.
New scans on a closed source raise `RuntimeError`. Closing or cancelling a single scan
leaves the source and other scans usable.

Native S3 streams expose a read-only `identity: ScanIdentity`. It includes the source token,
the opened handle's normalized object key, pinned ETag and size, actual columns/row offset,
batch/row/byte limits, and Parquet codec. Metadata comes from the opened handle without
another network request. Network reads continue to check ETag and size; identity does not replace
those checks or claim S3 version-ID pinning. Filters and resume positions are null because
this scan API does not execute them. Deadlines and execution buffer limits remain per scan.

Matching requests on one configured source compare equal. Separate source instances and
one-shot helper calls have distinct source tokens, even with identical options. Identity
remains available after stream close. Python file providers return `identity is None`
until their snapshot contract is integrated.

### Shared ranges and memory limits

Configured sources share raw byte ranges by normalized object key, pinned ETag and size
within that source's credential context. Different selections may reuse the same raw bytes;
their decoders, output ordering, limits and logical identities remain independent. Every
new scan opens its own handle and performs HEAD, including when its data is cached.
Separate sources never share ranges. One-shot helpers and Python file providers use their
existing independent reads.

`cache_bytes` bounds the combined reservations for pending fetches and retained shared
data. Zero disables sharing and caching. Completed, unused ranges are evicted in least
recently used order. Reads wait for capacity when entries are in use; logical reads larger
than the budget are split. Entry count is capped at 1024, with at most eight active fetches.
The native I/O context separately budgets output/staging buffers. Per-consumer copies,
decoder allocations, metadata and transport internals are outside `cache_bytes`; it is
not a process memory limit.

A 1 ms dispatch window lets pending overlapping or adjacent ranges combine without reading
gaps. Combined fetches are at most 8 MiB and must fit the shared budget. Once a fetch has
started, later requests reuse the covered portion and fetch only missing segments. Each
consumer still receives at most its `read_chunk_size` per file read. Coalescing can add
latency for a cold, low-latency workload; `cache_bytes=0` provides a comparison baseline.

Cancellation or a deadline removes only that consumer. The last consumer cancels an
unfinished fetch and waits for its cleanup; other consumers can continue using a fetch
started with the cancelled scan's handle. Failed reads are not retained for subsequent
reuse. Source shutdown drains pending work and releases cached data. Cached bytes remain
part of the originally opened snapshot; cache misses use its conditional handle, so data
from a replacement cannot be mixed into that snapshot.

## Scan identity

Rust and Python export `SourceIdentity` and `ScanIdentity`. They are immutable equality
and hash keys for resolved scan requests. These types do not execute scans, cache results,
or share reads. Native S3 scans construct them from the opened handle as described above;
other providers require explicit integration.

`SourceIdentity::new()` in Rust, or `SourceIdentity()` in Python, creates an opaque token
for one source configuration and credential context. Retain the token for requests in that
context. A different configuration, tenant, credential set or effective authorization
requires a new token, even when it uses the same filesystem object. Tokens contain no
credentials and have no serialization contract. They are intended for in-process use.

Python construction:

```python
from fsspec_data import ScanIdentity, SourceIdentity

source = SourceIdentity()
identity = ScanIdentity(
    source,
    "orders",
    snapshot={"namespace_id": "warehouse", "table_id": "orders-uuid", "generation": 7},
    columns=["order_id", "amount"],
    row_offset=100,
    row_limit=500,
)
```

The snapshot must describe the version actually pinned by the provider. Manifest snapshots
include namespace ID, table ID and generation; a generation number alone cannot distinguish
replacement tables. Object snapshots can use a provider-checked version ID or ETag. The
constructor does not verify a snapshot against storage. A missing or null snapshot gives
each construction a distinct identity, so a mutable path cannot match an earlier request
by default. Copying or cloning an existing identity preserves it.

| Python parameter  | Default  | Identity meaning                                                                                                                  |
| ----------------- | -------- | --------------------------------------------------------------------------------------------------------------------------------- |
| `source`          | Required | Opaque source and credential scope.                                                                                               |
| `path`            | Required | Exact path; no protocol stripping, case folding or path normalization.                                                            |
| `snapshot`        | `None`   | Provider-defined JSON description of the pinned version. Null disables matching independently constructed requests.               |
| `columns`         | `None`   | Ordered column names, including duplicates. All columns (`None`) differs from no columns (`[]`).                                  |
| `row_offset`      | `0`      | Rows skipped before output limits.                                                                                                |
| `batch_size`      | `1024`   | Positive maximum rows per batch; affects batch boundaries.                                                                        |
| `row_limit`       | `None`   | Output row limit. Zero differs from no limit.                                                                                     |
| `byte_limit`      | `None`   | Cumulative output byte limit.                                                                                                     |
| `filter`          | `None`   | Provider-defined JSON predicate; compared without interpretation or execution.                                                    |
| `start_position`  | `None`   | Provider-defined JSON resume position, including its source/version context where applicable.                                     |
| `backend_options` | `None`   | Additional result-affecting settings, including schema and codec options. Credentials belong to the source scope, not this value. |

JSON object key order is ignored recursively. Arrays, column order, duplicates, strings and
JSON types remain significant. Filters are not rewritten for semantic equivalence.
Order-sensitive settings, such as schema field order, must be represented as arrays.
Python accepts JSON scalars, lists and dictionaries with string keys; integers must fit signed or
unsigned 64 bits and floats must be finite. Other inputs are rejected rather than coerced.
Inputs are copied, so later mutation cannot change an identity already used as a key.

Rust construction uses `ScanIdentity::new(source, path, snapshot, selection, options, filter, start_position, backend_options)`: `selection` is `ScanSelection`, `options` is
`StreamOptions`, and the snapshot, filter, position and backend options are `serde_json::Value`.
Rust and Python use the same comparison implementation. Debug/repr output omits request
contents, including paths and predicate values. Hash values are not persistent IDs.

Cancellation, deadlines and execution buffer limits remain per consumer. Equal logical
identities do not authorize sharing cancellation state, bypassing a consumer's limits,
or skipping authorization and version checks. These execution rules must be enforced
separately by any future read-sharing implementation.
