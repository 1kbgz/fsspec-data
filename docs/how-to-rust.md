# How to integrate Rust batch streams

Use these recipes inside an integration that already has an interchange request,
encoded source, and output sink. Import the public types from `fsspec_data`:

```rust
use fsspec_data::{
    plan, CancellationToken, StreamOptions, DEFAULT_REGISTRY,
};
```

## Decode, plan, and encode

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
let batches = plan.apply_stream(decoded);
let mut output = Vec::new();
let mut writer = target.start_writer(target_schema.clone(), &mut output)?;
for batch in batches {
    writer.write_batch(&batch?)?;
}
writer.finish()?;
```

Use `start_writer` for Arrow IPC or Parquet when the producer supplies batches over time.
Arrow IPC emits bytes during batch writes. Parquet accepts batches incrementally but may
buffer encoded bytes until `finish`. Use `encode_stream` when the complete iterator can be
consumed by one call.

For an owned output sink, pass a boxed writer instead of a borrowed reference:

```rust
let output = std::fs::File::create("result.arrow")?;
let mut writer = target.start_owned_writer(target_schema, Box::new(output))?;
// Submit batches with writer.write_batch(&batch), then finish the session.
writer.finish()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

See the [API reference](api.md) for codec signatures, limits, casts, and errors.
