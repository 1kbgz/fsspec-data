# How to connect a batch producer or consumer

Use an Arrow schema and bounded record batches as the boundary between `fsspec-data` and an
integrating package. Keep discovery, query planning, and rendering in their owning package.

## Declare the format and schema contract

Map backend-native values to a `pyarrow.Schema`. Describe both sides of the conversion
before reading data:

```python
from fsspec_data import DataFormat, InterchangeRequest, SchemaPolicy

request = InterchangeRequest(
    provided_format=DataFormat.PARQUET,
    requested_format=DataFormat.ARROW,
    provided_schema=source_schema,
    requested_schema=consumer_schema,
    policy=SchemaPolicy.COMPATIBLE,
)
plan = request.plan()
```

Choose the narrowest schema policy in the [API reference](api.md) that accepts the required
conversion.

## Pass encoded data to a consumer lazily

Iterate the plan when the consumer accepts Arrow batches:

```python
stream = plan.iter_batches(encoded, batch_size=1_024, row_limit=10_000)
try:
    for batch in stream:
        consumer.accept(batch)
finally:
    stream.cancel()
```

Set `byte_limit` to reject decoded batches above a cumulative Arrow-memory budget. Cancel
the stream when the consumer stops early.

## Produce encoded output

Call `convert` when the consumer requires one encoded byte buffer:

```python
encoded_arrow = plan.convert(encoded, batch_size=1_024)
```

`convert` buffers its encoded result. Use `iter_batches` for incremental scans, previews,
and database reads.

## Integrate from Rust

Compose registered codecs with the plan's stream adapter:

```rust
let source = DEFAULT_REGISTRY.get(request.provided_format)?;
let target = DEFAULT_REGISTRY.get(request.requested_format)?;
let plan = plan(&request)?;
let target_schema = plan.requested_schema.clone();
let decoded = source.decode_stream(
    encoded,
    None,
    StreamOptions::default(),
    CancellationToken::new(),
)?;
let mut batches = plan.apply_stream(decoded);
let mut output = Vec::new();
let mut writer = target.start_writer(target_schema, &mut output)?;
for batch in batches {
    writer.write_batch(&batch?)?;
}
writer.finish()?;
```

Use `start_writer` for Arrow IPC or Parquet when the producer supplies batches over time.
Arrow IPC emits bytes during batch writes. Parquet accepts batches incrementally but may
buffer encoded bytes until `finish`. Use `encode_stream` when the complete iterator can be
consumed by one call.

See the [API reference](api.md) for supported formats, limits, casts, and errors.
