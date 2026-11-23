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

For native resource conversions and entry-point registration, see
[How to convert Xarray and Zarr resources](how-to-convert-resources.md).
For the Rust codec boundary, see [How to integrate Rust batch streams](how-to-rust.md).

## Read S3 Parquet without blocking asyncio

Use `scan_parquet_async` when the consumer already runs in an asyncio loop. Await opening
the scan, then use its async context manager to close the file when iteration ends early:

```python
import asyncio

from fsspec_data import scan_parquet_async


async def consume():
    stream = await scan_parquet_async(
        "s3://bucket/orders.parquet",
        columns=["order_id", "amount"],
        batch_size=4096,
        deadline=asyncio.get_running_loop().time() + 30,
    )
    async with stream:
        async for batch in stream:
            print(batch.num_rows)


asyncio.run(consume())
```

For an S3-compatible endpoint, pass flat options such as
`storage_options={"endpoint_url": "http://minio:9000", "region": "us-east-1"}`.
Credentials follow fsspec-rs's options and environment defaults.

Cancel the consuming asyncio task to stop pending reads. If you manage the stream without
an async context manager, await `stream.aclose()` in a `finally` block. See the
[async scan reference](api.md#python-asynchronous-parquet-scans) for buffer limits, deadline
semantics and supported protocols.

## Scan through a configured async filesystem

Pass your existing filesystem to `FileDataSource` when it supports `open_async`. Call this
function from the loop used by that filesystem:

```python
from fsspec_data import FileDataSource


async def consume(fs, path):
    source = FileDataSource(fs)
    async with await source.scan_async(path, columns=["order_id", "amount"]) as stream:
        async for batch in stream:
            print(batch.num_rows)
```

Alternatively, call `await scan_parquet_async(path, fs=fs, columns=["order_id", "amount"])`.
Keep the filesystem open until its scans finish. The scan closes its own file, and uses
the filesystem's existing credentials and configuration. Sync-only filesystems raise
`NotImplementedError`. Check the [file provider contract](api.md#python-asynchronous-parquet-scans)
when implementing an async filesystem.
