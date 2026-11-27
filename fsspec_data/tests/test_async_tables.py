import asyncio
import gc

import pyarrow as pa
import pytest

from fsspec_data import ManifestTable, WatchEnd
from fsspec_data.tests.test_tables import batch, s3_store  # noqa: F401


def publish(table, *, complete=False):
    writer = table.open_writer(batch().schema)
    writer.write_batch(batch(), expected_position=0, key="first")
    (writer.finish if complete else writer.commit)()
    return writer


def test_async_snapshots_selection_limits_and_reuse(tmp_path):
    table = ManifestTable(tmp_path)
    writer = publish(table)

    async def run():
        snapshot = await table.snapshot_async()
        writer.write_batch(batch(5), expected_position=5, key="second")
        writer.finish()
        assert snapshot.generation == 0
        assert (await table.snapshot_async()).num_rows == 10
        async with await snapshot.scan_async(columns=["b", "a"], row_offset=1, row_limit=3, batch_size=2, read_chunk_size=7) as scan:
            result = pa.Table.from_batches([item async for item in scan])
            assert result.to_pydict() == {"b": ["1", "2", "3"], "a": [1, 2, 3]}
        async with await snapshot.scan_async(max_batch_bytes=1) as scan:
            with pytest.raises(ValueError, match="byte"):
                await anext(scan)
        async with await snapshot.scan_async(row_limit=0) as scan:
            assert [item async for item in scan] == []
        # Closing a scan does not close its reusable snapshot.
        async with await snapshot.scan_async() as scan:
            assert (await anext(scan)).num_rows == 5
        await scan.aclose()

    asyncio.run(run())


def test_async_watch_resumes_appends_finishes_and_replaces(tmp_path):
    table = ManifestTable(tmp_path)
    writer = publish(table)

    async def run():
        async with await table.watch_async(batch_size=2) as watch:
            first = await anext(watch)
            assert watch.position == first.position and first.position.progress == 2
        assert watch.end_reason is WatchEnd.CANCELLED
        writer.write_batch(batch(5), expected_position=5, key="second")
        writer.commit()
        async with await table.watch_async(start=first.position, idle_timeout=0) as watch:
            items = [item async for item in watch]
            assert pa.Table.from_batches([item.batch for item in items]).column("a").to_pylist() == list(range(2, 10))
            assert watch.end_reason is WatchEnd.IDLE_TIMEOUT
            checkpoint = watch.position
        async with await table.watch_async(start=checkpoint, poll_interval=0.01) as watch:
            pending = asyncio.create_task(anext(watch))
            await asyncio.sleep(0.02)
            with table.open_writer(batch().schema, mode="replace") as replacement:
                replacement.write_batch(batch(100), expected_position=0, key="replacement")
            with pytest.raises(StopAsyncIteration):
                await asyncio.wait_for(pending, 1)
            assert watch.end_reason is WatchEnd.REPLACED
            assert watch.position == checkpoint
        replacement = table.open_writer(batch().schema, mode="append")
        replacement.finish()
        async with await table.watch_async() as watch:
            assert sum(item.batch.num_rows for item in [item async for item in watch]) == 5
            assert watch.end_reason is WatchEnd.COMPLETE

    asyncio.run(run())


def test_cancellation_concurrent_close_and_deadline_preserve_checkpoint(tmp_path):
    table = ManifestTable(tmp_path)
    publish(table)

    async def run():
        for cancel_task in (False, True):
            watch = await table.watch_async(poll_interval=60)
            await anext(watch)
            checkpoint = watch.position
            pending = asyncio.create_task(anext(watch))
            await asyncio.sleep(0.02)
            with pytest.raises(RuntimeError, match="already pending"):
                await anext(watch)
            if cancel_task:
                pending.cancel()
                with pytest.raises(asyncio.CancelledError):
                    await asyncio.wait_for(pending, 1)
            else:
                await asyncio.wait_for(watch.aclose(), 1)
                with pytest.raises(StopAsyncIteration):
                    await pending
            assert watch.position == checkpoint
            assert watch.end_reason is WatchEnd.CANCELLED
            await watch.aclose()
        watch = await table.watch_async(poll_interval=60, deadline=asyncio.get_running_loop().time() + 0.2)
        await anext(watch)
        checkpoint = watch.position
        with pytest.raises(TimeoutError):
            await asyncio.wait_for(anext(watch), 1)
        assert watch.position == checkpoint
        assert watch.end_reason is None
        await watch.aclose()
        with pytest.raises(TimeoutError):
            await table.snapshot_async(deadline=asyncio.get_running_loop().time() - 1)

    asyncio.run(run())


def test_corruption_and_buffer_errors_do_not_advance_watch(tmp_path):
    table = ManifestTable(tmp_path)
    publish(table)

    async def run():
        watch = await table.watch_async(max_batch_bytes=1)
        checkpoint = watch.position
        with pytest.raises(ValueError):
            await anext(watch)
        assert watch.position == checkpoint and watch.end_reason is None
        snapshot = await table.snapshot_async()
        part = tmp_path / snapshot.metadata["parts"][0]["key"]
        data = bytearray(part.read_bytes())
        data[len(data) // 2] ^= 1
        part.write_bytes(data)
        async with await snapshot.scan_async() as scan:
            with pytest.raises(OSError) as error:
                await anext(scan)
            assert getattr(error.value, "kind", None) == "corrupt"
        watch = await table.watch_async()
        checkpoint = watch.position
        with pytest.raises(OSError):
            await anext(watch)
        assert watch.position == checkpoint and watch.end_reason is None

    asyncio.run(run())


@pytest.mark.parametrize("argument,value", [("batch_size", 0), ("read_chunk_size", 0), ("read_chunk_size", 9 * 1024 * 1024), ("max_batch_bytes", 0)])
def test_invalid_async_limits(tmp_path, argument, value):
    table = ManifestTable(tmp_path)
    publish(table)

    async def run():
        snapshot = await table.snapshot_async()
        with pytest.raises(ValueError):
            await snapshot.scan_async(**{argument: value})
        with pytest.raises(ValueError):
            await table.watch_async(**{argument: value})
        with pytest.raises(ValueError):
            await table.watch_async(deadline=float("nan"))

    asyncio.run(run())


def test_native_s3_async_snapshot_and_watch(s3_store):  # noqa: F811
    table, _, _ = s3_store
    writer = publish(table)

    async def run():
        snapshot = await table.snapshot_async()
        scan1 = await snapshot.scan_async(read_chunk_size=19, batch_size=2)
        scan2 = await snapshot.scan_async(row_offset=3)
        async with scan1, scan2:
            assert (await anext(scan1)).num_rows == 2
            assert (await anext(scan2)).column("a").to_pylist() == [3, 4]
            await scan1.aclose()
            with pytest.raises(StopAsyncIteration):
                await anext(scan2)
        writer.finish()
        async with await table.watch_async() as watch:
            items = [item async for item in watch]
            assert sum(item.batch.num_rows for item in items) == 5
            assert watch.end_reason is WatchEnd.COMPLETE
        # A scan retains its async context even after its Python snapshot is dropped.
        sync_snapshot = await table.snapshot_async()
        sync_scan = sync_snapshot.scan()
        del sync_snapshot
        gc.collect()
        assert next(sync_scan).num_rows == 5
        del sync_scan
        scan = await snapshot.scan_async()
        del snapshot
        async with scan:
            assert (await anext(scan)).num_rows == 5

    asyncio.run(run())
