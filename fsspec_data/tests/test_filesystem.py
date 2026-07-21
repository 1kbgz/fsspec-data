import io

import fsspec
import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from fsspec_data import Codec, DataFileSystem, DecodedBatchStream

SCHEMA = pa.schema([pa.field("id", pa.int64(), nullable=False), pa.field("name", pa.string())])
SCHEMA_OPTIONS = {
    "fields": [
        {"name": "id", "type": "int64", "nullable": False},
        {"name": "name", "type": "string", "nullable": True},
    ]
}


@pytest.fixture
def memory_fs():
    fs = fsspec.filesystem("memory")
    fs.store.clear()
    fs.pipe("orders.csv", b"id,name\n1,ada\n2,grace\n")
    return fs


def test_chained_filesystem_converts_csv_to_seekable_parquet(memory_fs):
    fs = DataFileSystem(fo="orders.csv", fs=memory_fs, provided_schema=SCHEMA_OPTIONS)

    with fs.open("orders.parquet", "rb") as file:
        assert file.read(4) == b"PAR1"
        file.seek(0)
        table = pq.read_table(file)

    assert table.schema.equals(SCHEMA, check_metadata=False)
    assert table.to_pylist() == [{"id": 1, "name": "ada"}, {"id": 2, "name": "grace"}]


def test_chained_filesystem_reads_source_without_cat_file(memory_fs, monkeypatch):
    rows = [f"{index},name-{index}\n".encode() for index in range(50_000)]
    encoded = b"id,name\n" + b"".join(rows)
    bytes_read = 0

    class CountingReader(io.BytesIO):
        def read(self, size=-1):
            nonlocal bytes_read
            data = super().read(size)
            bytes_read += len(data)
            return data

    monkeypatch.setattr(memory_fs, "open", lambda *args, **kwargs: CountingReader(encoded))
    monkeypatch.setattr(memory_fs, "cat_file", lambda *args, **kwargs: pytest.fail("cat_file should not be called"))
    fs = DataFileSystem(
        fo="orders.csv",
        fs=memory_fs,
        provided_schema=SCHEMA_OPTIONS,
        batch_size=1,
        row_limit=1,
    )

    assert pq.read_table(fs.open("orders.parquet")).to_pylist() == [{"id": 0, "name": "name-0"}]
    assert bytes_read < len(encoded)


def test_chained_filesystem_streams_batches_into_spooled_output(memory_fs, monkeypatch):
    monkeypatch.setattr(DecodedBatchStream, "collect", lambda self: pytest.fail("decoded batches should not be collected"))
    monkeypatch.setattr(Codec, "encode_batches", lambda self, *args, **kwargs: pytest.fail("encoded bytes should not be collected"))
    fs = DataFileSystem(fo="orders.csv", fs=memory_fs, provided_schema=SCHEMA_OPTIONS)

    assert pq.read_table(fs.open("orders.parquet")).to_pylist() == [
        {"id": 1, "name": "ada"},
        {"id": 2, "name": "grace"},
    ]


def test_fsspec_url_builds_chained_filesystem(memory_fs):
    fsspec.register_implementation("fsspec-data", DataFileSystem, clobber=True)

    fs, path = fsspec.core.url_to_fs(
        "fsspec-data://orders.parquet::memory://orders.csv",
        provided_schema=SCHEMA_OPTIONS,
    )

    assert isinstance(fs, DataFileSystem)
    assert path == "orders.parquet"
    assert fs.fo == "/orders.csv"
    assert pq.read_table(fs.open(path)).to_pylist() == [{"id": 1, "name": "ada"}, {"id": 2, "name": "grace"}]


def test_source_url_constructs_target_filesystem(memory_fs):
    fs = DataFileSystem(fo="memory://orders.csv", provided_schema=SCHEMA_OPTIONS)

    assert pq.read_table(fs.open("orders.parquet")).to_pylist() == [
        {"id": 1, "name": "ada"},
        {"id": 2, "name": "grace"},
    ]


def test_target_filesystem_arguments_are_mutually_exclusive(memory_fs):
    with pytest.raises(ValueError, match="either fs or target_protocol"):
        DataFileSystem(fo="orders.csv", fs=memory_fs, target_protocol="memory", provided_schema=SCHEMA)


def test_embedded_schema_can_be_projected(memory_fs):
    parquet = pa.BufferOutputStream()
    pq.write_table(pa.table({"id": [1, 2], "name": ["ada", "grace"]}), parquet)
    memory_fs.pipe("orders.parquet", parquet.getvalue().to_pybytes())
    fs = DataFileSystem(
        fo="orders.parquet",
        fs=memory_fs,
        requested_schema={"fields": [{"name": "name", "type": "string"}]},
        schema_policy="projection",
    )

    with fs.open("orders.arrow") as file:
        result = pa.ipc.open_stream(file).read_all()

    assert result.to_pylist() == [{"name": "ada"}, {"name": "grace"}]


def test_embedded_schema_is_asserted(memory_fs):
    parquet = pa.BufferOutputStream()
    pq.write_table(pa.table({"id": [1]}), parquet)
    memory_fs.pipe("orders.parquet", parquet.getvalue().to_pybytes())
    fs = DataFileSystem(fo="orders.parquet", fs=memory_fs, provided_schema=pa.schema([("id", pa.int32())]))

    with pytest.raises(ValueError, match="provided schema does not match"):
        fs.open("orders.arrow")


def test_converted_output_rolls_over_at_spool_limit(memory_fs):
    fs = DataFileSystem(fo="orders.csv", fs=memory_fs, provided_schema=SCHEMA, spool_max_size=1)

    with fs.open("orders.parquet") as file:
        assert file._rolled
        assert file.read(4) == b"PAR1"


def test_info_and_listing_describe_converted_path(memory_fs):
    fs = DataFileSystem(fo="orders.csv", fs=memory_fs, provided_schema=SCHEMA)

    info = fs.info("orders.parquet")

    assert info == {"name": "orders.parquet", "size": info["size"], "type": "file"}
    assert info["size"] > 4
    assert fs.info("orders.parquet") == info
    assert fs.ls("orders.parquet") == [info]
    assert fs.ls("orders.parquet", detail=False) == ["orders.parquet"]
    assert fs.ls("") == []


def test_filesystem_is_read_only(memory_fs):
    fs = DataFileSystem(fo="orders.csv", fs=memory_fs, provided_schema=SCHEMA)

    with pytest.raises(ValueError, match="read-only"):
        fs.open("orders.parquet", "wb")


def test_text_input_requires_schema(memory_fs):
    fs = DataFileSystem(fo="orders.csv", fs=memory_fs)

    with pytest.raises(ValueError, match="requires an Arrow schema"):
        fs.open("orders.parquet")


def test_unknown_extension_requires_explicit_format(memory_fs):
    fs = DataFileSystem(fo="orders.csv", fs=memory_fs, provided_schema=SCHEMA)

    with pytest.raises(ValueError, match="cannot infer tabular format"):
        fs.open("orders.bin")


def test_extensionless_path_requires_explicit_format(memory_fs):
    fs = DataFileSystem(fo="orders.csv", fs=memory_fs, provided_schema=SCHEMA)

    with pytest.raises(ValueError, match="cannot infer tabular format"):
        fs.open("orders")


def test_schema_options_accept_a_field_sequence(memory_fs):
    fs = DataFileSystem(
        fo="orders.csv",
        fs=memory_fs,
        provided_schema=[
            {"name": "id", "type": pa.int64(), "nullable": False},
            {"name": "name", "type": "string"},
        ],
    )

    assert pq.read_table(fs.open("orders.parquet")).to_pylist() == [
        {"id": 1, "name": "ada"},
        {"id": 2, "name": "grace"},
    ]


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
        DataFileSystem(fo="orders.csv", fs=memory_fs, provided_schema=schema)
