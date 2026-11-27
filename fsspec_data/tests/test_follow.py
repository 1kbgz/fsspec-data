"""Follow checkpoints refer to immutable generations, including after restart."""

import threading
from dataclasses import replace

import pyarrow as pa
import pytest

from fsspec_data import FollowEnd, FollowPosition, ManifestTable, SinkOptions


def publish(table, values, *, mode="create", complete=False):
    batch = pa.record_batch({"value": values})
    writer = table.open_writer(batch.schema, mode=mode)
    writer.write_batch(batch, expected_position=writer.position, key=str(values))
    (writer.finish if complete else writer.commit)()
    return writer


def test_state_is_pinned_exact_and_separate_from_raw_metadata(tmp_path):
    table = ManifestTable(tmp_path)
    batch = pa.record_batch({"value": [1, 2]})
    writer = table.open_writer(batch.schema, options=SinkOptions(partition_values={"date": "2026-11-27"}))
    writer.write_batch(batch, expected_position=0, key="first")
    writer.commit()
    saved = table.snapshot()
    assert saved.state == {
        "generation": 0,
        "num_rows": 2,
        "num_rows_exact": True,
        "complete": False,
        "progress": 2,
        "progress_unit": "rows",
        "distinct.date": ["2026-11-27"],
    }
    modified = saved.state
    modified["num_rows"] = 100
    writer.finish()
    assert saved.state["num_rows"] == 2 and not saved.state["complete"]
    assert table.snapshot().state["generation"] == 1 and table.snapshot().state["complete"]
    assert "modified" not in saved.state and "parts" not in saved.state


def test_resume_new_source_instance_through_appends_and_finish(tmp_path):
    table = ManifestTable(tmp_path)
    publish(table, [0, 1, 2])
    first = table.follow(batch_size=2)
    item = next(first)
    assert item.batch.to_pydict() == {"value": [0, 1]}
    checkpoint = FollowPosition.from_json(item.position.to_json())
    first.close()
    assert list(first) == [] and first.end_reason == FollowEnd.CANCELLED
    publish(table, [3, 4], mode="append")
    publish(table, [5, 6], mode="append", complete=True)
    follow = ManifestTable(tmp_path).follow(start=checkpoint, batch_size=2)
    assert follow.schema == item.batch.schema
    items = list(follow)
    assert [v for i in items for v in i.batch.column(0).to_pylist()] == [2, 3, 4, 5, 6]
    assert [i.position.generation for i in items] == [0, 1, 2]
    assert follow.position == items[-1].position
    assert follow.end_reason == FollowEnd.COMPLETE
    assert list(follow) == []
    resumed = table.follow(start=follow.position)
    assert list(resumed) == [] and resumed.end_reason == FollowEnd.COMPLETE


def test_replace_stops_original_follow_and_preserves_checkpoint(tmp_path):
    table = ManifestTable(tmp_path)
    publish(table, [1])
    follow = table.follow()
    saved = next(follow).position
    publish(table, [100], mode="replace")
    publish(table, [101], mode="append", complete=True)
    assert list(follow) == []
    assert follow.end_reason == FollowEnd.REPLACED and follow.position == saved
    resumed = table.follow(start=saved)
    assert list(resumed) == [] and resumed.end_reason == FollowEnd.REPLACED
    assert [i.batch.column(0).to_pylist() for i in table.follow()] == [[100], [101]]


def test_follow_waits_for_commit_and_cancels_without_holding_gil(tmp_path):
    table = ManifestTable(tmp_path)
    writer = publish(table, [1])
    follow = table.follow(poll_interval=0.001, idle_timeout=5)
    next(follow)
    result = []
    thread = threading.Thread(target=lambda: result.append(next(follow)))
    thread.start()
    writer.write_batch(pa.record_batch({"value": [2]}), expected_position=1, key="second")
    writer.commit()
    thread.join(timeout=10)
    assert not thread.is_alive() and result[0].batch.column(0).to_pylist() == [2]
    # A long interval must still be interruptible from another Python thread.
    waiting = table.follow(start=result[0].position, poll_interval=60)
    thread = threading.Thread(target=lambda: result.append(list(waiting)))
    thread.start()
    waiting.cancel()
    thread.join(timeout=2)
    assert not thread.is_alive() and result[-1] == []
    assert waiting.end_reason == FollowEnd.CANCELLED


def test_idle_timeout_and_context_exit(tmp_path):
    table = ManifestTable(tmp_path)
    publish(table, [1, 2])
    follow = table.follow(columns=[], idle_timeout=0)
    assert next(follow).batch.num_rows == 2
    assert list(follow) == [] and follow.end_reason == FollowEnd.IDLE_TIMEOUT
    with table.follow() as cancelled:
        assert cancelled.end_reason is None
    assert list(cancelled) == [] and cancelled.end_reason == FollowEnd.CANCELLED


@pytest.mark.parametrize(
    "options",
    [
        {"batch_size": 0},
        {"poll_interval": 0},
        {"poll_interval": -1},
        {"poll_interval": float("nan")},
        {"idle_timeout": -1},
        {"idle_timeout": float("inf")},
        {"columns": ["missing"]},
    ],
)
def test_invalid_follow_options(tmp_path, options):
    table = ManifestTable(tmp_path)
    publish(table, [1])
    with pytest.raises(ValueError):
        table.follow(**options)


def test_positions_reject_wrong_path_namespace_generation_and_progress(tmp_path):
    table = ManifestTable(tmp_path / "one")
    publish(table, [1, 2])
    saved = table.follow().position
    other = ManifestTable(tmp_path / "two")
    publish(other, [1, 2])
    with pytest.raises(ValueError, match="different source"):
        other.follow(start=saved)
    for field, value in [
        ("namespace_id", "other"),
        ("table_id", "other"),
        ("progress", 3),
        ("progress_unit", "events"),
        ("manifest_sha256", "0" * 64),
    ]:
        with pytest.raises(ValueError):
            table.follow(start=replace(saved, **{field: value}))
    with pytest.raises(FileNotFoundError):
        table.follow(start=replace(saved, generation=99))
    with pytest.raises(ValueError):
        table.follow(start=replace(saved, progress=True))
    with pytest.raises(FileNotFoundError):
        ManifestTable(tmp_path / "unpublished").follow()


def test_failed_part_read_does_not_advance_checkpoint_and_ends_iteration(tmp_path):
    table = ManifestTable(tmp_path)
    publish(table, [1, 2])
    follow = table.follow()
    saved = follow.position
    part = table.snapshot().metadata["parts"][0]["key"]
    (tmp_path / part).write_bytes(b"corrupt")
    with pytest.raises(OSError) as error:
        next(follow)
    assert getattr(error.value, "kind", None) == "corrupt"
    assert follow.position == saved and follow.end_reason is None and list(follow) == []


def test_resume_skips_already_consumed_parts(tmp_path):
    table = ManifestTable(tmp_path)
    publish(table, [1, 2])
    saved = next(table.follow()).position
    publish(table, [3, 4], mode="append", complete=True)
    # Simulate a provider refusing old-part reads: resume has no need to fetch them.
    (tmp_path / table.snapshot(0).metadata["parts"][0]["key"]).unlink()
    resumed = table.follow(start=saved)
    assert [i.batch.column(0).to_pylist() for i in resumed] == [[3, 4]]
    assert resumed.end_reason == FollowEnd.COMPLETE
