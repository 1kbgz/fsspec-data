import io
import json

import fsspec
import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import fsspec_data.confluent as confluent_module
import fsspec_data.schema as schema_module
from fsspec_data import DataFileSystem, SchemaRef, json_schema_to_arrow, resolve_schema

SOURCE_SCHEMA = pa.schema(
    [
        pa.field("id", pa.int64(), nullable=False),
        pa.field("name", pa.string()),
    ]
)
TARGET_SCHEMA = pa.schema([pa.field("name", pa.string())])
SOURCE_SCHEMA_JSON = {
    "fields": [
        {"name": "id", "type": "int64", "nullable": False},
        {"name": "name", "type": "string", "nullable": True},
    ]
}
SOURCE_JSON_SCHEMA = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "properties": {
        "id": {"type": "integer"},
        "name": {"type": "string"},
    },
    "required": ["id", "name"],
    "additionalProperties": False,
}
TARGET_JSON_SCHEMA = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "properties": {"name": {"type": "string"}},
    "required": ["name"],
    "additionalProperties": False,
}


def test_schema_reference_hides_storage_options_from_repr():
    reference = SchemaRef(
        "s3://schemas/source.json",
        format="arrow-json",
        storage_options={"secret": "do-not-display"},
    )

    assert "do-not-display" not in repr(reference)


def test_arrow_ipc_schema_format_is_inferred(monkeypatch):
    monkeypatch.setattr(
        schema_module.fsspec,
        "open",
        lambda url, mode, **options: io.BytesIO(SOURCE_SCHEMA.serialize().to_pybytes()),
    )

    resolved = resolve_schema(SchemaRef("memory://schemas/source.arrow"))

    assert resolved is not None
    assert resolved.schema.equals(SOURCE_SCHEMA)
    assert resolved.provenance.source == "memory://schemas/source.arrow"
    assert resolved.provenance.format == "arrow-ipc-schema"


def test_schema_reference_requires_format_for_ambiguous_extension():
    with pytest.raises(ValueError, match="schema format cannot be inferred"):
        resolve_schema(SchemaRef("memory://schemas/source.json"))


def test_json_schema_maps_structural_types_and_nullability():
    schema = json_schema_to_arrow(
        {
            "type": "object",
            "properties": {
                "id": {"type": "integer"},
                "name": {"type": ["string", "null"]},
                "score": {"type": "number", "minimum": 0},
                "flags": {"type": "array", "items": {"type": "boolean"}},
                "profile": {
                    "type": "object",
                    "properties": {
                        "active": {"type": "boolean"},
                        "note": {"type": "string"},
                    },
                    "required": ["active"],
                },
            },
            "required": ["id", "name", "flags", "profile"],
        }
    )

    expected = pa.schema(
        [
            pa.field("id", pa.int64(), nullable=False),
            pa.field("name", pa.string()),
            pa.field("score", pa.float64()),
            pa.field("flags", pa.list_(pa.field("item", pa.bool_(), nullable=False)), nullable=False),
            pa.field(
                "profile",
                pa.struct(
                    [
                        pa.field("active", pa.bool_(), nullable=False),
                        pa.field("note", pa.string()),
                    ]
                ),
                nullable=False,
            ),
        ]
    )

    assert schema.equals(expected, check_metadata=False)


def test_data_filesystem_resolves_independent_schema_sources(monkeypatch):
    opened = []

    def open_schema(url, mode, **storage_options):
        opened.append((url, mode, storage_options))
        if url.endswith("source.json"):
            return io.BytesIO(json.dumps(SOURCE_SCHEMA_JSON).encode())
        if url.endswith("target.arrow"):
            return io.BytesIO(TARGET_SCHEMA.serialize().to_pybytes())
        raise AssertionError(url)

    monkeypatch.setattr(schema_module.fsspec, "open", open_schema)
    source_fs = fsspec.filesystem("memory")
    source_fs.store.clear()
    source_fs.pipe("orders.csv", b"id,name\n1,ada\n2,grace\n")
    fs = DataFileSystem(
        fo="orders.csv",
        fs=source_fs,
        provided_schema={
            "url": "memory://schemas/source.json",
            "format": "arrow-json",
            "storage_options": {"token": "provided-token"},
        },
        requested_schema={
            "url": "memory://schemas/target.arrow",
            "format": "arrow-ipc-schema",
            "storage_options": {"token": "requested-token"},
        },
        schema_policy="projection",
    )

    result = pq.read_table(fs.open("orders.parquet"))

    assert result.schema.equals(TARGET_SCHEMA, check_metadata=False)
    assert result.to_pylist() == [{"name": "ada"}, {"name": "grace"}]
    assert opened == [
        ("memory://schemas/source.json", "rb", {"token": "provided-token"}),
        ("memory://schemas/target.arrow", "rb", {"token": "requested-token"}),
    ]
    assert fs.provided_schema_provenance is not None
    assert fs.requested_schema_provenance is not None
    assert fs.provided_schema_provenance.source == "memory://schemas/source.json"
    assert fs.requested_schema_provenance.source == "memory://schemas/target.arrow"


def test_fsspec_open_resolves_referenced_schemas():
    memory_fs = fsspec.filesystem("memory")
    memory_fs.store.clear()
    memory_fs.pipe("orders.csv", b"id,name\n1,ada\n2,grace\n")
    memory_fs.pipe("schemas/source.json", json.dumps(SOURCE_SCHEMA_JSON).encode())
    memory_fs.pipe("schemas/target.arrow", TARGET_SCHEMA.serialize().to_pybytes())
    fsspec.register_implementation("fsspec-data", DataFileSystem, clobber=True)

    with fsspec.open(
        "fsspec-data://orders.parquet::memory://orders.csv",
        provided_schema={
            "url": "memory://schemas/source.json",
            "format": "arrow-json",
        },
        requested_schema="memory://schemas/target.arrow",
        schema_policy="projection",
    ) as file:
        result = pq.read_table(file)

    assert result.schema.equals(TARGET_SCHEMA, check_metadata=False)
    assert result.to_pylist() == [{"name": "ada"}, {"name": "grace"}]


def test_fsspec_open_resolves_json_schemas():
    memory_fs = fsspec.filesystem("memory")
    memory_fs.store.clear()
    memory_fs.pipe("orders.csv", b"id,name\n1,ada\n2,grace\n")
    memory_fs.pipe("schemas/source.schema.json", json.dumps(SOURCE_JSON_SCHEMA).encode())
    memory_fs.pipe("schemas/target.schema.json", json.dumps(TARGET_JSON_SCHEMA).encode())
    fsspec.register_implementation("fsspec-data", DataFileSystem, clobber=True)

    with fsspec.open(
        "fsspec-data://orders.parquet::memory://orders.csv",
        provided_schema={
            "url": "memory://schemas/source.schema.json",
            "format": "json-schema",
        },
        requested_schema={
            "url": "memory://schemas/target.schema.json",
            "format": "json-schema",
        },
        schema_policy="projection",
    ) as file:
        result = pq.read_table(file)

    assert result.schema.equals(json_schema_to_arrow(TARGET_JSON_SCHEMA), check_metadata=False)
    assert result.to_pylist() == [{"name": "ada"}, {"name": "grace"}]


def test_confluent_resolver_fetches_json_schema_with_scoped_credentials(monkeypatch):
    requests = []

    def urlopen(request, *, timeout):
        requests.append((request, timeout))
        return io.BytesIO(
            json.dumps(
                {
                    "subject": "orders-value",
                    "version": 7,
                    "id": 42,
                    "schemaType": "JSON",
                    "schema": json.dumps(SOURCE_JSON_SCHEMA),
                }
            ).encode()
        )

    monkeypatch.setattr(confluent_module.urllib.request, "urlopen", urlopen)
    reference = SchemaRef(
        "confluent://orders-value/versions/latest",
        format="json-schema",
        storage_options={
            "registry_url": "https://schemas.example.test",
            "username": "schema-key",
            "password": "schema-secret",
            "headers": {"X-Tenant": "analytics"},
            "timeout": 3,
        },
    )

    resolved = resolve_schema(reference)

    assert resolved is not None
    assert resolved.schema.equals(json_schema_to_arrow(SOURCE_JSON_SCHEMA), check_metadata=False)
    assert resolved.provenance.source == "confluent://orders-value/versions/latest"
    assert resolved.provenance.format == "json-schema"
    assert resolved.provenance.provider == "confluent"
    assert resolved.provenance.identifier == "42"
    assert resolved.provenance.version == "7"
    assert "schema-secret" not in repr(reference)
    assert "schema-secret" not in repr(resolved.provenance)
    assert len(requests) == 1
    request, timeout = requests[0]
    assert request.full_url == "https://schemas.example.test/subjects/orders-value/versions/latest"
    assert request.get_header("Authorization") == "Basic c2NoZW1hLWtleTpzY2hlbWEtc2VjcmV0"
    assert request.get_header("X-tenant") == "analytics"
    assert timeout == 3.0


def test_fsspec_open_composes_confluent_schema_with_file_schema(monkeypatch):
    monkeypatch.setattr(
        confluent_module.urllib.request,
        "urlopen",
        lambda request: io.BytesIO(
            json.dumps(
                {
                    "subject": "orders-value",
                    "version": 7,
                    "id": 42,
                    "schemaType": "JSON",
                    "schema": json.dumps(SOURCE_JSON_SCHEMA),
                }
            ).encode()
        ),
    )
    memory_fs = fsspec.filesystem("memory")
    memory_fs.store.clear()
    memory_fs.pipe("orders.csv", b"id,name\n1,ada\n2,grace\n")
    memory_fs.pipe("schemas/target.schema.json", json.dumps(TARGET_JSON_SCHEMA).encode())
    fsspec.register_implementation("fsspec-data", DataFileSystem, clobber=True)

    with fsspec.open(
        "fsspec-data://orders.parquet::memory://orders.csv",
        provided_schema={
            "url": "confluent://orders-value/versions/latest",
            "format": "json-schema",
            "storage_options": {"registry_url": "https://schemas.example.test"},
        },
        requested_schema={
            "url": "memory://schemas/target.schema.json",
            "format": "json-schema",
        },
        schema_policy="projection",
    ) as file:
        result = pq.read_table(file)

    assert result.schema.equals(json_schema_to_arrow(TARGET_JSON_SCHEMA), check_metadata=False)
    assert result.to_pylist() == [{"name": "ada"}, {"name": "grace"}]


@pytest.mark.parametrize(
    ("payload", "message"),
    [
        ({"schemaType": "AVRO", "schema": "{}"}, "schema type 'AVRO' is not supported"),
        (
            {
                "schemaType": "JSON",
                "schema": json.dumps(SOURCE_JSON_SCHEMA),
                "references": [{"name": "base", "subject": "base", "version": 1}],
            },
            "schema references are not supported yet",
        ),
    ],
)
def test_confluent_resolver_rejects_unsupported_documents(monkeypatch, payload, message):
    monkeypatch.setattr(
        confluent_module.urllib.request,
        "urlopen",
        lambda request: io.BytesIO(json.dumps(payload).encode()),
    )

    with pytest.raises(ValueError, match=message):
        resolve_schema(
            SchemaRef(
                "confluent://orders-value/versions/latest",
                storage_options={"registry_url": "https://schemas.example.test"},
            )
        )


def test_confluent_resolver_requires_registry_url():
    with pytest.raises(ValueError, match=r"require storage_options\['registry_url'\]"):
        resolve_schema(SchemaRef("confluent://orders-value/versions/latest"))


def test_confluent_resolver_rejects_invalid_version():
    with pytest.raises(ValueError, match="VERSION must be 'latest', -1, or a positive integer"):
        resolve_schema(
            SchemaRef(
                "confluent://orders-value/versions/newest",
                storage_options={"registry_url": "https://schemas.example.test"},
            )
        )


@pytest.mark.parametrize(
    ("schema", "message"),
    [
        (
            {"type": "object", "properties": {"value": {"anyOf": [{"type": "integer"}, {"type": "string"}]}}},
            r"\$.properties.value: keyword 'anyOf'",
        ),
        (
            {"type": "object", "properties": {"value": {"type": ["integer", "string"]}}},
            r"\$.properties.value: unions may contain only one non-null type",
        ),
        (
            {"type": "object", "properties": {"created": {"type": "string", "format": "date-time"}}},
            r"\$.properties.created: format 'date-time'",
        ),
        (
            {"type": "object", "properties": {"value": {"$ref": "#/$defs/value"}}},
            r"\$.properties.value: keyword '\$ref'",
        ),
        (
            {"type": "object", "additionalProperties": {"type": "string"}},
            r"\$: schema-valued 'additionalProperties'",
        ),
        (
            {"type": "object", "properties": {}, "required": ["missing"]},
            r"\$ requires unknown properties: missing",
        ),
    ],
)
def test_json_schema_rejects_ambiguous_constructs(schema, message):
    with pytest.raises(ValueError, match=message):
        json_schema_to_arrow(schema)


def test_schema_resolution_precedes_source_read(monkeypatch):
    monkeypatch.setattr(
        schema_module.fsspec,
        "open",
        lambda url, mode, **options: (_ for _ in ()).throw(ValueError("schema unavailable")),
    )
    source_fs = fsspec.filesystem("memory")
    monkeypatch.setattr(source_fs, "open", lambda *args, **kwargs: pytest.fail("source data must not be opened"))
    fs = DataFileSystem(
        fo="orders.csv",
        fs=source_fs,
        provided_schema={"url": "memory://schemas/source.json", "format": "arrow-json"},
    )

    with pytest.raises(ValueError, match="schema unavailable"):
        fs.open("orders.parquet")


def test_schema_reference_rejects_unknown_options():
    with pytest.raises(ValueError, match="unknown schema reference options: credentials"):
        DataFileSystem(
            fo="orders.csv",
            fs=fsspec.filesystem("memory"),
            provided_schema={"url": "memory://schemas/source.arrow", "credentials": {}},
        )
