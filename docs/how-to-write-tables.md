# How to write and read saved table versions

Use `ManifestTable` to publish Parquet batches and reopen a saved version. Install
`fsspec-data` and use a new local directory for this example. For S3, replace the path
with `s3://bucket/prefix` and pass `storage_options` to the constructor. The store must
provide atomic create-if-absent and consistent reads; deployment certification remains
separate from this API.

## Publish batches

```python
import pyarrow as pa
from fsspec_data import ManifestTable

table = ManifestTable("events")
first = pa.record_batch({"value": [10, 20]})

with table.open_writer(first.schema) as writer:
    writer.write_batch(first, expected_position=0, key="request-1")

saved = table.snapshot()
assert saved.num_rows == 2
assert not saved.complete
```

A clean context exit commits the data and closes the writer. An exception aborts its
unpublished work. Earlier commits remain visible. Use a stable request key when retrying
a batch, with its original expected position; the writer acknowledges an identical retry
without appending it twice.

## Append and finish

```python
second = pa.record_batch({"value": [30]})

with table.open_writer(first.schema, mode="append") as writer:
    writer.write_batch(second, expected_position=2, key="request-2")
    receipt = writer.finish()

assert receipt.complete
assert receipt.progress == 3
```

Use `commit()` for intermediate checkpoints and `finish()` when the table is complete.
Checkpoint `PublicationReceipt.progress` after success. `writer.position` also includes
unpublished rows and is not a durable checkpoint. A complete table rejects appends;
`mode="replace"` starts new table contents while retaining old saved versions.

## Read a saved version

```python
old = table.snapshot(saved.generation)
latest = table.snapshot()

old_rows = [batch.to_pydict() for batch in old.scan()]
new_rows = [batch.to_pydict() for batch in latest.scan(columns=["value"], row_offset=1)]

assert old_rows == [{"value": [10, 20]}]
assert sum(batch.num_rows for batch in latest.scan()) == 3
```

Snapshots remain pinned after subsequent publications. Scans read only listed parts,
one at a time, verifying their checksums before decoding. Set `max_part_bytes` when
constructing the table to cap encoded part reads. Use `batch_size`, `row_limit`, and
`byte_limit` to bound scan output; decoder allocations are not a process memory cap.
Cancel a scan with `stream.cancel()` if stopping early.

## Recover an uncertain publication

Persist the current recovery token before each `commit()` or `finish()`. The token changes
after successful publication. Store it with the table URL and the configuration needed to
reopen that source; it does not contain credentials.

```python
from pathlib import Path
from fsspec_data import RecoveryToken

pending = ManifestTable("recoverable-events")
writer = pending.open_writer(first.schema)
writer.write_batch(first, expected_position=0, key="request-1")
Path("pending.json").write_text(writer.recovery_token.to_json())
receipt = writer.commit()
writer.close()

# The same recovery call works after restarting the process.
token = RecoveryToken.from_json(Path("pending.json").read_text())
receipt = ManifestTable("recoverable-events").recover(token)
```

If observation reports `error.kind == "unknown_outcome"`, the request may still complete.
Use `table.recover(token, retry=True)` to resubmit the exact saved candidate. Pass the original
`SinkOptions` as `options=` if changing default limits. With a live writer, use
`writer.resolve(retry=True)` so its in-memory state also advances. Retrying recovery does
not append rows again. A competing winner raises `conflict`.

`finish()` may return a successful receipt with `marker_error` when the manifest was published
but `_SUCCESS` could not be written. The table is complete; `table.recover(receipt.recovery)`
retries marker repair. See [manifest table reference](manifest-tables.md) for limits,
error attributes, and recovery behavior.
