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

Python codec methods currently accept encoded input as bytes-like objects. This keeps the
first public contract small while preserving lazy decoding after the input crosses the
binding. Reader-backed adapters can extend the transport boundary without changing schema
planning or batch-stream semantics.
