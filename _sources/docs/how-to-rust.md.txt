# How to integrate Rust batch streams

Use these recipes inside an integration that already has an interchange request,
encoded source, and output sink. Import the public types from `fsspec_data`:

```rust
use fsspec_data::{
    plan, CancellationToken, DataFormat, SchemaResolverRegistry, StreamOptions, DEFAULT_REGISTRY,
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

## Read through a plan

`InterchangePlan::iter_batches` decodes a reader in the provided format and applies the plan
to each bounded batch. `convert` also encodes the requested format into a writer:

```rust
use std::fs::File;

let options = StreamOptions { batch_size: 4096, row_limit: Some(100_000), ..StreamOptions::default() };
for batch in plan.iter_batches(Box::new(File::open("orders.csv")?), options, CancellationToken::new())? {
    let batch = batch?;
    // The batch has plan.requested_schema.
}
plan.convert(Box::new(File::open("orders.csv")?), &mut File::create("orders.parquet")?, options)?;
```

CSV and JSONL sources are decoded with the plan's provided schema.

## Resolve schema references

Resolve both schemas before building the request. Each reference can carry its own
filesystem, so the schemas and the data can use different backends and credentials:

```rust
use std::sync::Arc;

use fsspec_data::{
    InterchangeRequest, ProjectionOptions, SchemaFormat, SchemaPolicy, SchemaReference,
    DEFAULT_SCHEMA_RESOLVERS,
};
use fsspec_rs::{S3Config, S3Fs};

let schemas = Arc::new(S3Fs::new(S3Config::new("schemas"))?);
let provided = DEFAULT_SCHEMA_RESOLVERS.resolve(
    &SchemaReference::new("s3://schemas/orders.json")
        .with_format(SchemaFormat::ArrowJson)
        .with_filesystem(schemas),
)?;
let requested = DEFAULT_SCHEMA_RESOLVERS.resolve(&SchemaReference::new("/srv/schemas/report.arrow"))?;
let plan = plan(&InterchangeRequest {
    provided_format: DataFormat::Csv,
    requested_format: DataFormat::Parquet,
    provided_schema: provided.schema,
    requested_schema: requested.schema,
    policy: SchemaPolicy::Compatible,
    options: ProjectionOptions::default(),
})?;
```

`provided.provenance` and `requested.provenance` record where each schema came from.
References without a filesystem use the filesystem registered for their protocol; the
default registry has local files only. To resolve another protocol without passing a
filesystem each time, build a registry and register one:

```rust
let mut resolvers = SchemaResolverRegistry::default();
resolvers.register_filesystem("s3", Arc::new(S3Fs::new(S3Config::new("schemas"))?));
```

With the `confluent` feature, `confluent://SUBJECT/versions/VERSION` references resolve
through the Confluent Schema Registry:

```rust
let reference = SchemaReference::new("confluent://orders-value/versions/latest")
    .with_option("registry_url", "https://registry.example.com")
    .with_option("username", key)
    .with_option("password", secret);
let resolved = DEFAULT_SCHEMA_RESOLVERS.resolve(&reference)?;
```

`fetch_confluent_versions` returns every registered version of a subject, for reading values
written with older schemas.

## Register a converter

Implement `Converter` for a route that is not one tabular file, and register it:

```rust
use std::collections::HashMap;

use fsspec_data::{ConversionEndpoint, Converter, ConverterRegistry, Result};

struct NetcdfToZarr;

impl Converter for NetcdfToZarr {
    fn source_type(&self) -> &str { "netcdf" }
    fn target_type(&self) -> &str { "zarr" }
    fn convert(
        &self,
        source: &ConversionEndpoint,
        target: &ConversionEndpoint,
        options: &HashMap<String, String>,
    ) -> Result<()> {
        // Read source.url through source.filesystem and write target.url through
        // target.filesystem.
        Ok(())
    }
}

let mut converters = ConverterRegistry::new();
converters.register(NetcdfToZarr, false)?;
converters.convert(
    "netcdf",
    "zarr",
    &ConversionEndpoint::new("s3://in/a.nc"),
    &ConversionEndpoint::new("s3://out/a.zarr"),
    &HashMap::new(),
)?;
```

Rust has no entry points, so integrations register their converters explicitly.

See the [API reference](api.md) for codec signatures, limits, casts, and errors, and the
[schema and converter reference](resources.md#rust) for the Rust schema types.
