"""Async Parquet batches from native S3 and asynchronous Python filesystems."""

from __future__ import annotations

import asyncio
import inspect
import math
import operator
from collections.abc import Mapping, Sequence
from contextlib import suppress
from importlib import import_module
from types import TracebackType
from typing import Any, Self

import pyarrow as pa
from fsspec import AbstractFileSystem

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


class _AsyncFileProvider:
    """Keep foreign file calls and cancellation cleanup on the caller's loop."""

    def __init__(self, file: Any) -> None:
        self.file: Any = file
        self._pending: set[asyncio.Task[Any]] = set()
        self._closed = False
        self._close_task: asyncio.Task[None] | None = None

    async def _call(self, method: str, argument: int) -> Any:
        if self._closed:
            raise ValueError("async file is closed")
        task = asyncio.current_task()
        assert task is not None
        self._pending.add(task)
        try:
            if method == "read":
                return await self.file.read(argument)
            position = self.file.seek(argument, 0)
            return await position if inspect.isawaitable(position) else position
        finally:
            self._pending.discard(task)

    async def _close(self) -> None:
        if self._close_task is None:
            self._closed = True
            self._close_task = asyncio.create_task(self._finish_close())
        await asyncio.shield(self._close_task)

    async def _finish_close(self) -> None:
        pending = list(self._pending)
        for task in pending:
            task.cancel()
        await asyncio.gather(*pending, return_exceptions=True)
        try:
            close = getattr(self.file, "aclose", None) or self.file.close
            await close()
        finally:
            self.file = None


async def scan_parquet_async(
    url: str,
    *,
    fs: AbstractFileSystem | None = None,
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
    """Open a Parquet scan on native S3 or a filesystem with ``open_async``.

    ``deadline`` uses asyncio loop time. With ``fs``, only the opened file is owned
    by the scan; its reads and close must be awaitable, its size known, and seeks
    must be asynchronous or operate on a cached cursor without I/O.
    """
    if deadline is not None and not math.isfinite(deadline):
        raise ValueError("deadline must be finite")
    loop = asyncio.get_running_loop()
    arguments = (
        None if columns is None else list(columns),
        row_offset,
        batch_size,
        row_limit,
        byte_limit,
        max_range_bytes,
        max_batch_bytes,
        read_chunk_size,
    )
    if fs is not None:
        if storage_options is not None:
            raise ValueError("fs and storage_options are mutually exclusive")
        opener = getattr(fs, "open_async", None)
        if not callable(opener):
            raise NotImplementedError("filesystem does not support open_async")
        async with asyncio.timeout_at(deadline):
            file = await opener(url, mode="rb")
        provider = _AsyncFileProvider(file)
        try:
            size = operator.index(file.size)
            if size < 0:
                raise ValueError("async file size must be nonnegative")
            timeout = None if deadline is None else max(0.0, deadline - loop.time())
            native = _rust.async_parquet_file(provider, size, *arguments, timeout)
        except BaseException:
            with suppress(Exception, asyncio.CancelledError):
                await asyncio.shield(provider._close())
            raise
    else:
        protocol, separator, path = url.partition("://")
        if not separator or protocol not in {"s3", "s3-rs"}:
            raise ValueError("async Parquet scans require an s3:// or s3-rs:// URL, or an explicit fs")
        bucket, _, key = path.partition("/")
        if not bucket or not key:
            raise ValueError("S3 URL must include a bucket and object key")
        options = {}
        for name, value in (storage_options or {}).items():
            if not isinstance(value, (str, bool, int, float)):
                raise TypeError("storage_options values must be strings, booleans or numbers")
            options[name] = str(value).lower() if isinstance(value, bool) else str(value)
        timeout = None if deadline is None else max(0.0, deadline - loop.time())
        native = _rust.async_parquet_scan(bucket, f"s3://{bucket}/{key}", options, *arguments, timeout)
    try:
        schema = await native.open()
    except BaseException:
        with suppress(Exception, asyncio.CancelledError):
            await asyncio.shield(native.close())
        raise
    return AsyncDecodedBatchStream(schema, native)
