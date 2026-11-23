import asyncio
import contextvars
import threading

import pyarrow as pa
import pyarrow.parquet as pq
import pytest
from fsspec import AbstractFileSystem

from fsspec_data import DataSource, FileDataSource, scan_parquet_async

TENANT = contextvars.ContextVar("async_source_tenant", default="unset")


def parquet_bytes():
    sink = pa.BufferOutputStream()
    table = pa.table({"a": range(12), "b": range(20, 32)}).replace_schema_metadata({"source": "foreign"})
    pq.write_table(table, sink, row_group_size=4)
    return sink.getvalue().to_pybytes()


class AsyncFile:
    def __init__(self, data, *, async_seek=False):
        self.data = data
        self.size = len(data)
        self.position = 0
        self.async_seek = async_seek
        self.calls = []
        self.read_gate = asyncio.Event()
        self.read_gate.set()
        self.read_started = asyncio.Event()
        self.cleanup_started = asyncio.Event()
        self.cleanup_gate = asyncio.Event()
        self.cleanup_gate.set()
        self.close_started = asyncio.Event()
        self.close_gate = asyncio.Event()
        self.close_gate.set()
        self.active = 0
        self.closed = 0
        self.error = None
        self.extra_bytes = False

    def record(self, method):
        self.calls.append((method, asyncio.get_running_loop(), threading.get_ident(), TENANT.get()))

    async def read(self, size):
        self.record("read")
        self.read_started.set()
        self.active += 1
        try:
            try:
                await self.read_gate.wait()
            except asyncio.CancelledError:
                self.cleanup_started.set()
                await self.cleanup_gate.wait()
                raise
            if self.error is not None:
                raise self.error
            if self.extra_bytes:
                return b"x" * (size + 1)
            data = self.data[self.position : self.position + size]
            self.position += len(data)
            return data
        finally:
            self.active -= 1

    def seek(self, offset, whence=0):
        self.record("seek")
        assert whence == 0
        if self.async_seek:
            return self.seek_async(offset)
        self.position = offset
        return offset

    async def seek_async(self, offset):
        await asyncio.sleep(0)
        self.position = offset
        return offset

    async def close(self):
        self.record("close")
        self.close_started.set()
        assert self.active == 0
        await self.close_gate.wait()
        self.closed += 1


class AsyncFS(AbstractFileSystem):
    cachable = False

    def __init__(self, data=None, *, async_seek=False):
        super().__init__()
        self.data = parquet_bytes() if data is None else data
        self.async_seek = async_seek
        self.files = []
        self.paths = []
        self.open_started = asyncio.Event()
        self.open_gate = asyncio.Event()
        self.open_gate.set()
        self.configure = lambda file: None

    def info(self, path, **kwargs):
        raise AssertionError("synchronous metadata must not be called")

    def open(self, path, mode="rb", block_size=None, cache_options=None, compression=None, **kwargs):
        raise AssertionError("synchronous open must not be called")

    async def open_async(self, path, mode="rb", **kwargs):
        assert mode == "rb"
        self.paths.append(path)
        file = AsyncFile(self.data, async_seek=self.async_seek)
        self.configure(file)
        self.files.append(file)
        self.open_started.set()
        try:
            await self.open_gate.wait()
        except asyncio.CancelledError:
            await file.close()
            raise
        return file


@pytest.mark.parametrize("async_seek", [False, True])
def test_file_source_selection_uses_original_loop_and_context(async_seek):
    async def run():
        fs = AsyncFS(async_seek=async_seek)
        token = TENANT.set("source-user")
        try:
            stream = await FileDataSource(fs).scan_async("orders.parquet", columns=["b", "a", "b"], row_offset=5, row_limit=2, batch_size=1)
            TENANT.set("other-user")
            assert stream.schema.metadata == {b"source": b"foreign"}
            async with stream:
                batches = [batch async for batch in stream]
            assert [batch.column(0).to_pylist() for batch in batches] == [[25], [26]]
            assert [batch.column(1).to_pylist() for batch in batches] == [[5], [6]]
            assert all(batch.column(0).equals(batch.column(2)) for batch in batches)
            (file,) = fs.files
            assert file.closed == 1
            assert fs.paths == ["orders.parquet"]
            assert {method for method, _, _, _ in file.calls} == {"read", "seek", "close"}
            assert all(
                loop is asyncio.get_running_loop() and thread == threading.get_ident() and tenant == "source-user"
                for _, loop, thread, tenant in file.calls
            )
            assert TENANT.get() == "other-user"
        finally:
            TENANT.reset(token)

    asyncio.run(run())


@pytest.mark.parametrize("during_open", [False, True])
def test_cancellation_waits_for_foreign_read_cleanup(during_open):
    async def run():
        fs = AsyncFS()
        if during_open:

            def configure(file):
                file.read_gate.clear()
                file.cleanup_gate.clear()

            fs.configure = configure
            stream = None
            reading = asyncio.create_task(FileDataSource(fs).scan_async("rows.parquet"))
            await fs.open_started.wait()
        else:
            stream = await FileDataSource(fs).scan_async("rows.parquet")
            fs.files[0].read_gate.clear()
            fs.files[0].cleanup_gate.clear()
            fs.files[0].read_started.clear()
            reading = asyncio.create_task(anext(stream))
        file = fs.files[0]
        await file.read_started.wait()
        reading.cancel()
        await file.cleanup_started.wait()
        assert not reading.done()
        assert not file.close_started.is_set()
        file.cleanup_gate.set()
        with pytest.raises(asyncio.CancelledError):
            await reading
        assert file.active == 0 and file.closed == 1
        if stream is not None:
            await stream.aclose()
            assert file.closed == 1

    asyncio.run(run())


def test_closing_scan_drains_read_before_closing_foreign_file():
    async def run():
        fs = AsyncFS()
        stream = await FileDataSource(fs).scan_async("rows.parquet")
        file = fs.files[0]
        file.read_gate.clear()
        file.cleanup_gate.clear()
        file.close_gate.clear()
        file.read_started.clear()
        reading = asyncio.create_task(anext(stream))
        await file.read_started.wait()
        closing = asyncio.create_task(stream.aclose())
        await file.cleanup_started.wait()
        assert not closing.done() and not file.close_started.is_set()
        file.cleanup_gate.set()
        await file.close_started.wait()
        closing.cancel()
        with pytest.raises(asyncio.CancelledError):
            await closing
        assert file.closed == 0
        file.close_gate.set()
        await stream.aclose()
        with pytest.raises(asyncio.CancelledError):
            await reading
        assert file.closed == 1

    asyncio.run(run())


def test_async_opener_is_cancelled_and_owns_partial_open_cleanup():
    async def run():
        fs = AsyncFS()
        fs.open_gate.clear()
        opening = asyncio.create_task(FileDataSource(fs).scan_async("rows.parquet"))
        await fs.open_started.wait()
        opening.cancel()
        with pytest.raises(asyncio.CancelledError):
            await opening
        assert fs.files[0].closed == 1

    asyncio.run(run())


@pytest.mark.parametrize("during_opener", [False, True])
def test_deadline_covers_opener_and_foreign_reads(during_opener):
    async def run():
        fs = AsyncFS()
        if during_opener:
            fs.open_gate.clear()
        else:
            fs.configure = lambda file: file.read_gate.clear()
        with pytest.raises(TimeoutError):
            await FileDataSource(fs).scan_async("rows.parquet", deadline=asyncio.get_running_loop().time() + 0.5)
        assert fs.files[0].closed == 1
        assert fs.files[0].active == 0

    asyncio.run(run())


def test_no_prefetch_and_scans_do_not_close_shared_filesystem():
    async def run():
        fs = AsyncFS()
        source = FileDataSource(fs)
        first, second = await asyncio.gather(source.scan_async("first.parquet", batch_size=4), source.scan_async("second.parquet"))
        file = fs.files[0]
        count = len(file.calls)
        await asyncio.sleep(0.02)
        assert len(file.calls) == count
        assert (await anext(first)).num_rows == 4
        count = len(file.calls)
        await asyncio.sleep(0.02)
        assert len(file.calls) == count
        await first.aclose()
        assert fs.files[1].closed == 0
        async with second:
            assert sum([batch.num_rows async for batch in second]) == 12
        assert all(file.closed == 1 for file in fs.files)

    asyncio.run(run())


def test_python_provider_error_identity_and_attributes_are_preserved():
    class ProviderError(OSError):
        kind = "conflict"
        retry_safe = False

    async def run():
        fs = AsyncFS()
        stream = await FileDataSource(fs).scan_async("rows.parquet")
        error = ProviderError("provider rejected a changed object")
        fs.files[0].error = error
        with pytest.raises(ProviderError) as raised:
            await anext(stream)
        assert raised.value is error
        assert raised.value.kind == "conflict" and raised.value.retry_safe is False
        assert fs.files[0].closed == 1

    asyncio.run(run())


@pytest.mark.parametrize("invalid", ["size", "read", "read_type", "oversized_subclass", "options"])
def test_invalid_provider_or_options_still_close_owned_file(invalid):
    async def run():
        fs = AsyncFS()
        if invalid == "size":
            fs.configure = lambda file: setattr(file, "size", None)
        elif invalid == "read":
            fs.configure = lambda file: setattr(file, "extra_bytes", True)
        elif invalid in {"read_type", "oversized_subclass"}:

            class MisreportedBytes(bytes):
                def __len__(self):
                    return 0

            async def read(size):
                return bytearray() if invalid == "read_type" else MisreportedBytes(b"x" * (size + 1))

            fs.configure = lambda file: setattr(file, "read", read)
        with pytest.raises((TypeError, ValueError)):
            await FileDataSource(fs).scan_async("rows.parquet", batch_size=0 if invalid == "options" else 1024)
        assert fs.files[0].closed == 1

    asyncio.run(run())


def test_aclose_provider_error_is_preserved_without_repeating_close():
    async def run():
        fs = AsyncFS()
        stream = await FileDataSource(fs).scan_async("rows.parquet")
        file = fs.files[0]
        error = OSError("provider close failed")

        async def aclose():
            file.closed += 1
            raise error

        file.aclose = aclose
        for _ in range(2):
            with pytest.raises(OSError) as raised:
                await stream.aclose()
            assert raised.value is error
        assert file.closed == 1

    asyncio.run(run())


def test_unsupported_sources_do_not_fall_back_to_sync_calls():
    async def run():
        with pytest.raises(NotImplementedError, match="data source"):
            await DataSource().scan_async("orders")
        sync_fs = AbstractFileSystem()
        with pytest.raises(NotImplementedError, match="open_async"):
            await FileDataSource(sync_fs).scan_async("rows.parquet")
        fs = AsyncFS()
        with pytest.raises(NotImplementedError, match="Parquet"):
            await FileDataSource(fs).scan_async("rows.csv")
        with pytest.raises(ValueError, match="mutually exclusive"):
            await scan_parquet_async("rows.parquet", fs=fs, storage_options={})
        assert fs.files == []

    asyncio.run(run())


def test_async_data_source_can_resolve_its_own_table_paths():
    async def run():
        fs = AsyncFS()

        class Catalog(DataSource):
            async def scan_async(self, path, **options):
                assert path == "orders"
                return await FileDataSource(fs).scan_async("private/part.parquet", **options)

        async with await Catalog().scan_async("orders", columns=["b"], row_limit=1) as stream:
            assert (await anext(stream)).column(0).to_pylist() == [20]
        assert fs.paths == ["private/part.parquet"]

    asyncio.run(run())
