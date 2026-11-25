"""Cleanup keeps saved snapshots readable and fences out failed publishers."""

import json
import os
import time
import uuid

import pyarrow as pa
import pytest

from fsspec_data import ManifestTable, RecoveryToken


@pytest.fixture(params=["local", "deployment"])
def table(request, tmp_path):
    if request.param == "local":
        return ManifestTable(tmp_path)
    root = os.environ.get("FSSPEC_TABLE_TEST_URL")
    if not root:
        pytest.skip("FSSPEC_TABLE_TEST_URL not set; deployment publication checks not run")
    return ManifestTable(
        root.rstrip("/") + "/cleanup-" + uuid.uuid4().hex,
        storage_options=json.loads(os.environ.get("FSSPEC_TABLE_TEST_OPTIONS", "{}")),
    )


def stage(table, value, mode="create"):
    data = pa.record_batch({"value": [value]})
    writer = table.open_writer(data.schema, mode=mode)
    writer.write_batch(data, expected_position=writer.position, key=str(value))
    writer.flush()
    return writer


def test_collect_preserves_old_snapshots_and_resolves_losing_writer(table):
    winner, loser = stage(table, 1), stage(table, 2)
    receipt = winner.commit()
    token = RecoveryToken.from_json(loser.recovery_token.to_json())
    with pytest.raises(OSError) as error:
        loser.commit()
    assert getattr(error.value, "kind", None) == "conflict"
    saved = table.snapshot(0)
    stage(table, 3, mode="replace").finish()
    collector = table.orphan_collector(retention_seconds=0.01)
    first = collector.collect(dry_run=False)
    assert first.generation == 1 and first.pending == 2
    assert first.eligible == first.deleted == []
    time.sleep(0.02)
    dry = collector.collect()
    assert len(dry.eligible) == 2 and not dry.deleted
    assert collector.collect(dry_run=False).deleted == dry.eligible
    for snapshot in (saved, table.snapshot(0)):
        assert [b.to_pydict() for b in snapshot.scan()] == [{"value": [1]}]
    assert [b.to_pydict() for b in table.snapshot().scan()] == [{"value": [3]}]
    assert table.recover(receipt.recovery) == receipt
    with pytest.raises(OSError) as error:
        table.recover(token, retry=True)
    assert getattr(error.value, "kind", None) == "conflict"
    assert collector.collect(dry_run=False).deleted == []


def test_local_unchanged_files_future_attempts_and_symlinks(tmp_path):
    table = ManifestTable(tmp_path / "table")
    stage(table, 1).commit()
    future = stage(table, 2, mode="append")
    root = tmp_path / "table" / "parts" / "00000000000000000000"
    outside = tmp_path / "outside"
    outside.mkdir()
    (outside / "keep").write_bytes(b"external")
    try:
        (root / "linked").symlink_to(outside, target_is_directory=True)
    except OSError:
        pass  # Windows can prohibit symlink creation for this user.
    (root / "unexpected-file").write_bytes(b"keep")
    (root / "unknown.attempt").mkdir()
    (root / "unknown.attempt" / "keep").write_bytes(b"keep")
    collector = table.orphan_collector(retention_seconds=0.001)
    collector.collect()
    time.sleep(0.01)
    assert collector.collect(dry_run=False).deleted == []
    assert (outside / "keep").read_bytes() == b"external"
    assert (root / "unexpected-file").exists()
    assert (root / "unknown.attempt" / "keep").exists()
    future.commit()
    assert table.snapshot().num_rows == 2


@pytest.mark.parametrize("retention", [0, -1, float("inf"), float("nan")])
def test_invalid_retention(tmp_path, retention):
    with pytest.raises(ValueError):
        ManifestTable(tmp_path).orphan_collector(retention_seconds=retention)


def test_new_collector_resets_retention_and_empty_namespace_is_untouched(tmp_path):
    table = ManifestTable(tmp_path)
    abandoned = stage(table, 1)
    collector = table.orphan_collector(retention_seconds=0.001)
    assert collector.collect(dry_run=False).generation is None
    stage(table, 2).commit()
    assert collector.collect().pending == 1
    time.sleep(0.01)
    restarted = table.orphan_collector(retention_seconds=0.001)
    assert restarted.collect(dry_run=False).pending == 1
    assert collector.collect(dry_run=False).deleted
    with pytest.raises(OSError) as error:
        abandoned.commit()
    assert getattr(error.value, "kind", None) == "conflict"
