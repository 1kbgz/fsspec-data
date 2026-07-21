# Interchange design

`fsspec-data` is the interchange boundary between packages that produce tabular data and
packages that consume it. It reconciles formats and schemas without absorbing database,
dataframe, or user-interface responsibilities.

## Arrow is the internal boundary

Arrow provides one typed, columnar representation for schema comparison, casting, and
batch transport. Codecs translate external encodings into Arrow batches and back again.
Adding a format therefore requires one Arrow decoder and encoder instead of converters for
every pair of formats.

The Rust core and Python bindings share this boundary. Schema decisions, cast
classifications, batch limits, and cancellation semantics remain consistent whether an
integration enters through Rust or PyArrow.

## Planning precedes execution

An `InterchangeRequest` separates validation from data access. Planning checks that codecs,
field mappings, nullability changes, and casts can satisfy the requested contract without
reading input. Execution then applies that stable plan to each batch.

This split lets an integrating package reject an unsupported request before starting a
database scan or opening an output sink. Runtime checks remain necessary only for facts the
schema cannot prove, such as whether a string contains an integer or a nullable column
actually contains nulls.

## Streaming is the default execution model

Record batches keep memory bounded and allow a consumer to stop early. Row, batch, and byte
limits protect previews and interactive clients from unexpectedly large inputs.
Cancellation propagates through the planned stream to its decoder.

Encoded conversion is a buffering adapter on top of that stream. It exists for consumers
that require a complete byte buffer, while the batch iterator remains the primary boundary
for scans and previews.

## Package responsibilities remain narrow

- Database libraries own connections, discovery, SQL, predicate pushdown, and
  database-to-Arrow mapping.
- Dataframe integrations own expression translation and local fallback execution.
- Browsers own pagination, rendering, and request lifecycles.
- `fsspec-data` owns format codecs, schema reconciliation, casts, and interchange limits.

These boundaries keep backend-specific semantics close to each backend and make the
interchange layer reusable by all of them.

## Current transport boundary

Python codec methods accept encoded input as bytes-like objects or binary file-like readers.
Reader-backed input lets row limits and cancellation stop upstream reads before EOF instead
of requiring the complete encoded source to cross the Python/Rust boundary first.

Parquet readers must be seekable because Parquet metadata is stored in the footer and column
chunks can reside at different offsets. Arrow IPC, CSV, and JSONL consume readers
sequentially, but use the same seekable reader contract so registry consumers have one input
boundary.

`DataFileSystem` opens its inner source through fsspec, applies its schema plan batch by
batch, and writes encoded output directly into a seekable spooled file. This bounds
intermediate transport memory without changing the filesystem contract: `_open()` still
completes conversion before returning the spool, and `info()` may perform a complete
conversion to determine output size. Individual format writers may also buffer internally;
in particular, Parquet can defer encoded output until `finish`.
