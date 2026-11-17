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


@pytest.mark.parametrize("path", ["/data/orders.csv", "/data/nested/ids.parquet"])
def test_file_column_selection_preserves_rows_with_no_columns(files, path):
    stream = files.scan_columns(path, [], batch_size=1, row_limit=1, byte_limit=0)
    assert stream.schema.names == []
    batches = list(stream)
    assert [batch.num_rows for batch in batches] == [1]
    assert batches[0].num_columns == 0


def test_file_column_selection_order_duplicates_and_unknown_names(files):
    stream = files.scan_columns("/data/orders.csv", ["customer", "id", "customer"])
    assert stream.schema.names == ["customer", "id", "customer"]
    batch = next(stream)
    assert batch.column(0).to_pylist() == ["a", "b"]
    assert batch.column(1).to_pylist() == [1, 2]
    assert batch.column(2).equals(batch.column(0))
    with pytest.raises(ValueError, match="missing"):
        files.scan_columns("/data/orders.csv", ["missing"])


def test_source_column_selection_fallback_limits_and_cancellation():
    source = Catalog()
    path = "finance/rates/datasets/curves"
    stream = source.scan_columns(path, [], byte_limit=0, batch_size=1, row_limit=2)
    assert [batch.num_rows for batch in stream] == [1, 1]
    stream = source.scan_columns(path, ["tenor", "tenor"], batch_size=1)
    assert stream.schema.names == ["tenor", "tenor"]
    assert next(stream).column(0).to_pylist() == ["1y"]
    stream.cancel()
    with pytest.raises(RuntimeError, match="cancel"):
        next(stream)
    with pytest.raises(ValueError, match="missing"):
        source.scan_columns(path, ["missing"])


def test_parquet_selection_preserves_nested_columns_and_metadata(files):
    table = pa.table({"id": [1, 2], "details": [{"name": "a", "count": 3}, {"name": "b", "count": 4}]})
    table = table.replace_schema_metadata({b"source": b"test"})
    buffer = io.BytesIO()
    pq.write_table(table, buffer)
    files.fs.pipe_file("/data/nested.parquet", buffer.getvalue())
    stream = files.scan_columns("/data/nested.parquet", ["details", "id", "details"])
    expected = table.select(["details", "id", "details"])
    assert stream.schema.equals(expected.schema, check_metadata=True)
    actual = pa.Table.from_batches(list(stream))
    assert actual.equals(expected, check_metadata=True)


@pytest.mark.parametrize("format", [DataFormat.ARROW, DataFormat.PARQUET])
@pytest.mark.parametrize("file_like", [False, True])
def test_codec_column_selection_bytes_and_readers(format, file_like):
    from fsspec_data import DEFAULT_REGISTRY

    codec = DEFAULT_REGISTRY.get(format)
    batch = pa.record_batch({"id": [1, 2], "payload": ["x" * 1000, "y" * 1000]})
    encoded = codec.encode_batches([batch])
    data = io.BytesIO(encoded) if file_like else encoded
    result = codec.decode_batches(data, columns=["id"], byte_limit=3000)
    assert result.schema.names == ["id"]
    assert [value for batch in result.batches for value in batch.column(0).to_pylist()] == [1, 2]


@pytest.mark.parametrize("cancel_fails", [False, True])
def test_column_selection_cancels_source(cancel_fails):
    class CancellableBatches:
        schema = Catalog.curves.schema

        def __init__(self):
            self.batches = iter(Catalog.curves.to_batches())
            self.cancelled = False

        def __iter__(self):
            return self

        def __next__(self):
            return next(self.batches)

        def cancel(self):
            self.cancelled = True
            if cancel_fails:
                raise RuntimeError("source cancellation failed")

    class CancellableCatalog(Catalog):
        def scan(self, path, **options):
            self.stream = CancellableBatches()
            return self.stream

    source = CancellableCatalog()
    stream = source.scan_columns("finance/rates/datasets/curves", ["tenor"], batch_size=1)
    assert next(stream).column(0).to_pylist() == ["1y"]
    if cancel_fails:
        with pytest.raises(RuntimeError, match="source cancellation failed"):
            stream.cancel()
    else:
        stream.cancel()
    assert source.stream.cancelled
    with pytest.raises(RuntimeError, match="batch stream was cancelled"):
        next(stream)
