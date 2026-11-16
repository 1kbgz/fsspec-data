# How to convert a file through an fsspec chain

Open a `dataview` URL with the file you want on the left and its source on the right. The
source can be a single object or a directory. Arrow IPC and Parquet carry their own schema;
CSV and JSONL schemas are inferred from the start of the file unless you pass
`provided_schema`.

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
url = "dataview://orders.parquet::memory://orders.csv"
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

## Rename, add, or drop fields

Pass field operations with the requested schema. Renames map provided names to requested
names; defaults fill requested fields that have no source; `drop` removes provided fields:

```python
requested_schema = {
    "fields": [
        {"name": "customer_id", "type": "int64", "nullable": False},
        {"name": "region", "type": "string", "nullable": False},
    ]
}
with fsspec.open(
    url,
    "rb",
    provided_schema=schema,
    requested_schema=requested_schema,
    schema_policy="compatible",
    renames={"id": "customer_id"},
    defaults={"region": "us"},
    drop=["name"],
) as file:
    customers = pq.read_table(file)
```

Under `compatible` and `coerce`, every provided field must appear in the output or in
`drop`. Under `projection`, unused fields are dropped without listing them. Add
`casts={"amount": "lossy"}` to allow a cast for one field that the policy would reject.

## Keep backend options scoped

For a database source, view a schema directory and pass connection settings under the
database's protocol:

```python
url = "dataview://orders.parquet::db+duckdb:///main"
with fsspec.open(url, "rb", **{"db+duckdb": {"database": "warehouse.duckdb"}}) as file:
    orders = pq.read_table(file)
```

The schema options apply to every table the view converts. To load schemas independently
of the data, follow
[How to resolve schemas from files and registries](how-to-resolve-schemas.md). See the
[`DataViewFileSystem` reference](api.md#dataviewfilesystem) for limits and buffering.
