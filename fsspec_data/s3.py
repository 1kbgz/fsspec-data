"""Configured native S3 sources for asynchronous Parquet scans."""

from __future__ import annotations

import asyncio
import math
from collections.abc import Mapping, Sequence
from contextlib import suppress
from types import TracebackType
from typing import Self

from .async_scan import AsyncDecodedBatchStream, _open_scan, _rust, _s3_options
from .describe import DataSource


class S3ParquetSource(DataSource):
    """Async Parquet source retaining one native client and credential-context identity.

    Use an async context manager or ``aclose`` to close its scans and client. Other
    DataSource operations are unsupported. Recreate after changing configuration or
    effective authorization; requests within this source retain its initial client.
    ``cache_bytes`` bounds pending and retained shared ranges; zero disables the cache.
    """

    def __init__(
        self, bucket: str, *, storage_options: Mapping[str, str | bool | int | float] | None = None, cache_bytes: int = 64 * 1024 * 1024
    ) -> None:
        self._bucket = bucket
        self._native = _rust.NativeS3ParquetSource(bucket, _s3_options(storage_options), cache_bytes)
        self._close_waiter: asyncio.Future[None] | None = None

    async def scan_async(
        self,
        path: str,
        *,
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
        if self._close_waiter is not None:
            raise RuntimeError("source is closed")
        if deadline is not None and not math.isfinite(deadline):
            raise ValueError("deadline must be finite")
        if path.startswith("s3-rs://"):
            path = "s3://" + path.removeprefix("s3-rs://")
        elif not path.startswith("s3://"):
            path = f"s3://{self._bucket}/{path}"
        timeout = None if deadline is None else max(0.0, deadline - asyncio.get_running_loop().time())
        native = self._native.scan(
            path,
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
        return await _open_scan(native, self)

    async def aclose(self) -> None:
        """Cancel and drain this source's scans, then close its filesystem and context."""
        if self._close_waiter is None:
            self._close_waiter = self._native.close()
        await asyncio.shield(self._close_waiter)

    async def __aenter__(self) -> Self:
        if self._close_waiter is not None:
            raise RuntimeError("source is closed")
        return self

    async def __aexit__(self, exc_type: type[BaseException] | None, exc: BaseException | None, traceback: TracebackType | None) -> None:
        if exc is None:
            await self.aclose()
        else:
            with suppress(Exception, asyncio.CancelledError):
                await self.aclose()
