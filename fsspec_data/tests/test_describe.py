import io
from typing import ClassVar

import fsspec
import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from fsspec_data import DataFormat, DataSource, Facet, FacetNotSupportedError, FileDataSource, Node


@pytest.fixture
def files():
    fs = fsspec.filesystem("memory")
    fs.store.clear()
    fs.pseudo_dirs.clear()
    fs.pseudo_dirs.append("")
    fs.pipe_file("/data/orders.csv", b"id,customer\n1,a\n2,b\n")
    fs.pipe_file("/data/notes.txt", b"hello")
    buffer = io.BytesIO()
    pq.write_table(pa.table({"id": [7, 8]}), buffer)
    fs.pipe_file("/data/nested/ids.parquet", buffer.getvalue())
    return FileDataSource(fs)


def rows(stream):
    return sum(batch.num_rows for batch in stream)


def test_files_describe_directories_tables_and_other_files(files):
    children = files.children("/data")
    assert [(node.path, node.facets, node.native_format) for node in children] == [
        ("/data/nested", {Facet.CHILDREN, Facet.METADATA}, None),
        ("/data/notes.txt", {Facet.RAW, Facet.METADATA}, None),
        ("/data/orders.csv", {Facet.TABLE, Facet.RAW, Facet.METADATA}, DataFormat.CSV),
    ]
    assert files.describe("/data/orders.csv") == children[2]
    assert files.metadata("/data/notes.txt")["size"] == 5
    assert "size" not in files.metadata("/data/nested")


def test_files_read_tables_by_native_format(files):
    assert files.schema("/data/orders.csv").names == ["id", "customer"]
    assert rows(files.scan("/data/orders.csv")) == 2
    assert files.schema("/data/nested/ids.parquet").names == ["id"]
    assert rows(files.scan("/data/nested/ids.parquet", row_limit=1)) == 1

    with pytest.raises(FacetNotSupportedError) as error:
        files.schema("/data/notes.txt")
    assert error.value.facet is Facet.TABLE
    with files.open_raw("/data/notes.txt") as raw:
        assert raw.read() == b"hello"


def test_csv_schemas_are_inferred_from_complete_sample_records(files):
    files.sample_size = 14
    assert files.schema("/data/orders.csv").names == ["id", "customer"]


def test_data_format_from_path():
    assert DataFormat.from_path("a/b.PQ") is DataFormat.PARQUET
    assert DataFormat.from_path("a/b.ndjson") is DataFormat.JSONL
    assert DataFormat.from_path("a.d/b") is None
    assert DataFormat.from_path("notes.txt") is None


class Catalog(DataSource):
    """A data catalog: ``category/team/datasets/{name}`` datasets with tags at every level."""

    tags: ClassVar[dict[str, dict[str, str]]] = {
        "": {"owner": "platform"},
        "finance": {"steward": "cfo"},
        "finance/rates": {"team": "rates"},
        "finance/rates/datasets": {},
        "finance/rates/datasets/curves": {"refresh": "daily"},
    }
    curves = pa.table({"tenor": ["1y", "5y", "10y"]})

    def describe(self, path):
        path = path.strip("/")
        if path not in self.tags:
            raise FileNotFoundError(path)
        if path.endswith("datasets/curves"):
            return Node(path, [Facet.TABLE, Facet.METADATA])
        return Node(path, [Facet.CHILDREN, Facet.METADATA])

    def children(self, path):
        path = path.strip("/")
        return [self.describe(key) for key in self.tags if key and key.rpartition("/")[0] == path]

    def schema(self, path):
        return self.curves.schema

    def scan(self, path, **options):
        return iter(self.curves.to_batches())

    def metadata(self, path):
        return self.tags[path.strip("/")]


def test_a_catalog_backend_describes_metadata_at_every_level():
    catalog = Catalog()
    assert catalog.describe("/").has("children")
    assert catalog.metadata("/") == {"owner": "platform"}

    [curves] = catalog.children("finance/rates/datasets")
    assert curves.has(Facet.TABLE) and not curves.has(Facet.RAW)
    assert catalog.metadata(curves.path) == {"refresh": "daily"}
    assert rows(catalog.scan(curves.path)) == 3
    with pytest.raises(FacetNotSupportedError):
        catalog.open_raw(curves.path)


def test_facets_a_source_does_not_implement_raise_facet_not_supported():
    class Bare(DataSource):
        def describe(self, path):
            return Node(path, [])

    source = Bare()
    assert source.describe("x").facets == frozenset()
    calls = [
        (Facet.CHILDREN, source.children),
        (Facet.TABLE, source.schema),
        (Facet.TABLE, source.scan),
        (Facet.RAW, source.open_raw),
        (Facet.METADATA, source.metadata),
    ]
    for facet, call in calls:
        with pytest.raises(FacetNotSupportedError) as error:
            call("x")
        assert error.value.facet is facet
        assert str(error.value) == f"x has no {facet.value} facet"
    with pytest.raises(NotImplementedError):
        DataSource().describe("x")
