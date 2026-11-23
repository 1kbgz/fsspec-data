"""Compare request counts and elapsed time with a local S3-compatible range server.

Run after installing the project: python benchmarks/shared_s3_scans.py
"""

import argparse
import asyncio
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pyarrow as pa
import pyarrow.parquet as pq

from fsspec_data import S3ParquetSource


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rows", type=int, default=100_000)
    parser.add_argument("--scans", type=int, default=4)
    args = parser.parse_args()
    sink = pa.BufferOutputStream()
    pq.write_table(pa.table({"a": range(args.rows), "b": range(args.rows)}), sink, row_group_size=25_000, compression="NONE")
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

    async def consume(source):
        rows, checksum = 0, 0
        async with await source.scan_async("data.parquet", columns=["a", "b"]) as stream:
            async for batch in stream:
                rows += batch.num_rows
                checksum += sum(batch.column(0).to_pylist())
        assert rows == args.rows and checksum == args.rows * (args.rows - 1) // 2

    async def run():
        for cache_bytes in (0, 64 * 1024 * 1024):
            async with S3ParquetSource("bench", storage_options=options, cache_bytes=cache_bytes) as source:
                for run in ("cold", "repeat"):
                    with lock:
                        before = dict(counts)
                    start = time.perf_counter()
                    await asyncio.gather(*(consume(source) for _ in range(args.scans)))
                    elapsed = time.perf_counter() - start
                    with lock:
                        result = {key: value - before[key] for key, value in counts.items()}
                    print(json.dumps({"cache_bytes": cache_bytes, "run": run, "scans": args.scans, "seconds": round(elapsed, 4), **result}))

    try:
        asyncio.run(run())
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    main()
