"""Benchmark shared scans against a local S3-compatible range server."""

import asyncio
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from fsspec_data import S3ParquetSource


@pytest.fixture
def s3_store():
    sink = pa.BufferOutputStream()
    pq.write_table(pa.table({"a": range(100_000), "b": range(100_000)}), sink, row_group_size=25_000, compression="NONE")
    data = sink.getvalue().to_pybytes()
    counts = {"HEAD": 0, "GET": 0, "bytes": 0}
    lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format, *args):
            pass

        def do_GET(self):
            start, end = 0, len(data) - 1
            bounds = self.headers.get("Range")
            if bounds:
                start, end = map(int, bounds.removeprefix("bytes=").split("-"))
            assert self.headers.get("If-Match") in (None, '"benchmark"')
            self.send_response(206 if bounds else 200)
            self.send_header("ETag", '"benchmark"')
            self.send_header("Last-Modified", "Sun, 22 Nov 2026 12:00:00 GMT")
            self.send_header("Content-Length", str(end - start + 1))
            if bounds:
                self.send_header("Content-Range", f"bytes {start}-{end}/{len(data)}")
            self.end_headers()
            with lock:
                counts[self.command] += 1
                counts["bytes"] += end - start + 1 if self.command == "GET" else 0
            if self.command == "GET":
                self.wfile.write(data[start : end + 1])

        do_HEAD = do_GET

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    options = {"anon": True, "endpoint_url": f"http://127.0.0.1:{server.server_port}", "region": "us-east-1"}

    try:
        yield options, counts, lock
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


@pytest.mark.benchmark(group="shared-s3-scans")
@pytest.mark.parametrize("cache_bytes", [0, 64 * 1024 * 1024], ids=["uncached", "shared"])
@pytest.mark.parametrize("cache_state", ["cold", "warm"])
def test_shared_s3_scans(benchmark, s3_store, cache_bytes, cache_state):
    options, counts, lock = s3_store
    rows, scans = 100_000, 4
    source = None
    rounds = []

    async def consume():
        async def scan():
            assert source is not None
            count, checksum = 0, 0
            async with await source.scan_async("data.parquet", columns=["a", "b"]) as stream:
                async for batch in stream:
                    count += batch.num_rows
                    checksum += sum(batch.column(0).to_pylist())
            assert count == rows and checksum == rows * (rows - 1) // 2

        await asyncio.gather(*(scan() for _ in range(scans)))

    async def close():
        assert source is not None
        await source.aclose()

    with asyncio.Runner() as runner:

        def setup():
            nonlocal source
            source = S3ParquetSource("bench", storage_options=options, cache_bytes=cache_bytes)
            if cache_state == "warm":
                runner.run(consume())
            with lock:
                counts.update(HEAD=0, GET=0, bytes=0)

        def teardown():
            nonlocal source
            with lock:
                rounds.append(dict(counts))
            runner.run(close())
            source = None

        try:
            # Fresh source per round keeps cold rounds cold; warming is outside timing.
            benchmark.pedantic(lambda: runner.run(consume()), setup=setup, teardown=teardown, rounds=5, iterations=1)
        finally:
            # pytest-benchmark skips teardown when benchmarking is disabled.
            if source is not None:
                teardown()

    assert all(result["HEAD"] == scans for result in rounds)
    if cache_state == "warm" and cache_bytes:
        assert all(result["GET"] == result["bytes"] == 0 for result in rounds)
    else:
        assert all(result["GET"] > 0 and result["bytes"] > 0 for result in rounds)
    benchmark.extra_info.update(rows=rows, scans=scans, requests_per_round=rounds)
