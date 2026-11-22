import asyncio
import select
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from fsspec_data import AsyncDecodedBatchStream, scan_parquet_async


@pytest.fixture
def store():
    table = pa.table({"a": range(12), "b": range(20, 32), "unused": range(40, 52)}).replace_schema_metadata({"origin": "test"})
    sink = pa.BufferOutputStream()
    pq.write_table(table, sink, row_group_size=4, compression="NONE")
    state = SimpleNamespace(
        data=sink.getvalue().to_pybytes(),
        etag='"v1"',
        requests=[],
        paths=[],
        block=None,
        started=threading.Event(),
        peer_closed=threading.Event(),
        release=threading.Event(),
        status=200,
    )

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format, *args):
            pass

        def do_GET(self):
            request = (self.command, self.headers.get("Range"), self.headers.get("If-Match"))
            state.requests.append(request)
            state.paths.append(self.path)
            if state.block == self.command:
                state.started.set()
                while not state.release.wait(0.01):
                    try:
                        if select.select([self.connection], [], [], 0)[0] and not self.connection.recv(1):
                            state.peer_closed.set()
                            return
                    except (ConnectionResetError, ConnectionAbortedError):
                        state.peer_closed.set()
                        return
            status = state.status
            if self.headers.get("If-Match") not in (None, state.etag):
                status = 412
            if status != 200:
                self.send_response(status)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            data = state.data
            start, end = 0, len(data) - 1
            if request[1] is not None:
                start, end = map(int, request[1].removeprefix("bytes=").split("-"))
                status = 206
            self.send_response(status)
            self.send_header("Content-Length", str(end - start + 1))
            self.send_header("ETag", state.etag)
            self.send_header("Last-Modified", "Sun, 22 Nov 2026 12:00:00 GMT")
            if status == 206:
                self.send_header("Content-Range", f"bytes {start}-{end}/{len(data)}")
            self.end_headers()
            if self.command == "GET":
                try:
                    self.wfile.write(data[start : end + 1])
                except (BrokenPipeError, ConnectionResetError):
                    state.peer_closed.set()

        do_HEAD = do_GET

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=lambda: server.serve_forever(poll_interval=0.01), daemon=True)
    thread.start()
    state.options = {"anon": True, "region": "us-east-1", "endpoint_url": f"http://127.0.0.1:{server.server_port}"}
    try:
        yield state
    finally:
        state.release.set()
        server.shutdown()
        server.server_close()
        thread.join()


async def until(predicate):
    async with asyncio.timeout(5):
        while not predicate():
            await asyncio.sleep(0.001)


async def scan(store, **kwargs):
    return await scan_parquet_async("s3://test/rows.parquet", storage_options=store.options, **kwargs)


def test_selection_schema_and_only_requested_column_reads(store):
    async def run():
        stream = await scan(store, columns=["b", "a", "b"], row_offset=5, row_limit=2, batch_size=1)
        assert stream.schema.names == ["b", "a", "b"]
        assert stream.schema.metadata == {b"origin": b"test"}
        async with stream:
            batches = [batch async for batch in stream]
        assert [batch.column(0).to_pylist() for batch in batches] == [[25], [26]]
        assert [batch.column(1).to_pylist() for batch in batches] == [[5], [6]]
        assert all(batch.column(0).equals(batch.column(2)) for batch in batches)
        assert all(batch.schema == stream.schema for batch in batches)
        await stream.aclose()
        with pytest.raises(StopAsyncIteration):
            await anext(stream)
        assert all(condition == '"v1"' for method, _, condition in store.requests if method == "GET")

        stream = await scan(store, columns=["b"], row_offset=5, row_limit=2)
        before = len(store.requests)
        async with stream:
            assert (await anext(stream)).column(0).to_pylist() == [25, 26]
        column = pq.ParquetFile(pa.BufferReader(store.data)).metadata.row_group(1).column(1)
        start = column.dictionary_page_offset
        assert store.requests[before:] == [("GET", f"bytes={start}-{start + column.total_compressed_size - 1}", '"v1"')]

    asyncio.run(run())


def test_slow_consumer_does_not_prefetch(store):
    async def run():
        async with await scan(store, batch_size=4) as stream:
            before = len(store.requests)
            await asyncio.sleep(0.02)
            assert len(store.requests) == before
            assert (await anext(stream)).num_rows == 4
            after = len(store.requests)
            assert after > before
            await asyncio.sleep(0.02)
            assert len(store.requests) == after

    asyncio.run(run())


@pytest.mark.parametrize("during_open", [False, True])
def test_task_cancellation_closes_pending_network_read(store, during_open):
    async def run():
        stream = None if during_open else await scan(store)
        store.block = "HEAD" if during_open else "GET"
        if during_open:
            task = asyncio.create_task(scan(store))
        else:
            assert stream is not None
            task = asyncio.create_task(anext(stream))
        await until(store.started.is_set)
        # The caller's event loop remains runnable while native I/O waits.
        heartbeat = []
        asyncio.get_running_loop().call_soon(heartbeat.append, True)
        await asyncio.sleep(0)
        assert heartbeat == [True]
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        await until(store.peer_closed.is_set)
        if stream is not None:
            await stream.aclose()
            with pytest.raises(StopAsyncIteration):
                await anext(stream)

    asyncio.run(run())


@pytest.mark.parametrize("during_open", [False, True])
def test_deadline_stops_io_and_raises_timeout(store, during_open):
    async def run():
        deadline = asyncio.get_running_loop().time() + 1
        if during_open:
            store.block = "HEAD"
            with pytest.raises(TimeoutError):
                await scan(store, deadline=deadline)
        else:
            stream = await scan(store, deadline=deadline)
            store.block = "GET"
            with pytest.raises(TimeoutError):
                await anext(stream)
            await stream.aclose()
        await until(store.peer_closed.is_set)

    asyncio.run(run())


def test_close_cancels_pending_next_and_rejects_overlapping_reads(store):
    async def run():
        stream = await scan(store)
        store.block = "GET"
        pending = asyncio.create_task(anext(stream))
        await until(store.started.is_set)
        with pytest.raises(RuntimeError, match="already pending"):
            await anext(stream)
        await stream.aclose()
        with pytest.raises(asyncio.CancelledError):
            await pending
        await until(store.peer_closed.is_set)
        with pytest.raises(StopAsyncIteration):
            await anext(stream)

    asyncio.run(run())


def test_context_manager_exception_closes_stream(store):
    async def run():
        stream = await scan(store)
        with pytest.raises(LookupError, match="consumer failure"):
            async with stream:
                await anext(stream)
                raise LookupError("consumer failure")
        await stream.aclose()
        with pytest.raises(StopAsyncIteration):
            await anext(stream)

    asyncio.run(run())


@pytest.mark.parametrize("options", [{"max_range_bytes": 8}, {"max_batch_bytes": 1}, {"byte_limit": 1}])
def test_buffer_limits_report_failure_and_cleanup(store, options):
    async def run():
        with pytest.raises(ValueError, match="byte limit"):
            async with await scan(store, **options) as stream:
                await anext(stream)

    asyncio.run(run())


def test_storage_failures_preserve_kind_and_retry_safety(store):
    async def run():
        store.status = 404
        with pytest.raises(FileNotFoundError) as missing:
            await scan(store)
        assert vars(missing.value)["kind"] == "not_found"
        assert isinstance(vars(missing.value)["retry_safe"], bool)
        store.status = 200
        stream = await scan(store)
        store.etag = '"v2"'
        with pytest.raises(OSError) as conflict:
            await anext(stream)
        assert vars(conflict.value)["kind"] == "conflict"
        assert vars(conflict.value)["retry_safe"] is False
        await stream.aclose()

    asyncio.run(run())


@pytest.mark.parametrize("kwargs", [{"row_limit": 0}, {"columns": [], "row_offset": 5, "row_limit": 3}])
def test_empty_selection_and_zero_rows(store, kwargs):
    async def run():
        async with await scan(store, **kwargs) as stream:
            batches = [batch async for batch in stream]
        assert sum(batch.num_rows for batch in batches) == kwargs["row_limit"]
        if "columns" in kwargs:
            assert all(batch.num_columns == 0 for batch in batches)

    asyncio.run(run())


@pytest.mark.parametrize("kwargs", [{"read_chunk_size": 0}, {"read_chunk_size": 9 * 1024 * 1024}, {"batch_size": 0}, {"deadline": float("nan")}])
def test_invalid_options_do_not_contact_storage(store, kwargs):
    async def run():
        with pytest.raises(ValueError):
            await scan(store, **kwargs)
        assert store.requests == []

    asyncio.run(run())


def test_key_with_bucket_prefix_is_not_stripped_twice(store):
    async def run():
        async with await scan_parquet_async("s3-rs://test/test/rows.parquet", storage_options=store.options) as stream:
            assert (await anext(stream)).num_rows > 0
        assert store.paths and set(store.paths) == {"/test/test/rows.parquet"}

    asyncio.run(run())


def test_cancelling_close_waiter_does_not_cancel_cleanup():
    async def run():
        cleanup = asyncio.get_running_loop().create_future()
        calls = []

        class Native:
            def close(self):
                calls.append(True)
                return cleanup

        stream = AsyncDecodedBatchStream(pa.schema([]), Native())
        closing = asyncio.create_task(stream.aclose())
        await asyncio.sleep(0)
        assert calls == [True]
        closing.cancel()
        with pytest.raises(asyncio.CancelledError):
            await closing
        assert not cleanup.cancelled()
        cleanup.set_result(None)
        await stream.aclose()
        assert calls == [True]

    asyncio.run(run())


def test_ready_batch_is_not_delivered_after_close():
    async def run():
        ready = asyncio.get_running_loop().create_future()
        closed = asyncio.get_running_loop().create_future()
        closed.set_result(None)

        class Native:
            def next(self):
                return ready

            def close(self):
                return closed

        stream = AsyncDecodedBatchStream(pa.schema([("a", pa.int64())]), Native())
        reading = asyncio.create_task(anext(stream))
        await asyncio.sleep(0)
        ready.set_result(pa.record_batch({"a": [1]}))
        await stream.aclose()
        with pytest.raises(StopAsyncIteration):
            await reading

    asyncio.run(run())


def test_deadline_includes_idle_consumer_time(store):
    async def run():
        async with await scan(store, deadline=asyncio.get_running_loop().time() + 0.5) as stream:
            requests = len(store.requests)
            await asyncio.sleep(0.55)
            with pytest.raises(TimeoutError):
                await anext(stream)
            assert len(store.requests) == requests

    asyncio.run(run())


def test_independent_scans_continue_after_one_is_closed(store):
    async def run():
        first, second = await asyncio.gather(scan(store), scan(store))
        await first.aclose()
        async with second:
            assert sum([batch.num_rows async for batch in second]) == 12

    asyncio.run(run())
