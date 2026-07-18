# How to convert a file through an fsspec chain

This guide shows you how to expose a source file in another tabular format and Arrow schema
without replacing its filesystem.

## Define schemas for text input

CSV and JSONL sources require an Arrow schema. Use nested dictionaries when the same options
must cross the `fsspec-rs` Python bridge:

```python
schema = {
    "fields": [
        {"name": "id", "type": "int64", "nullable": False},
        {"name": "name", "type": "string", "nullable": True},
    ]
}
```

Pass a `pyarrow.Schema` instead when the chain remains in Python. Arrow IPC and Parquet
sources provide their own schemas, so `provided_schema` is optional for those formats.

## Open the converted representation

Place the requested representation on the left and the source object on the right:

```python
import fsspec
import pyarrow.parquet as pq

url = "fsspec-data://orders.parquet::memory://orders.csv"

with fsspec.open(url, provided_schema=schema) as file:
    orders = pq.read_table(file)
```

The outer filename selects Parquet output. The inner filename selects CSV input. Use
`provided_format` or `requested_format` when a filename has no recognized suffix.

To project, reorder, or cast fields, pass `requested_schema` and the corresponding
`schema_policy`:

```python
with fsspec.open(
    url,
    provided_schema=schema,
    requested_schema={"fields": [{"name": "name", "type": "string"}]},
    schema_policy="projection",
) as file:
    names = pq.read_table(file)
```

For a database source, pass connection settings to the inner protocol:

```python
url = "fsspec-data://orders.parquet::db+duckdb:///main/orders.arrow"

with fsspec.open(url, **{"db+duckdb": {"database": "warehouse.duckdb"}}) as file:
    orders = pq.read_table(file)
```

See the [API reference](api.md#datafilesystem) for constructor options and supported suffixes.
