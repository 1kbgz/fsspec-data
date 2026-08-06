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

## Resolve schemas from independent filesystems

Use a schema reference when a schema is stored separately from the data. Each reference owns
its storage options, so provided and requested schemas can use different backends or
credentials:

```python
provided_schema = {
    "url": "s3://input-schemas/orders.json",
    "format": "arrow-json",
    "storage_options": {"profile": "input-schema-reader"},
}
requested_schema = {
    "url": "s3://output-schemas/orders.schema.json",
    "format": "json-schema",
    "storage_options": {"profile": "output-schema-reader"},
}
```

`arrow-json` uses the `fields` descriptor shown above. `arrow-ipc-schema` accepts bytes from
`pyarrow.Schema.serialize()` or an Arrow IPC stream. A string URL is shorthand for a reference
when its suffix identifies an Arrow IPC schema:

```python
requested_schema = "s3://output-schemas/orders.arrow"
```

`json-schema` accepts a structural JSON Schema whose root is an object. It maps JSON Schema
objects, arrays with one `items` schema, strings, booleans, integers, numbers, and nulls to
Arrow. A union may contain null and one other type. Object `required` entries determine Arrow
field nullability; optional properties and explicit null unions are nullable.

References, composition keywords, multiple non-null union members, schema-valued
`additionalProperties`, tuple arrays, and `format` annotations are rejected with a path to the
unsupported field. Validation-only keywords such as numeric bounds and string lengths are not
represented or enforced by the Arrow schema.

## Resolve JSON Schema from Confluent Schema Registry

Use a `confluent://SUBJECT/versions/VERSION` reference for a JSON Schema registered with
Confluent. Put registry connection options on that reference:

```python
provided_schema = {
    "url": "confluent://orders-value/versions/latest",
    "format": "json-schema",
    "storage_options": {
        "registry_url": "https://schema-registry.example.com",
        "username": "schema-api-key",
        "password": "schema-api-secret",
        "timeout": 10,
    },
}
```

Pass `headers` in `storage_options` for additional string-valued HTTP headers. The resolver
supports JSON Schema records without external schema references. It rejects Avro, Protobuf,
and registry records that contain references.

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
    provided_schema=provided_schema,
    requested_schema=requested_schema,
    schema_policy="projection",
) as file:
    names = pq.read_table(file)
```

The data filesystem and each schema reference keep separate options. For example, the inner
S3 URL can use fsspec's `s3` options while the provided and requested schema references use
different registry credentials.

For a database source, pass connection settings to the inner protocol:

```python
url = "fsspec-data://orders.parquet::db+duckdb:///main/orders.arrow"

with fsspec.open(url, **{"db+duckdb": {"database": "warehouse.duckdb"}}) as file:
    orders = pq.read_table(file)
```

See the [API reference](api.md#datafilesystem) for constructor options and supported suffixes.
