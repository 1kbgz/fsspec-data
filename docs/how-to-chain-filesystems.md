# How to convert a file through an fsspec chain

Place the requested representation on the left and the source object on the right.
CSV and JSONL input require a schema; Arrow IPC and Parquet carry their own.

## Convert CSV to Parquet

Create source bytes on an in-memory filesystem and open the converted view:

```python
import fsspec
import pyarrow.parquet as pq

import fsspec_data

source = fsspec.filesystem("memory")
source.pipe_file("/orders.csv", b"id,name\n1,Ada\n2,Grace\n")
schema = {
    "fields": [
        {"name": "id", "type": "int64", "nullable": False},
        {"name": "name", "type": "string", "nullable": True},
    ]
}
url = "fsspec-data://orders.parquet::memory://orders.csv"
with fsspec.open(url, "rb", provided_schema=schema) as file:
    orders = pq.read_table(file)
print(orders.to_pylist())
```

The result is `[{'id': 1, 'name': 'Ada'}, {'id': 2, 'name': 'Grace'}]`.
For remote input, install its backend and replace the inner memory URL.

## Select output fields

Pass a requested schema with the `projection` policy:

```python
requested_schema = {"fields": [{"name": "name", "type": "string"}]}
with fsspec.open(
    url,
    "rb",
    provided_schema=schema,
    requested_schema=requested_schema,
    schema_policy="projection",
) as file:
    names = pq.read_table(file)
```

Use `compatible` or `coerce` for casts permitted by those policies. Use a
`pyarrow.Schema` directly when options stay in Python; use nested descriptors
when options cross the Rust/Python bridge.

## Keep backend options scoped

For a database source, pass connection settings under its protocol:

```python
url = "fsspec-data://orders.parquet::db+duckdb:///main/orders.arrow"
with fsspec.open(url, "rb", **{"db+duckdb": {"database": "warehouse.duckdb"}}) as file:
    orders = pq.read_table(file)
```

Use `provided_format` or `requested_format` when a filename lacks a recognized
suffix. To load schemas independently of the data, follow
[How to resolve schemas from files and registries](how-to-resolve-schemas.md).
See [DataFileSystem reference](api.md) for limits and buffering.
