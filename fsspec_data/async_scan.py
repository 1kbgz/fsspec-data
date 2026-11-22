"""Async Parquet batches read through native fsspec-rs S3 handles."""

from __future__ import annotations

import asyncio
import math
from collections.abc import Mapping, Sequence
from contextlib import suppress
from importlib import import_module
from types import TracebackType
from typing import Any, Self

import pyarrow as pa

_rust = import_module(".fsspec_data", __package__)


class AsyncDecodedBatchStream:
    """Demand-driven Arrow batches; use ``aclose`` or an async context manager."""

    def __init__(self, schema: pa.Schema, native: Any) -> None:
        self.schema = schema
        self._native = native
        self._closed = False
        self._pending = False
        self._close_waiter: asyncio.Future[None] | None = None

    def __aiter__(self) -> AsyncDecodedBatchStream:
        return self

    async def __anext__(self) -> pa.RecordBatch:
        if self._closed:
            raise StopAsyncIteration
        if self._pending:
            raise RuntimeError("another batch request is already pending")
        self._pending = True
        try:
            batch = await self._native.next()
        except BaseException:
            # Preserve the scan error; cleanup continues if cancelled again.
            with suppress(Exception, asyncio.CancelledError):
                await self.aclose()
            raise
        finally:
            self._pending = False
        if self._closed or batch is None:
            await self.aclose()
            raise StopAsyncIteration
        return batch

    async def aclose(self) -> None:
        """Cancel reads and wait for cleanup. Cleanup continues if this await is cancelled."""
        self._closed = True
        if self._close_waiter is None:
            self._close_waiter = self._native.close()
        await asyncio.shield(self._close_waiter)

    async def __aenter__(self) -> Self:
        if self._closed:
            raise RuntimeError("scan is closed")
        return self

    async def __aexit__(self, exc_type: type[BaseException] | None, exc: BaseException | None, traceback: TracebackType | None) -> None:
        if exc is None:
            await self.aclose()
        else:
            with suppress(Exception, asyncio.CancelledError):
                await self.aclose()


async def scan_parquet_async(
    url: str,
    *,
    storage_options: Mapping[str, str | bool | int | float] | None = None,
    columns: Sequence[str] | None = None,
    row_offset: int = 0,
    batch_size: int = 1024,
    row_limit: int | None = None,
    byte_limit: int | None = None,
    max_range_bytes: int = 64 * 1024 * 1024,
    max_batch_bytes: int = 64 * 1024 * 1024,
    read_chunk_size: int = 1024 * 1024,
    deadline: float | None = None,
) -> AsyncDecodedBatchStream:
    """Open a native S3 Parquet scan. ``deadline`` uses ``asyncio`` loop time.

    ``url`` is ``s3://bucket/key`` or ``s3-rs://bucket/key``. Storage options use
    fsspec-rs's flat names, such as ``anon``, ``region`` and ``endpoint_url``.
    The stream owns its file. Decoding allocations are outside the buffer limits.
    """
    protocol, separator, path = url.partition("://")
    if not separator or protocol not in {"s3", "s3-rs"}:
        raise ValueError("async Parquet scans require an s3:// or s3-rs:// URL")
    bucket, _, key = path.partition("/")
    if not bucket or not key:
        raise ValueError("S3 URL must include a bucket and object key")
    options = {}
    for name, value in (storage_options or {}).items():
        if not isinstance(value, (str, bool, int, float)):
            raise TypeError("storage_options values must be strings, booleans or numbers")
        options[name] = str(value).lower() if isinstance(value, bool) else str(value)
    if deadline is not None and not math.isfinite(deadline):
        raise ValueError("deadline must be finite")
    timeout = None if deadline is None else max(0.0, deadline - asyncio.get_running_loop().time())
    native = _rust.async_parquet_scan(
        bucket,
        f"s3://{bucket}/{key}",
        options,
        None if columns is None else list(columns),
        row_offset,
        batch_size,
        row_limit,
        byte_limit,
        max_range_bytes,
        max_batch_bytes,
        read_chunk_size,
        timeout,
    )
    try:
        schema = await native.open()
    except BaseException:
        with suppress(Exception, asyncio.CancelledError):
            await asyncio.shield(native.close())
        raise
    return AsyncDecodedBatchStream(schema, native)
