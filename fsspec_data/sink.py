"""Structural contracts for configured table destinations and positioned writers."""

from __future__ import annotations

from types import TracebackType
from typing import Literal, Protocol, Self, TypeVar, runtime_checkable

import pyarrow as pa

_BatchReceipt_co = TypeVar("_BatchReceipt_co", covariant=True)
_PublicationReceipt_co = TypeVar("_PublicationReceipt_co", covariant=True)
_RecoveryToken_co = TypeVar("_RecoveryToken_co", covariant=True)
_Options_contra = TypeVar("_Options_contra", contravariant=True)


@runtime_checkable
class DataWriter(Protocol[_BatchReceipt_co, _PublicationReceipt_co, _RecoveryToken_co]):
    """Positioned Arrow writes; receipts and recovery stay backend-specific.

    Exact retries must not duplicate data. Forward positions are gaps; conflicting
    positions or key/payload reuse are conflicts. commit publishes and keeps the
    writer open; finish publishes and completes it. abort/close/drop discard pending
    work without undoing earlier commits. Clean context exit commits then closes;
    an exception aborts. Uncertain publications retain backend recovery information.
    """

    @property
    def schema(self) -> pa.Schema: ...

    @property
    def progress_unit(self) -> str:
        """Unit of expected_position and position, independently of output row counts."""
        ...

    @property
    def position(self) -> int:
        """Accepted progress, including unpublished work; not a durable checkpoint."""
        ...

    @property
    def recovery_token(self) -> _RecoveryToken_co:
        """Persist before publication; identifies the current attempt, not credentials."""
        ...

    def resolve(self, *, retry: bool = False) -> _PublicationReceipt_co:
        """Observe or retry the same uncertain publication; never append it again."""
        ...

    def write_batch(self, batch: pa.RecordBatch, *, expected_position: int, key: str) -> _BatchReceipt_co: ...

    def flush(self) -> None:
        """Stage pending data without publishing or completing the table."""
        ...

    def commit(self) -> _PublicationReceipt_co: ...
    def finish(self) -> _PublicationReceipt_co: ...
    def abort(self) -> None: ...
    def close(self) -> None: ...
    def __enter__(self) -> Self: ...
    def __exit__(self, exc_type: type[BaseException] | None, exc: BaseException | None, traceback: TracebackType | None) -> None: ...


_Writer_co = TypeVar("_Writer_co", bound=DataWriter[object, object, object], covariant=True)


@runtime_checkable
class DataSink(Protocol[_Options_contra, _Writer_co]):
    """One configured table destination; existing ManifestTable conforms structurally.

    Options and writer receipts are backend-specific. Implementations reject unsupported
    modes/options before writes. No inheritance or process-global registration is required.
    A protocol check verifies interface shape, not publication guarantees.
    """

    def open_writer(
        self,
        schema: pa.Schema,
        *,
        mode: Literal["create", "append", "replace"] = "create",
        options: _Options_contra | None = None,
    ) -> _Writer_co: ...
