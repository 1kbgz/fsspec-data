# How to resolve schemas from files and registries

Use separate schema references when data, source schema, and requested schema live
in different locations. Pass references to `DataFileSystem` through
`provided_schema` or `requested_schema`.

## Read a schema file

Construct a reference with its own backend options:

```python
from fsspec_data import SchemaRef, resolve_schema

reference = SchemaRef(
    "s3://input-schemas/orders.json",
    format="arrow-json",
    storage_options={"profile": "schema-reader"},
)
resolved = resolve_schema(reference)
print(resolved.schema)
```

Install the required backend, such as `s3fs`, first. Use `format="json-schema"` for
a supported structural JSON Schema, or `arrow-ipc-schema` for serialized Arrow
schema bytes or an Arrow IPC stream.

## Pass independent references through a chain

Use mappings when the options must cross the Rust/Python bridge:

```python
import fsspec

import fsspec_data

provided_schema = {
    "url": "s3://input-schemas/orders.json",
    "format": "arrow-json",
    "storage_options": {"profile": "input-schema-reader"},
}
requested_schema = {
    "url": "s3://output-schemas/orders.arrow",
    "storage_options": {"profile": "output-schema-reader"},
}
with fsspec.open(
    "fsspec-data://orders.parquet::s3://input-data/orders.csv",
    "rb",
    provided_schema=provided_schema,
    requested_schema=requested_schema,
    schema_policy="projection",
    s3={"profile": "data-reader"},
) as file:
    converted = file.read()
```

Choose a policy that accepts the actual schema mapping. The `.arrow` reference
suffix selects `arrow-ipc-schema` without an explicit `format`.

## Read Confluent JSON Schema

Use a subject/version URL and scope registry options to that reference:

```python
from fsspec_data import SchemaRef, resolve_schema

resolved = resolve_schema(SchemaRef(
    "confluent://orders-value/versions/latest",
    format="json-schema",
    storage_options={
        "registry_url": "https://schema-registry.example.com",
        "username": "schema-api-key",
        "password": "schema-api-secret",
        "timeout": 10,
    },
))
```

Supply actual registry credentials through your application's configuration.
Consult [Schema reference](resources.md) for supported versions, formats,
JSON Schema limits, and provenance fields.
