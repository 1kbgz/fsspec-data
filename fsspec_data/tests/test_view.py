import io
import json
from typing import ClassVar

import fsspec
import pyarrow as pa
import pyarrow.csv as pcsv
import pyarrow.parquet as pq
import pytest

from fsspec_data import DataSource, Facet, Node
from fsspec_data.view import DataViewFileSystem


@pytest.fixture
def memory():
    fs = fsspec.filesystem("memory")
    fs.store.clear()
    fs.pseudo_dirs.clear()
    fs.pseudo_dirs.append("")
    fs.pipe_file("/data/orders.csv", b"id,customer\n1,a\n2,b\n")
    fs.pipe_file("/data/notes.txt", b"hello")
    buffer = io.BytesIO()
    pq.write_table(pa.table({"id": [7, 8]}), buffer)
    fs.pipe_file("/data/nested/ids.parquet", buffer.getvalue())
    return fs


def view(fs, **options):
    return DataViewFileSystem("/data", fs=fs, **options)


def read(fs, path):
    with fs.open(path, "rb") as file:
        return file.read()


def test_default_mapping_lists_native_files_conversions_and_metadata(memory):
    names = view(memory).ls("", detail=False)
    assert names == [
        ".json",
        ".metadata.json",
        "nested",
        "nested.json",
        "nested.metadata.json",
        "notes.txt",
        "notes.txt.metadata.json",
        "orders.csv",
        "orders.metadata.json",
        "orders.parquet",
        "orders.schema.json",
    ]
    assert view(memory).ls("nested", detail=False) == [
        "nested/ids.metadata.json",
        "nested/ids.parquet",
        "nested/ids.schema.json",
    ]


def test_rendered_files_hold_converted_rows_schemas_and_metadata(memory):
    fs = view(memory)
    assert pq.read_table(io.BytesIO(read(fs, "orders.parquet"))).to_pydict() == {"id": [1, 2], "customer": ["a", "b"]}
    assert read(fs, "orders.csv") == b"id,customer\n1,a\n2,b\n"
    assert read(fs, "notes.txt") == b"hello"
    assert json.loads(read(fs, "orders.schema.json")) == {
        "fields": [{"name": "id", "type": "int64", "nullable": True}, {"name": "customer", "type": "string", "nullable": True}]
    }
    assert json.loads(read(fs, "notes.txt.metadata.json"))["size"] == 5
    assert [row["name"] for row in json.loads(read(fs, "nested.json"))] == ["ids.parquet"]
    assert fs.info("orders.parquet")["size"] > 0
    assert fs.info("nested")["type"] == "directory"


def test_mapping_decides_what_appears(memory):
    fs = view(memory, mapping={"table": ["parquet", "csv"], "index": ["csv"]})
    assert fs.ls("", detail=False) == [".csv", "nested", "nested.csv", "orders.csv", "orders.parquet"]
    assert read(fs, "orders.csv") == b"id,customer\n1,a\n2,b\n"
    assert pcsv.read_csv(io.BytesIO(read(fs, "nested.csv"))).column_names[:2] == ["name", "size"]
    with pytest.raises(ValueError, match="unknown forms"):
        view(memory, mapping={"table": ["xlsx"]})
    with pytest.raises(ValueError, match="unknown facet"):
        view(memory, mapping={"rows": ["parquet"]})


def test_two_nodes_rendering_one_name_is_an_error(memory):
    memory.pipe_file("/data/orders.jsonl", b'{"id": 3}\n')
    with pytest.raises(ValueError, match="'orders.parquet' is rendered from both"):
        view(memory).ls("")


def test_resolve_maps_view_paths_back_to_nodes(memory):
    entry = view(memory).resolve("orders.parquet")
    assert (entry.node.path, entry.facet, entry.form, entry.url) == ("/data/orders.csv", "table", "parquet", "memory:///data/orders.csv")
    assert view(memory).resolve("nested/ids.schema.json").facet == "schema"
    with pytest.raises(FileNotFoundError):
        view(memory).resolve("missing.parquet")


def test_views_open_through_fsspec_urls(memory):
    with fsspec.open("dataview://orders.parquet::memory:///data", "rb") as file:
        assert pq.read_table(file).num_rows == 2
    with fsspec.open("dataview://nested.csv::memory:///data", "rb", dataview={"mapping": {"index": ["csv"]}}) as file:
        assert file.read().startswith(b"name,size")
    fs, path = fsspec.core.url_to_fs("dataview::memory:///data")
    assert path == "/data"
    assert "data/orders.parquet" in fs.ls(path, detail=False)
    assert pq.read_table(fs.open(f"{path}/orders.parquet")).num_rows == 2
    with pytest.raises(ValueError, match="read-only"):
        view(memory).open("orders.parquet", "wb")


class Catalog(DataSource):
    """A data catalog: ``category/team/datasets/{name}`` datasets with tags at every level.
    ``.../datasets/archive`` exists but is never listed."""

    tags: ClassVar[dict[str, dict[str, str]]] = {
        "": {"owner": "platform"},
        "finance": {"steward": "cfo"},
        "finance/rates": {"team": "rates"},
        "finance/rates/datasets": {},
        "finance/rates/datasets/curves": {"refresh": "daily"},
        "finance/rates/datasets/archive": {"refresh": "never"},
    }
    curves = pa.table({"tenor": ["1y", "5y", "10y"]})

    def describe(self, path):
        path = path.strip("/")
        if path not in self.tags:
            raise FileNotFoundError(path)
        if path.startswith("finance/rates/datasets/"):
            return Node(path, [Facet.TABLE, Facet.METADATA])
        return Node(path, [Facet.CHILDREN, Facet.METADATA])

    def children(self, path):
        path = path.strip("/")
        return [self.describe(key) for key in self.tags if key and key.rpartition("/")[0] == path and not key.endswith("archive")]

    def schema(self, path):
        return self.curves.schema

    def scan(self, path, **options):
        return pa.RecordBatchReader.from_batches(self.curves.schema, self.curves.to_batches())

    def metadata(self, path):
        return self.tags[path.strip("/")]


def test_a_catalog_renders_datasets_and_metadata_at_every_level():
    fs = DataViewFileSystem(source=Catalog())
    assert fs.ls("", detail=False) == [".json", ".metadata.json", "finance", "finance.json", "finance.metadata.json"]
    assert json.loads(read(fs, ".metadata.json")) == {"owner": "platform"}
    assert fs.ls("finance/rates/datasets", detail=False) == [
        "finance/rates/datasets/curves.metadata.json",
        "finance/rates/datasets/curves.parquet",
        "finance/rates/datasets/curves.schema.json",
    ]
    assert json.loads(read(fs, "finance/rates.json")) == [{"name": "datasets"}]
    assert pq.read_table(io.BytesIO(read(fs, "finance/rates/datasets/curves.parquet"))).num_rows == 3
    assert json.loads(read(fs, "finance/rates/datasets/archive.metadata.json")) == {"refresh": "never"}
    assert fs.resolve("finance/rates/datasets/curves.parquet").url is None


def test_views_check_arguments_paths_and_forms(memory):
    with pytest.raises(ValueError, match="only one of"):
        DataViewFileSystem("/data", fs=memory, source=Catalog())
    with pytest.raises(TypeError, match="must be a list"):
        view(memory, mapping={"table": "parquet"})

    fs = DataViewFileSystem("memory:///data", mapping={"table": ["json"]})
    assert fs.resolve("").facet == "children"
    assert fs.info("orders.json")["size"] > 0
    assert json.loads(read(fs, "orders.json")) == [{"id": 1, "customer": "a"}, {"id": 2, "customer": "b"}]
    with pytest.raises(IsADirectoryError):
        fs.open("nested")
    with pytest.raises(NotADirectoryError):
        fs.ls("orders.json")

    memory.pipe_file("/data/later.csv", b"id\n1\n")
    assert "later.json" not in fs.ls("", detail=False)
    fs.invalidate_cache()
    assert "later.json" in fs.ls("", detail=False)
