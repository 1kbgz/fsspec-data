"""Versioned Parquet tables published through immutable manifests."""

from __future__ import annotations

import json
import os
from collections.abc import Mapping, Sequence
from dataclasses import asdict, dataclass, field
from enum import StrEnum
from importlib import import_module
from types import TracebackType
from typing import Any, Literal, Self

import pyarrow as pa

from .async_scan import _s3_options
from .interchange import DecodedBatchStream

_rust = import_module(".fsspec_data", __package__)


@dataclass(frozen=True)
class SinkOptions:
    """Part buffering and publication limits; memory_limit is not a process RSS cap."""

    target_part_bytes: int = 8 * 1024 * 1024
    memory_limit: int = 64 * 1024 * 1024
    max_manifest_bytes: int = 8 * 1024 * 1024
    max_history_reads: int = 256
    partition_values: Mapping[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class RecoveryToken:
    """Persist before publication; restore using the same storage configuration."""

    root: str
    namespace_id: str
    table_id: str
    generation: int
    publication_id: str

    def to_json(self) -> str:
        return json.dumps(asdict(self), allow_nan=False)

    @classmethod
    def from_json(cls, value: str) -> Self:
        return cls(**json.loads(value))


@dataclass(frozen=True)
class BatchReceipt:
    key: str
    start: int
    end: int
    fingerprint: str


@dataclass(frozen=True)
class PublicationReceipt:
    recovery: RecoveryToken
    sha256: str
    progress: int
    num_rows: int
    complete: bool
    marker_error: dict[str, str] | None

    @classmethod
    def _from_json(cls, value: str) -> Self:
        data = json.loads(value)
        data["recovery"] = RecoveryToken(**data["recovery"])
        return cls(**data)


class ManifestTable:
    """One partition on native local or S3 storage, with retained saved generations.

    Reads and writes are synchronous. Storage must provide durable, atomic
    create-if-absent and consistent reads; capability support is not certification.
    """

    def __init__(
        self,
        root: str | os.PathLike[str],
        *,
        storage_options: Mapping[str, str | bool | int | float] | None = None,
        max_manifest_bytes: int = 8 * 1024 * 1024,
        max_part_bytes: int = 64 * 1024 * 1024,
        max_manifest_reads: int = 256,
    ) -> None:
        self._native = _rust.NativeManifestTable(
            os.fspath(root), _s3_options(storage_options), max_manifest_bytes, max_part_bytes, max_manifest_reads
        )

    def snapshot(self, generation: int | None = None) -> TableSnapshot:
        """Resolve latest publication or a saved generation without directory listing."""
        return TableSnapshot(self._native.snapshot(generation))

    def follow(
        self,
        *,
        start: FollowPosition | None = None,
        columns: Sequence[str] | None = None,
        batch_size: int = 1024,
        poll_interval: float = 0.1,
        idle_timeout: float | None = None,
    ) -> TableFollower:
        """Follow existing and newly published rows, or resume a saved position.

        Requires an already published table with row progress. A new follow starts
        at the latest generation. A resume visits its saved generation and each
        successor; replacement ends the follow. Storage calls are synchronous.
        """
        return TableFollower(
            self._native.follow(
                None if start is None else start.to_json(),
                None if columns is None else list(columns),
                batch_size,
                poll_interval,
                idle_timeout,
            )
        )

    def orphan_collector(self, *, retention_seconds: float, max_generations: int = 256, max_entries: int = 100_000) -> OrphanCollector:
        """Create a collector whose retention clock starts when it observes a file.

        Keep this object between sweeps. Restarting resets retention. All published
        versions are retained; files in an unpublished generation are left alone.
        """
        return OrphanCollector(self._native.orphan_collector(retention_seconds, max_generations, max_entries))

    def open_writer(
        self, schema: pa.Schema, *, mode: Literal["create", "append", "replace"] = "create", options: SinkOptions | None = None
    ) -> TableWriter:
        options = options or SinkOptions()
        native = self._native.open_writer(
            schema,
            mode,
            options.target_part_bytes,
            options.memory_limit,
            options.max_manifest_bytes,
            options.max_history_reads,
            json.dumps(dict(options.partition_values), allow_nan=False),
        )
        return TableWriter(native, schema)

    def recover(self, token: RecoveryToken, *, retry: bool = False, options: SinkOptions | None = None) -> PublicationReceipt:
        """Observe publication, or resubmit its exact saved candidate with retry=True."""
        options = options or SinkOptions()
        return PublicationReceipt._from_json(
            self._native.recover(
                token.to_json(),
                retry,
                options.target_part_bytes,
                options.memory_limit,
                options.max_manifest_bytes,
                options.max_history_reads,
            )
        )


@dataclass(frozen=True)
class FollowPosition:
    """Serializable checkpoint for the same source location and saved table generation."""

    source: str
    namespace_id: str
    table_id: str
    generation: int
    manifest_sha256: str
    progress: int
    progress_unit: str

    def to_json(self) -> str:
        return json.dumps(asdict(self), allow_nan=False)

    @classmethod
    def from_json(cls, value: str) -> Self:
        return cls(**json.loads(value))


class FollowEnd(StrEnum):
    COMPLETE = "complete"
    IDLE_TIMEOUT = "idle_timeout"
    CANCELLED = "cancelled"
    REPLACED = "replaced"


@dataclass(frozen=True)
class FollowBatch:
    batch: pa.RecordBatch
    position: FollowPosition


class TableFollower:
    """Iterator of batches and checkpoints. Inspect end_reason after exhaustion.

    Persist an item's position after processing its batch. Cancellation wakes
    polling waits, but cannot interrupt a synchronous storage call in progress.
    """

    def __init__(self, native: Any) -> None:
        self._native = native
        self.schema: pa.Schema = native.schema

    @property
    def position(self) -> FollowPosition:
        return FollowPosition.from_json(self._native.position)

    @property
    def end_reason(self) -> FollowEnd | None:
        value = self._native.end_reason
        return None if value is None else FollowEnd(value)

    def __iter__(self) -> Self:
        return self

    def __next__(self) -> FollowBatch:
        item = self._native.next_batch()
        if item is None:
            raise StopIteration
        batch, position = item
        return FollowBatch(batch, FollowPosition.from_json(position))

    def cancel(self) -> None:
        """Request cancellation; next iteration reports CANCELLED."""
        self._native.cancel()

    def close(self) -> None:
        self.cancel()

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *args: object) -> None:
        self.close()


@dataclass(frozen=True)
class CleanupReport:
    generation: int | None
    pending: int
    eligible: list[str]
    deleted: list[str]


class OrphanCollector:
    """Observe abandoned files and remove them after retention in occupied generations."""

    def __init__(self, native: Any) -> None:
        self._native = native

    def collect(self, *, dry_run: bool = True) -> CleanupReport:
        """Validate history, then list candidates; dry runs also track retention.

        Keys in the report are relative to the table root. Validation or listing
        failures prevent deletion. A deletion failure may leave a partial sweep;
        rerunning is safe. Failure resets retention observations.
        """
        return CleanupReport(**json.loads(self._native.collect(dry_run)))


class TableWriter:
    """Positioned Arrow writes; clean context exit commits, exceptional exit aborts."""

    def __init__(self, native: Any, schema: pa.Schema) -> None:
        self._native = native
        self.schema = schema
        self._closed = False

    @property
    def position(self) -> int:
        """Accepted rows, including unpublished work. Checkpoint receipt.progress instead."""
        return self._native.position

    @property
    def recovery_token(self) -> RecoveryToken:
        """Token for the current publication attempt; changes after a successful commit."""
        return RecoveryToken.from_json(self._native.recovery_token)

    def write_batch(self, batch: pa.RecordBatch, *, expected_position: int, key: str) -> BatchReceipt:
        data = json.loads(self._native.write_batch(batch, expected_position, key))
        return BatchReceipt(data["key"], data["progress"]["start"], data["progress"]["end"], data["fingerprint"])

    def flush(self) -> None:
        """Stage complete parts without publishing them."""
        self._native.flush()

    def commit(self) -> PublicationReceipt:
        """Publish accepted work and keep the table open."""
        return self._publish(False)

    def finish(self) -> PublicationReceipt:
        """Publish accepted work and mark the table complete."""
        return self._publish(True)

    def _publish(self, complete: bool) -> PublicationReceipt:
        try:
            receipt = PublicationReceipt._from_json(self._native.publish(complete))
        except Exception as error:
            error.__dict__["recovery_token"] = self.recovery_token
            raise
        self._closed = receipt.complete
        return receipt

    def resolve(self, *, retry: bool = False) -> PublicationReceipt:
        """Resolve an uncertain publication on this writer before continuing."""
        receipt = PublicationReceipt._from_json(self._native.resolve(retry))
        self._closed = receipt.complete
        return receipt

    def abort(self) -> None:
        """Discard unpublished work; earlier commits remain visible."""
        self._native.close()
        self._closed = True

    def close(self) -> None:
        """Abort pending work without completing the table."""
        self.abort()

    def __enter__(self) -> Self:
        if self._closed:
            raise ValueError("writer is closed")
        return self

    def __exit__(self, exc_type: type[BaseException] | None, exc: BaseException | None, traceback: TracebackType | None) -> None:
        if self._closed:
            return
        if exc is None:
            self.commit()
            self.close()
        else:
            try:
                self.abort()
            except OSError as cleanup_error:
                exc.add_note(f"Writer abort failed: {cleanup_error}. Resolve publication with the saved recovery token.")


class TableSnapshot:
    """Frozen published metadata and checksum-verified, demand-driven part reads."""

    def __init__(self, native: Any) -> None:
        self._native = native

    @property
    def schema(self) -> pa.Schema:
        return self._native.schema

    @property
    def state(self) -> dict[str, Any]:
        """Visible counts, generation, completion and progress from this manifest.

        Counts are exact. Unavailable timestamps/distinct values are absent;
        partition values provide cheap distinct.<column> entries.
        """
        return json.loads(self._native.state)

    @property
    def metadata(self) -> dict[str, Any]:
        """Return an independent copy of the validated manifest metadata."""
        return json.loads(self._native.metadata)

    @property
    def generation(self) -> int:
        return self.metadata["generation"]

    @property
    def num_rows(self) -> int:
        return self.metadata["num_rows"]

    @property
    def progress(self) -> int:
        return self.metadata["progress"]

    @property
    def complete(self) -> bool:
        return self.metadata["complete"]

    def scan(
        self,
        *,
        columns: Sequence[str] | None = None,
        row_offset: int = 0,
        batch_size: int = 1024,
        row_limit: int | None = None,
        byte_limit: int | None = None,
    ) -> DecodedBatchStream:
        """Read only listed parts, checking each encoded part before decoding it.

        Selection and output limits apply across parts. A part is read in full;
        max_part_bytes bounds encoded input, not decoder allocations.
        """
        schema, native = self._native.scan(None if columns is None else list(columns), batch_size, row_limit, byte_limit, row_offset)
        return DecodedBatchStream(schema, native)
