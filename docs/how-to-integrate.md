# How to connect a batch producer or consumer

Use an Arrow schema and bounded record batches as the boundary between `fsspec-data` and an
integrating package. Keep discovery, query planning, and rendering in their owning package.

## Declare the format and schema contract

Map backend-native values to a `pyarrow.Schema`. Describe both sides of the conversion
before reading data:

```python
from fsspec_data import DEFAULT_REGISTRY, DataFormat, InterchangeRequest, SchemaPolicy

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

Write an encoded stream directly to a binary file when the consumer does not need one byte
buffer:

```python
with open("result.arrow", "wb") as output:
    DEFAULT_REGISTRY.get(DataFormat.ARROW).encode_batches_to(
        plan.iter_batches(encoded),
        output,
        schema=plan.requested_schema,
    )
```

## Convert between Xarray and Zarr

Install the optional dependencies:

```console
pip install "fsspec-data[xarray]"
```

Open a Zarr store as an Xarray dataset while keeping storage credentials scoped to the source:

```python
from fsspec_data import DEFAULT_CONVERTERS

dataset = DEFAULT_CONVERTERS.convert(
    "zarr",
    "xarray",
    "s3://weather-input/forecast.zarr",
    source_options={"profile": "weather-reader"},
    conversion_options={"group": "forecast", "chunks": "auto"},
)
```

Write a dataset to another Zarr store with independent target credentials:

```python
DEFAULT_CONVERTERS.convert(
    "xarray",
    "zarr",
    dataset,
    "s3://weather-output/forecast.zarr",
    target_options={"profile": "weather-writer"},
    conversion_options={"mode": "w-", "zarr_format": 3},
)
```

Use the converter API rather than `DataFileSystem.open()` for Zarr because a Zarr hierarchy
contains multiple metadata and chunk objects.

## Register a converter through an entry point

Expose one `Converter` object from the integration package:

```python
from fsspec_data import Converter


def geotiff_to_xarray(
    source,
    target,
    *,
    source_options,
    target_options,
    conversion_options,
):
    ...


GEOTIFF_TO_XARRAY = Converter("geotiff", "xarray", geotiff_to_xarray)
```

Register that object in the package's `pyproject.toml`:

```toml
[project.entry-points."fsspec_data.converters"]
geotiff-xarray = "my_package.converters:GEOTIFF_TO_XARRAY"
```

Installed entry points are discovered on the registry's first lookup. Keep optional heavy
imports inside the converter handler so loading its descriptor remains inexpensive. Route
names are case-insensitive. Duplicate source-target routes raise an error instead of selecting
one by installation order.

Call `DEFAULT_CONVERTERS.register(converter)` instead when registration is local to one
process and does not need package discovery.

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

If the writer session must be stored beyond the scope that creates it, pass a boxed shared
sink to `start_owned_writer`:

```rust
let sink = SharedSink::default();
let output = sink.clone();
let writer = target.start_owned_writer(target_schema, Box::new(sink))?;
let file = EncodedFile::new(writer, output);
```

See the [API reference](api.md) for supported formats, limits, casts, and errors.
