# Manifest table reference

`ManifestTable`, `TableSnapshot`, `TableWriter`, `SinkOptions`, `BatchReceipt`,
`PublicationReceipt`, `RecoveryToken`, `OrphanCollector`, `CleanupReport`, `TableWatcher`,
`WatchBatch`, `WatchPosition`, `WatchEnd`, and `AsyncTableWatcher` are exported from `fsspec_data`. The bindings
use the Rust manifest reader and file sink. Synchronous operations release the Python GIL
during storage access and encoding/decoding. Async readers use native S3 handles or local-file
workers, with Parquet decoding on a worker.

## ManifestTable

`ManifestTable(root, *, storage_options=None, max_manifest_bytes=8388608, max_part_bytes=67108864, max_manifest_reads=256)` configures one table partition on native
local or S3 storage. Local roots accept `str` or `PathLike[str]` and are resolved to
absolute paths at construction. S3 roots use `s3://bucket/prefix`. Storage options follow
the native S3 client, including endpoint, region, credentials and anonymous access.
Each table owns a configured filesystem instance; Python fsspec objects and cross-extension
filesystem registrations are not used by this constructor.

The storage backend must support durable, atomic create-if-absent and consistent reads.
Native capability support does not certify a deployment. All published generations and
their referenced parts are retained. Orphan cleanup is explicit.

| Method                                                                            | Result                                                                                      |
| --------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| `snapshot(generation=None)`                                                       | Validated `TableSnapshot`; `None` discovers latest publication without listing directories. |
| `open_writer(schema, *, mode="create", options=None)`                             | `TableWriter` accepting PyArrow record batches with exactly matching schema.                |
| `orphan_collector(*, retention_seconds, max_generations=256, max_entries=100000)` | Stateful `OrphanCollector` for failed-write files.                                          |
| `recover(token, *, retry=False, options=None)`                                    | `PublicationReceipt` for a saved attempt, using its persisted descriptor.                   |

`create` requires no live table, `append` requires an incomplete table with matching schema
and partition values, and `replace` requires an existing table. Replacement creates a new
table identity and retains earlier generations. Schema evolution and cross-partition
transactions are not implemented. Writer schemas support booleans, signed/unsigned integers,
Float32/64, strings, binary, dates, timestamps, valid Time32/64 units, durations, and Decimal128.
Nested and dictionary fields and other scalar types are rejected as unsupported.

`recover` checks the token's root against this table. The caller must restore the same source
and credential context. `retry=False` observes publication and repairs a completion marker;
`retry=True` may also resubmit the identical candidate. Observation of an absent publication
returns `unknown_outcome`, since an earlier request may still complete. Recovery cannot
reconstruct an attempt that failed before its candidate descriptor was stored. Passing the
original writer's options preserves its publication and recovery read limits.

## TableWatcher

`table.watch(*, start=None, columns=None, batch_size=1024, poll_interval=0.1, idle_timeout=None)` returns a synchronous `TableWatcher`. The table must already be
published with row-based progress. A new watch begins with all rows of the latest
snapshot. `start` resumes a `WatchPosition` from its saved generation. The watcher
reads successive generations in order, without listing directories.

Each iteration returns `WatchBatch(batch, position)`. Its position describes progress
after that batch. The watcher also exposes the selected Arrow `schema`, last returned
`position`, and `end_reason`. Before the first batch, `position` is the starting checkpoint.
A position records the source location, namespace/table identity, generation, manifest
checksum, progress and progress unit. JSON serialization uses `to_json()` and
`WatchPosition.from_json(text)`. It contains no credential options; reopening a source
still requires authorization. The source location includes the protocol, resolved path (including the S3 bucket), and explicit S3
endpoint/region options. Resume requires the same location configuration.

`columns` preserves requested order, duplicates and empty projections. `batch_size` bounds
rows per batch and must be positive. `poll_interval` is a finite positive number of seconds.
`idle_timeout` is a finite nonnegative number of seconds, or `None` to wait indefinitely.
Zero drains available rows and ends when no successor is published. Positive timeouts
measure time since the last returned batch, including consumer processing time, and are
checked when a successor is absent. They are not storage request deadlines.

After iteration ends, `end_reason` is one of:

| Value                   | Meaning                                                      |
| ----------------------- | ------------------------------------------------------------ |
| `WatchEnd.COMPLETE`     | All rows of a complete generation were returned.             |
| `WatchEnd.IDLE_TIMEOUT` | No successor was available before the idle timeout.          |
| `WatchEnd.CANCELLED`    | Cancellation was observed.                                   |
| `WatchEnd.REPLACED`     | A successor changed the table identity or deleted the table. |

`cancel()` and `close()` request cancellation; context exit calls `close()`. Cancellation
wakes polling waits and is checked between reads. A synchronous storage call cannot be
interrupted. Properties and iteration serialize on the watcher; cancellation can be called
from another thread. Errors propagate and terminate iteration without setting an end reason
or advancing the last returned checkpoint. Resume can retry from that checkpoint.

The watcher verifies encoded parts before decoding and skips parts before the saved progress.
It does not switch to replacement data, infer event/array positions from row counts, watch
filesystem notifications. Use `watch_async` for asynchronous iteration. Standalone scans remain pinned.
The Rust equivalents are `watch::ManifestWatcher`, `WatchOptions`, `WatchPosition`,
`WatchBatch`, and `WatchEnd`; Rust callers provide a stable non-secret source label.

## Asynchronous reads

- `await table.snapshot_async(generation=None, *, deadline=None)` returns a `TableSnapshot`.
- `await snapshot.scan_async(*, columns=None, row_offset=0, batch_size=1024, row_limit=None, byte_limit=None, max_batch_bytes=67108864, read_chunk_size=1048576, deadline=None)` returns `AsyncDecodedBatchStream`.
- `await table.watch_async(*, start=None, columns=None, batch_size=1024, poll_interval=0.1, idle_timeout=None, max_batch_bytes=67108864, read_chunk_size=1048576, deadline=None)` returns `AsyncTableWatcher`.

Async scans and watchers support `async for`, async context management and `await aclose()`.
Watch batches, positions and terminal reasons have the synchronous API's meanings.
Concurrent calls to `anext` on one iterator raise `RuntimeError`. Closing during a pending
request cancels it and suppresses undelivered rows without advancing the Python checkpoint.
Errors terminate iteration and preserve the last delivered checkpoint.

`deadline` is an absolute `asyncio` loop time, including discovery, reads, decoding and time
between batch requests. Expiry raises `TimeoutError`; it is distinct from a watch's idle
timeout. Task cancellation raises `asyncio.CancelledError`. `aclose` waits for pending I/O
and decoder cleanup; cleanup continues if its Python awaiter is cancelled. A local system
call or decoder computation cannot be forcibly interrupted, so cleanup can exceed a deadline.
Dropping an iterator requests cancellation; deterministic cleanup requires `aclose`.

A scan fetches parts only on demand. It retains one encoded part at a time, capped by the
table's `max_part_bytes`, plus transport read chunks and decoder/output allocations.
`read_chunk_size` must be positive and at most 8 MiB. `max_batch_bytes` must be positive
and is checked after decoding/projection. One batch request may be outstanding per iterator;
there is no background batch prefetch. These are per-scan limits, not a total process memory
cap. Caller-retained batches and Parquet internal allocations are not capped.

Each part's full size and SHA-256 are verified before any of its rows are returned. Schema
and row-count checks also match synchronous scans. No range/predicate pushdown or physical
read sharing is provided for manifest parts. Snapshots from `snapshot_async` use native
async S3 reads; snapshots from `snapshot` use their existing synchronous filesystem on the
scan worker. Local reads use a worker in both cases. Closing one scan leaves its snapshot
and other scans usable.

Rust provides `ManifestReader::read_async`, `Snapshot::scan_async`, `AsyncTableScan`,
`AsyncManifestWatcher` and `AsyncTableOptions` in `async_tables`. `ManifestReader::new_async`
borrows an `fsspec_rs::async_files::AsyncFilesystem`; its caller retains and closes the
filesystem/context after readers finish. Existing `ManifestReader::new` uses synchronous
storage on a worker. Rust `next().await` returns `Result<Option<_>>`; `close().await` drains
cleanup, and cancellation uses `CancellationToken`. Rust deadlines use `std::time::Instant`.

## SinkOptions

| Field                | Default | Meaning                                                                  |
| -------------------- | ------- | ------------------------------------------------------------------------ |
| `target_part_bytes`  | 8 MiB   | Part rotation target, excluding possible footer/encoding overshoot.      |
| `memory_limit`       | 64 MiB  | Input/output buffer accounting plus estimated Parquet encoder memory.    |
| `max_manifest_bytes` | 8 MiB   | Encoded publication metadata cap, at most 8 MiB.                         |
| `max_history_reads`  | 256     | History reads allowed while checking an earlier retry key.               |
| `partition_values`   | `{}`    | JSON-compatible partition metadata; must match on append or replacement. |

The target must be positive and smaller than the memory limit. Oversized inputs or encoded
parts fail without publishing partial rows. Internal Parquet and transport allocations can
exceed accounted memory; `memory_limit` is not a process RSS guarantee. Parquet is the only
writer encoding. Progress is measured in rows, stored separately from `num_rows`.

## TableWriter

| Member                                          | Behavior                                                                           |
| ----------------------------------------------- | ---------------------------------------------------------------------------------- |
| `schema`                                        | Input Arrow schema.                                                                |
| `position`                                      | Accepted rows, including pending work.                                             |
| `recovery_token`                                | Current publication attempt; persist before each publication.                      |
| `write_batch(batch, *, expected_position, key)` | Returns `BatchReceipt`; identical retries are acknowledged without encoding twice. |
| `flush()`                                       | Stages completed parts without publishing a manifest.                              |
| `commit()`                                      | Publishes pending work and keeps the writer/table open.                            |
| `finish()`                                      | Publishes and marks complete; repeated successful finish returns its receipt.      |
| `resolve(*, retry=False)`                       | Resolves an uncertain publication and updates this writer's state.                 |
| `abort()` / `close()`                           | Discards pending work, preserving earlier commits.                                 |

Keys must contain 1–256 UTF-8 bytes. Batches must be nonempty. An unexpected forward
position raises `gap`; overlapping positions, mismatched retry payloads or competing
publishers raise `conflict`. Retry history is scoped to the table identity and bounded by
`max_history_reads`.

Context-manager success commits and closes without marking complete. Exceptional exit
aborts; if abort fails, the original exception is preserved with a cleanup note. Dropping a
writer never publishes. An uncertain publication blocks further writes and abort until it
is resolved. Publication errors expose `recovery_token` as well as structured error attributes.

`BatchReceipt` contains `key`, `start`, `end`, and a batch `fingerprint`.
`PublicationReceipt` contains `recovery`, manifest `sha256`, committed `progress`, `num_rows`,
`complete`, and optional `marker_error` (`kind`, `message`). A marker failure does not undo
successful publication. Only publication receipts establish committed progress.

`RecoveryToken` contains `root`, `namespace_id`, `table_id`, `generation`, and `publication_id`.
`to_json()` and `from_json(text)` support persistence. Tokens contain no credentials and
must be paired with the original storage configuration.

## TableSnapshot

Read-only properties expose `schema`, `generation`, `num_rows`, `progress`, `complete`, and
`metadata`, and `state`. Metadata returns an independent dictionary of the validated manifest, including
identity, partition values, parts and receipts. A snapshot retains its filesystem and selected
generation even after its parent `ManifestTable` is released.

`state` returns an independent dictionary with `generation`, `num_rows`, `num_rows_exact`,
`complete`, `progress`, and `progress_unit`. Manifest row counts are exact. Progress is
separate from row counts even when its unit is `rows`. Partition values appear as singleton
`distinct.<column>` lists. Unavailable modification times and other distinct values are omitted.
Completion comes from the manifest, independently of `_SUCCESS`. Rust exposes `Snapshot::state()`.

`scan(*, columns=None, row_offset=0, batch_size=1024, row_limit=None, byte_limit=None)` returns
`DecodedBatchStream`. Output preserves part order and requested column order, including
duplicates and empty projections. Offset and limits apply across parts. Only Parquet parts without encoding options are supported. Selection currently
skips complete parts before the offset, then slices decoded rows; this API does not perform
range or predicate pushdown. Rust exposes the same reader through `Snapshot::scan()`.

Each requested part is read in full within `max_part_bytes`, verified against its SHA-256 and
size, then checked for matching Parquet schema and row count. Unlisted files are invisible.
Later parts are fetched only as iteration reaches them. A zero row limit reads no parts.
`byte_limit` bounds cumulative emitted Arrow bytes, not internal decoder allocations.
Cancellation stops iteration; synchronous reads already in progress are not interruptible.

Storage/publication errors expose `kind` and `retry_safe` using the existing native error
mapping. Not found, already exists, permission, timeout, unavailable and invalid map to Python
built-ins; `conflict`, `gap`, `corrupt` and `unknown_outcome` use `OSError` with those attributes.
Scan option validation and output-limit errors retain `DecodedBatchStream` behavior.

## OrphanCollector

`collect(*, dry_run=True)` returns a `CleanupReport` with `generation` (published cleanup
boundary, or `None` for an unpublished table), `pending` (observed orphan files awaiting
retention), `eligible` (relative keys past retention), and `deleted` (keys removed this
sweep). Dry runs update observations but leave `deleted` empty. An absent object during
deletion is ignored and does not appear in `deleted`.

Retention must be positive and finite. It uses monotonic collector observations, not
writer or store timestamps. Changed size/modification metadata, a missing listing entry,
a failed sweep, or a new collector resets the relevant observations. Observations are
in-memory and bound to this filesystem instance and table root.

Before deletion, cleanup validates the complete manifest chain from zero through its
boundary and protects parts referenced by any generation, plus winning candidate descriptors.
It lists only the fixed `parts/<generation>/<attempt>/<filename>` layout. Future generations,
manifests, completion markers, directories, symlinks and malformed keys are not removed.
The namespace must be managed exclusively by protocol-compliant writers; replacing published
manifests, changing part contents, introducing filesystem aliases, and independent lifecycle
deletion violate that contract. Historical pruning and multipart-upload cleanup are unsupported.

`max_generations` bounds the number of historical manifests. Discovery has the reader's
256-request default limit. The table's `max_manifest_bytes` bounds each encoded manifest.
`max_entries` separately bounds protected keys and total entries returned across listings;
it is not a transport or process memory limit, since a filesystem can allocate a complete
listing before returning it. Exceeding a limit raises `invalid` before deletion. Read,
validation, or listing errors also prevent deletion. Deletion errors propagate after any
successful earlier deletions; the sweep is repeatable.

Rust exposes the equivalent `cleanup::{OrphanCollector, CleanupLimits, CleanupReport}`.
`OrphanCollector::new` accepts a shared filesystem, root, retention `Duration`, and limits;
`collect(dry_run)` requires mutable access.
