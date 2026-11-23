"""Local manifest publication and saved-generation scan benchmarks."""

import pyarrow as pa
import pytest

from fsspec_data import ManifestTable


@pytest.fixture(params=[1_000, 10_000], ids=["1k-rows", "10k-rows"])
def records(request):
    return pa.record_batch({"value": range(request.param)})


@pytest.mark.benchmark(group="manifest-publication")
def test_publish_table(benchmark, tmp_path_factory, records):
    def setup():
        return (ManifestTable(tmp_path_factory.mktemp("publication")),), {}

    def publish(table):
        writer = table.open_writer(records.schema)
        try:
            writer.write_batch(records, expected_position=0, key="batch")
            return writer.commit()
        finally:
            writer.close()

    receipt = benchmark.pedantic(publish, setup=setup, rounds=5, iterations=1)
    assert receipt.num_rows == receipt.progress == records.num_rows
    benchmark.extra_info.update(rows=records.num_rows)


@pytest.mark.benchmark(group="manifest-scan")
@pytest.mark.parametrize("generation", [0, None], ids=["pinned", "latest"])
def test_scan_table(benchmark, tmp_path, records, generation):
    table = ManifestTable(tmp_path)
    with table.open_writer(records.schema) as writer:
        writer.write_batch(records, expected_position=0, key="first")
        writer.commit()
        writer.write_batch(records, expected_position=records.num_rows, key="second")
    expected_rows = records.num_rows * (1 if generation == 0 else 2)

    def scan():
        stream = table.snapshot(generation).scan(batch_size=1024)
        return sum(batch.num_rows for batch in stream)

    assert benchmark(scan) == expected_rows
    benchmark.extra_info.update(rows=expected_rows)
