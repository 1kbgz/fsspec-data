import io
import json

import fsspec
import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from fsspec_data import DEFAULT_REGISTRY, Codec, DataViewFileSystem, DecodedBatchStream

SCHEMA = pa.schema([pa.field("id", pa.int64(), nullable=False), pa.field("name", pa.string())])
SCHEMA_OPTIONS = {
    "fields": [
        {"name": "id", "type": "int64", "nullable": False},
        {"name": "name", "type": "string", "nullable": True},
    ]
}
ROWS = [{"id": 1, "name": "ada"}, {"id": 2, "name": "grace"}]


@pytest.fixture
def memory_fs():
    fs = fsspec.filesystem("memory")
    fs.store.clear()
    fs.pipe("orders.csv", b"id,name\n1,ada\n2,grace\n")
    return fs


def test_a_single_file_converts_with_a_provided_schema(memory_fs):
    fs = DataViewFileSystem("orders.csv", fs=memory_fs, provided_schema=SCHEMA_OPTIONS)

    assert fs.ls("", detail=False) == ["orders.csv", "orders.metadata.json", "orders.parquet", "orders.schema.json"]
    with fs.open("orders.parquet", "rb") as file:
        assert file.read(4) == b"PAR1"
        file.seek(0)
        table = pq.read_table(file)
    assert table.schema.equals(SCHEMA, check_metadata=False)
    assert table.to_pylist() == ROWS


def test_a_single_file_converts_to_avro(memory_fs):
    fs = DataViewFileSystem("orders.csv", fs=memory_fs, mapping={"table": ["avro"]}, provided_schema=SCHEMA_OPTIONS)

    data = fs.cat_file("orders.avro")
    assert data.startswith(b"Obj\x01")
    assert pa.Table.from_batches(DEFAULT_REGISTRY.get("avro").decode_batches(data).batches).to_pylist() == ROWS


def test_conversion_streams_from_the_source_and_honors_limits(memory_fs, monkeypatch):
    encoded = b"id,name\n" + b"".join(f"{index},name-{index}\n".encode() for index in range(50_000))
    bytes_read = 0

    class CountingReader(io.BytesIO):
        def read(self, size=-1):
            nonlocal bytes_read
            data = super().read(size)
            bytes_read += len(data)
            return data

    monkeypatch.setattr(memory_fs, "open", lambda *args, **kwargs: CountingReader(encoded))
    monkeypatch.setattr(memory_fs, "cat_file", lambda *args, **kwargs: pytest.fail("cat_file should not be called"))
    fs = DataViewFileSystem("orders.csv", fs=memory_fs, provided_schema=SCHEMA_OPTIONS, batch_size=1, row_limit=1)

    assert pq.read_table(fs.open("orders.parquet")).to_pylist() == [{"id": 0, "name": "name-0"}]
    assert bytes_read < len(encoded)


def test_conversion_streams_batches_into_spooled_output(memory_fs, monkeypatch):
    monkeypatch.setattr(DecodedBatchStream, "collect", lambda self: pytest.fail("decoded batches should not be collected"))
    monkeypatch.setattr(Codec, "encode_batches", lambda self, *args, **kwargs: pytest.fail("encoded bytes should not be collected"))
    fs = DataViewFileSystem("orders.csv", fs=memory_fs, provided_schema=SCHEMA_OPTIONS)

    assert pq.read_table(fs.open("orders.parquet")).to_pylist() == ROWS


def test_fsspec_urls_name_the_rendered_file_and_the_source(memory_fs):
    fs, path = fsspec.core.url_to_fs("dataview://orders.parquet::memory://orders.csv", provided_schema=SCHEMA_OPTIONS)

    assert isinstance(fs, DataViewFileSystem)
    assert path == "orders.parquet"
    assert pq.read_table(fs.open(path)).to_pylist() == ROWS
    assert pq.read_table(DataViewFileSystem("memory://orders.csv").open("orders.parquet")).num_rows == 2


def test_requested_schemas_project_embedded_schemas(memory_fs):
    parquet = pa.BufferOutputStream()
    pq.write_table(pa.table({"id": [1, 2], "name": ["ada", "grace"]}), parquet)
    memory_fs.pipe("orders.parquet", parquet.getvalue().to_pybytes())
    fs = DataViewFileSystem(
        "orders.parquet",
        fs=memory_fs,
        mapping={"table": ["arrow"], "schema": ["json"]},
        requested_schema={"fields": [{"name": "name", "type": "string"}]},
        schema_policy="projection",
    )

    with fs.open("orders.arrow") as file:
        assert pa.ipc.open_stream(file).read_all().to_pylist() == [{"name": "ada"}, {"name": "grace"}]
    assert [field["name"] for field in json.loads(fs.cat_file("orders.schema.json"))["fields"]] == ["name"]


def test_conversions_replace_the_native_file_of_the_same_format(memory_fs):
    fs = DataViewFileSystem(
        "orders.csv",
        fs=memory_fs,
        mapping={"table": ["raw", "csv"]},
        requested_schema={"fields": [{"name": "name", "type": "string"}]},
        schema_policy="projection",
    )

    assert fs.ls("", detail=False) == ["orders.csv"]
    assert fs.cat_file("orders.csv") == b"name\nada\ngrace\n"


def test_provided_schemas_must_match_embedded_schemas(memory_fs):
    parquet = pa.BufferOutputStream()
    pq.write_table(pa.table({"id": [1]}), parquet)
    memory_fs.pipe("orders.parquet", parquet.getvalue().to_pybytes())
    fs = DataViewFileSystem("orders.parquet", fs=memory_fs, mapping={"table": ["arrow"]}, provided_schema=pa.schema([("id", pa.int32())]))

    with pytest.raises(ValueError, match="provided schema does not match"):
        fs.open("orders.arrow")


def test_converted_output_rolls_over_at_spool_limit(memory_fs):
    fs = DataViewFileSystem("orders.csv", fs=memory_fs, provided_schema=SCHEMA, spool_max_size=1)

    with fs.open("orders.parquet") as file:
        assert file._rolled
        assert file.read(4) == b"PAR1"


def test_unmapped_names_are_not_found(memory_fs):
    fs = DataViewFileSystem("orders.csv", fs=memory_fs, provided_schema=SCHEMA)

    with pytest.raises(FileNotFoundError):
        fs.open("orders.bin")


def test_schema_options_accept_a_field_sequence(memory_fs):
    fs = DataViewFileSystem(
        "orders.csv",
        fs=memory_fs,
        provided_schema=[{"name": "id", "type": pa.int64(), "nullable": False}, {"name": "name", "type": "string"}],
    )

    assert pq.read_table(fs.open("orders.parquet")).to_pylist() == ROWS


@pytest.mark.parametrize(
    ("schema", "error", "message"),
    [
        ({"fields": "invalid"}, TypeError, "sequence of fields"),
        ({"fields": ["invalid"]}, TypeError, "fields must be mappings"),
        ({"fields": [{}]}, ValueError, "missing 'name'"),
        ({"fields": [{"name": "id"}]}, ValueError, "missing 'type'"),
        ({"fields": [{"name": 1, "type": "int64"}]}, TypeError, "name and type"),
        ({"fields": [{"name": "id", "type": object()}]}, TypeError, "name and type"),
        ({"fields": [{"name": "id", "type": "int64", "nullable": "yes"}]}, TypeError, "nullable must be a boolean"),
    ],
)
def test_invalid_schema_options_are_rejected(memory_fs, schema, error, message):
    with pytest.raises(error, match=message):
        DataViewFileSystem("orders.csv", fs=memory_fs, provided_schema=schema)
