import gc
import hashlib
import json
import threading
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from fsspec_data import ManifestTable, RecoveryToken, SinkOptions


def batch(start=0, count=5):
    return pa.record_batch({"a": range(start, start + count), "b": [str(i) for i in range(start, start + count)]})


def values(snapshot, **options):
    stream = snapshot.scan(**options)
    return pa.Table.from_batches(list(stream), schema=stream.schema).to_pydict()


def test_publication_append_finish_and_saved_generations(tmp_path):
    table = ManifestTable(tmp_path)
    writer = table.open_writer(batch().schema)
    token = RecoveryToken.from_json(writer.recovery_token.to_json())
    first = writer.write_batch(batch(), expected_position=0, key="first")
    assert first.start == 0 and first.end == 5 and writer.position == 5
    writer.flush()
    with pytest.raises(FileNotFoundError):
        table.snapshot()
    published = writer.commit()
    assert published.recovery == token and published.progress == published.num_rows == 5
    assert not published.complete and published.marker_error is None
    old = table.snapshot()
    assert old.generation == 0 and not old.complete
    assert writer.recovery_token.generation == 1
    writer.write_batch(batch(5), expected_position=5, key="second")
    final = writer.finish()
    assert final.complete and final.progress == 10
    assert writer.finish() == final
    assert table.snapshot().generation == 1
    assert values(old)["a"] == values(table.snapshot(0))["a"] == list(range(5))
    assert values(table.snapshot())["a"] == list(range(10))
    assert (tmp_path / "manifests/00000000000000000001/_SUCCESS").exists()
    with pytest.raises(OSError) as error:
        table.open_writer(batch().schema, mode="append")
    assert getattr(error.value, "kind", None) == "conflict"
    with pytest.raises(FileExistsError):
        table.open_writer(batch().schema)
    with table.open_writer(batch(100).schema, mode="replace") as replacement:
        replacement.write_batch(batch(100), expected_position=0, key="first")
    assert values(table.snapshot())["a"] == list(range(100, 105))
    assert values(old)["a"] == list(range(5))
    assert table.snapshot().metadata["table_id"] != old.metadata["table_id"]


def test_retry_keys_positions_and_reopen(tmp_path):
    table = ManifestTable(tmp_path)
    with table.open_writer(batch().schema) as writer:
        receipt = writer.write_batch(batch(), expected_position=0, key="first")
        assert writer.write_batch(batch(), expected_position=0, key="first") == receipt
        for position, key, data, kind in [(8, "gap", batch(), "gap"), (0, "different", batch(), "conflict"), (0, "first", batch(10), "conflict")]:
            with pytest.raises(OSError) as error:
                writer.write_batch(data, expected_position=position, key=key)
            assert getattr(error.value, "kind", None) == kind and getattr(error.value, "retry_safe", None) is False
        assert writer.position == 5
    with ManifestTable(tmp_path).open_writer(batch().schema, mode="append") as writer:
        assert writer.write_batch(batch(), expected_position=0, key="first") == receipt
        writer.write_batch(batch(5), expected_position=5, key="second")
    assert values(table.snapshot())["a"] == list(range(10))
    assert not table.snapshot().complete


def test_abort_close_drop_and_context_exception_preserve_prior_commits(tmp_path):
    table = ManifestTable(tmp_path)
    with pytest.raises(RuntimeError, match="stop"), table.open_writer(batch().schema) as writer:
        writer.write_batch(batch(), expected_position=0, key="first")
        writer.commit()
        writer.write_batch(batch(5), expected_position=5, key="discard")
        raise RuntimeError("stop")
    assert values(table.snapshot())["a"] == list(range(5))
    for method in ("abort", "close", "drop"):
        writer = table.open_writer(batch().schema, mode="append")
        writer.write_batch(batch(5), expected_position=5, key=method)
        writer.flush()
        if method != "drop":
            getattr(writer, method)()
        del writer
        gc.collect()
        assert table.snapshot().generation == 0
    assert not table.snapshot().complete
    empty = ManifestTable(tmp_path / "unpublished")
    with pytest.raises(RuntimeError), empty.open_writer(batch().schema) as writer:
        writer.write_batch(batch(), expected_position=0, key="x")
        raise RuntimeError()
    with pytest.raises(FileNotFoundError):
        empty.snapshot()


def test_concurrent_publishers_only_one_wins(tmp_path):
    table = ManifestTable(tmp_path)
    writers = [table.open_writer(batch().schema) for _ in range(2)]
    for i, writer in enumerate(writers):
        writer.write_batch(batch(i * 10), expected_position=0, key="first")
    barrier = threading.Barrier(2)

    def commit(writer):
        barrier.wait()
        try:
            return writer.commit()
        except OSError as error:
            return error

    with ThreadPoolExecutor(2) as pool:
        results = list(pool.map(commit, writers))
    errors = [result for result in results if isinstance(result, OSError)]
    assert len(errors) == 1 and errors[0].kind == "conflict"
    assert values(table.snapshot())["a"] in [list(range(5)), list(range(10, 15))]
    assert table.snapshot().generation == 0
    for writer in writers:
        writer.close()


def test_snapshot_scan_across_parts_and_limits(tmp_path):
    table = ManifestTable(tmp_path)
    with table.open_writer(batch().schema, options=SinkOptions(partition_values={"day": "2026-11-22"})) as writer:
        for i in range(3):
            writer.write_batch(batch(i * 5), expected_position=i * 5, key=str(i))
            writer.flush()
    snapshot = table.snapshot()
    assert len(snapshot.metadata["parts"]) == 3
    assert snapshot.schema == batch().schema
    assert snapshot.metadata["partition_values"] == {"day": "2026-11-22"}
    data = snapshot.scan(columns=["b", "a", "b"], row_offset=3, row_limit=8, batch_size=2).collect()
    assert data.schema.names == ["b", "a", "b"]
    assert [v for b in data.batches for v in b.column(1).to_pylist()] == list(range(3, 11))
    assert all(b.num_rows <= 2 for b in data.batches)
    assert list(snapshot.scan(row_limit=0)) == []
    assert list(snapshot.scan(row_offset=100)) == []
    assert sum(b.num_rows for b in snapshot.scan(columns=[])) == 15
    with pytest.raises(ValueError):
        list(snapshot.scan(byte_limit=1))
    with pytest.raises(ValueError):
        snapshot.scan(batch_size=0)
    with pytest.raises(ValueError):
        snapshot.scan(columns=["missing"])
    stream = snapshot.scan()
    stream.cancel()
    with pytest.raises(RuntimeError, match="cancelled"):
        next(stream)
    altered = snapshot.metadata
    altered["parts"].clear()
    assert len(snapshot.metadata["parts"]) == 3


def test_parts_checked_on_demand_and_orphans_ignored(tmp_path):
    table = ManifestTable(tmp_path)
    with table.open_writer(batch().schema) as writer:
        writer.write_batch(batch(), expected_position=0, key="first")
        writer.flush()
        writer.write_batch(batch(5), expected_position=5, key="second")
    snapshot = table.snapshot()
    (tmp_path / "unlisted.parquet").write_bytes(b"not data")
    second = tmp_path / snapshot.metadata["parts"][1]["key"]
    second.write_bytes(b"broken")
    assert values(snapshot, row_limit=5)["a"] == list(range(5))
    with pytest.raises(OSError) as error:
        values(snapshot)
    assert getattr(error.value, "kind", None) == "corrupt"
    with pytest.raises(ValueError) as error:
        values(ManifestTable(tmp_path, max_part_bytes=1).snapshot())
    assert getattr(error.value, "kind", None) == "invalid"


def test_parquet_schema_checked_against_manifest(tmp_path):
    table = ManifestTable(tmp_path)
    with table.open_writer(batch().schema) as writer:
        writer.write_batch(batch(), expected_position=0, key="first")
    manifest_path = tmp_path / "manifests/00000000000000000000/manifest.json"
    manifest = json.loads(manifest_path.read_text())
    part = manifest["parts"][0]
    path = tmp_path / part["key"]
    pq.write_table(pa.table({"different": range(5)}), path)
    encoded = path.read_bytes()
    part.update(size=len(encoded), sha256=hashlib.sha256(encoded).hexdigest())
    manifest_path.write_text(json.dumps(manifest))
    with pytest.raises(OSError) as error:
        values(table.snapshot())
    assert getattr(error.value, "kind", None) == "corrupt"


def test_recovery_and_empty_table(tmp_path):
    table = ManifestTable(tmp_path)
    writer = table.open_writer(batch().schema)
    token = RecoveryToken.from_json(writer.recovery_token.to_json())
    receipt = writer.finish()
    assert table.recover(token) == table.recover(token, retry=True) == receipt
    assert ManifestTable(tmp_path).recover(token) == receipt
    assert values(table.snapshot()) == {"a": [], "b": []}
    assert table.snapshot().schema == batch().schema
    with pytest.raises(ValueError):
        ManifestTable(tmp_path / "other").recover(token)
    with pytest.raises(FileNotFoundError):
        table.snapshot(100)
    with pytest.raises((OverflowError, ValueError)):
        table.snapshot(-1)


@pytest.mark.parametrize("options", [SinkOptions(target_part_bytes=0), SinkOptions(memory_limit=1), SinkOptions(max_history_reads=0)])
def test_invalid_writer_limits(tmp_path, options):
    with pytest.raises(ValueError):
        ManifestTable(tmp_path).open_writer(batch().schema, options=options)
    assert not list(tmp_path.iterdir())


@pytest.fixture
def s3_store():
    objects = {}
    state = {"hide_manifest": False, "lose_ack": False, "deny_marker": False, "defer_manifest": False}
    lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format, *args):
            pass

        def do_PUT(self):
            key = urlparse(self.path).path
            body = self.rfile.read(int(self.headers["Content-Length"]))
            assert self.headers.get("If-None-Match") == "*"
            with lock:
                if key.endswith("/manifest.json") and state["defer_manifest"]:
                    status = 503
                elif key.endswith("/_SUCCESS") and state["deny_marker"]:
                    status = 403
                elif key in objects:
                    status = 412
                else:
                    objects[key] = body
                    status = 200
                if key.endswith("/manifest.json") and state["lose_ack"]:
                    state["hide_manifest"] = True
                    status = 503
            self.send_response(status)
            self.send_header("Content-Length", "0")
            self.end_headers()

        def do_GET(self):
            key = urlparse(self.path).path
            assert not urlparse(self.path).query, "table access must not list directories"
            with lock:
                body = objects.get(key)
                hidden = key.endswith("/manifest.json") and state["hide_manifest"]
            status = 403 if hidden else 404 if body is None else 200
            start, end = 0, len(body or b"")
            bounds = self.headers.get("Range")
            if status == 200 and bounds:
                a, b = bounds.removeprefix("bytes=").split("-")
                start, end = int(a), min(int(b) + 1, end) if b else end
                status = 206
            self.send_response(status)
            self.send_header("Content-Length", str(end - start if status in (200, 206) else 0))
            self.send_header("ETag", '"' + hashlib.sha256(body or b"").hexdigest() + '"')
            self.send_header("Last-Modified", "Sun, 22 Nov 2026 12:00:00 GMT")
            if status == 206:
                assert body is not None
                self.send_header("Content-Range", f"bytes {start}-{end - 1}/{len(body)}")
            self.end_headers()
            if self.command == "GET" and status in (200, 206):
                assert body is not None
                self.wfile.write(body[start:end])

        do_HEAD = do_GET

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    options = {"anon": True, "endpoint_url": f"http://127.0.0.1:{server.server_port}", "region": "us-east-1"}
    try:
        yield ManifestTable("s3://test/table", storage_options=options), state, options
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


def test_uncertain_publication_can_be_resolved_without_duplicate_rows(s3_store):
    table, state, options = s3_store
    writer = table.open_writer(batch().schema)
    writer.write_batch(batch(), expected_position=0, key="first")
    token = writer.recovery_token
    state["lose_ack"] = True
    with pytest.raises(OSError) as error:
        writer.commit()
    assert getattr(error.value, "kind", None) == "unknown_outcome" and not getattr(error.value, "retry_safe", None)
    assert getattr(error.value, "recovery_token", None) == token
    with pytest.raises(OSError) as error:
        writer.abort()
    assert getattr(error.value, "kind", None) == "unknown_outcome"
    state.update(lose_ack=False, hide_manifest=False)
    receipt = writer.resolve()
    assert receipt.progress == 5
    assert ManifestTable("s3://test/table", storage_options=options).recover(token) == receipt
    writer.close()
    assert values(table.snapshot())["a"] == list(range(5))


def test_marker_failure_does_not_undo_complete_publication(s3_store):
    table, state, _ = s3_store
    writer = table.open_writer(batch().schema)
    writer.write_batch(batch(), expected_position=0, key="first")
    token = writer.recovery_token
    state["deny_marker"] = True
    receipt = writer.finish()
    assert receipt.complete and receipt.marker_error["kind"] == "permission_denied"
    assert table.snapshot().complete
    state["deny_marker"] = False
    assert table.recover(token).marker_error is None


def test_recovery_after_restart_can_resubmit_exact_candidate(s3_store):
    table, state, options = s3_store
    writer = table.open_writer(batch().schema)
    writer.write_batch(batch(), expected_position=0, key="first")
    token = writer.recovery_token
    state["defer_manifest"] = True
    with pytest.raises(OSError):
        writer.commit()
    del writer
    gc.collect()
    with pytest.raises(OSError) as error:
        table.recover(token)
    assert getattr(error.value, "kind", None) == "unknown_outcome"
    state["defer_manifest"] = False
    restarted = ManifestTable("s3://test/table", storage_options=options)
    receipt = restarted.recover(token, retry=True)
    assert receipt.progress == 5
    assert restarted.recover(token, retry=True) == receipt
    assert values(restarted.snapshot())["a"] == list(range(5))


def test_relative_local_path_remains_bound_after_chdir(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    table = ManifestTable("table")
    with table.open_writer(batch().schema) as writer:
        writer.write_batch(batch(), expected_position=0, key="first")
    other = tmp_path / "other"
    other.mkdir()
    monkeypatch.chdir(other)
    assert values(table.snapshot())["a"] == list(range(5))


def test_writer_schema_and_memory_validation_leaves_no_publication(tmp_path):
    table = ManifestTable(tmp_path)
    writer = table.open_writer(batch().schema, options=SinkOptions(target_part_bytes=128, memory_limit=1024))
    with pytest.raises(ValueError):
        writer.write_batch(pa.record_batch({"wrong": [1]}), expected_position=0, key="wrong")
    with pytest.raises(ValueError):
        writer.write_batch(batch(0, 10000), expected_position=0, key="large")
    writer.close()
    with pytest.raises(FileNotFoundError):
        table.snapshot()


def test_context_exit_preserves_exception_if_publication_cannot_be_aborted(s3_store):
    table, state, _ = s3_store
    state["defer_manifest"] = True
    with pytest.raises(OSError) as error, table.open_writer(batch().schema) as writer:
        writer.write_batch(batch(), expected_position=0, key="first")
        writer.commit()
    assert getattr(error.value, "kind", None) == "unknown_outcome"
    assert any("Writer abort failed" in note for note in error.value.__notes__)
    state["defer_manifest"] = False
    assert writer.resolve(retry=True).progress == 5
    writer.close()


def test_schema_metadata_round_trip(tmp_path):
    records = batch().replace_schema_metadata({b"source": b"example"})
    table = ManifestTable(tmp_path)
    with table.open_writer(records.schema) as writer:
        writer.write_batch(records, expected_position=0, key="first")
    snapshot = table.snapshot()
    assert snapshot.schema.equals(records.schema, check_metadata=True)
    decoded = next(snapshot.scan())
    assert decoded.equals(records, check_metadata=True)


@pytest.mark.parametrize("changes", [{"encoding": "arrow"}, {"options": {"unsupported": True}}])
def test_scan_rejects_unsupported_encoding_options(tmp_path, changes):
    table = ManifestTable(tmp_path)
    with table.open_writer(batch().schema) as writer:
        writer.write_batch(batch(), expected_position=0, key="first")
    path = tmp_path / "manifests/00000000000000000000/manifest.json"
    manifest = json.loads(path.read_text())
    manifest["parts"][0].update(changes)
    path.write_text(json.dumps(manifest))
    with pytest.raises(NotImplementedError):
        values(table.snapshot())


def test_follow_positions_include_s3_bucket_endpoint_and_exclude_credentials(s3_store):
    table, _, options = s3_store
    with table.open_writer(batch().schema) as writer:
        writer.write_batch(batch(), expected_position=0, key="first")
    saved = next(table.follow()).position
    for root, changed_options in [
        ("s3://different/table", options),
        ("s3://test/other", options),
        ("s3://test/table", {**options, "endpoint_url": "http://127.0.0.1:1"}),
    ]:
        with pytest.raises(ValueError, match="different source"):
            ManifestTable(root, storage_options=changed_options).follow(start=saved)
    credentialed = {**options, "anon": False, "key": "test-access-key", "secret": "test-secret-key"}
    resumed = ManifestTable("s3://test/table", storage_options=credentialed).follow(start=saved, idle_timeout=0)
    assert resumed.position == saved
    assert "test-access-key" not in saved.to_json() and "test-secret-key" not in saved.to_json()
    assert list(resumed) == []
