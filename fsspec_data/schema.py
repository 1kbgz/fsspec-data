from __future__ import annotations

import json
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field, replace
from enum import Enum
from typing import Any, TypeAlias, cast
from urllib.parse import urlsplit

import fsspec
import pyarrow as pa

from .confluent import fetch_confluent_schema
from .json_schema import json_schema_to_arrow


class SchemaFormat(str, Enum):
    ARROW_IPC = "arrow-ipc-schema"
    ARROW_JSON = "arrow-json"
    JSON_SCHEMA = "json-schema"


@dataclass(frozen=True)
class SchemaRef:
    url: str
    format: SchemaFormat | str | None = None
    storage_options: Mapping[str, Any] = field(default_factory=dict, repr=False)

    def __post_init__(self) -> None:
        if not isinstance(self.url, str) or not self.url:
            raise ValueError("schema reference URL must be a non-empty string")
        if self.format is not None:
            object.__setattr__(self, "format", SchemaFormat(self.format))
        if not isinstance(self.storage_options, Mapping):
            raise TypeError("schema reference storage_options must be a mapping")
        object.__setattr__(self, "storage_options", dict(self.storage_options))


@dataclass(frozen=True)
class SchemaProvenance:
    source: str
    format: str
    provider: str
    identifier: str | None = None
    version: str | None = None


@dataclass(frozen=True)
class ResolvedSchema:
    schema: pa.Schema
    provenance: SchemaProvenance


@dataclass(frozen=True)
class SchemaDocument:
    data: bytes
    format: SchemaFormat
    provenance: SchemaProvenance


SchemaInput: TypeAlias = pa.Schema | SchemaRef | str | Mapping[str, Any] | Sequence[Mapping[str, Any]] | None
SchemaDecoder: TypeAlias = Callable[[bytes], pa.Schema]
SchemaProvider: TypeAlias = Callable[[SchemaRef], SchemaDocument]


class SchemaResolverRegistry:
    def __init__(self) -> None:
        self._decoders: dict[SchemaFormat, SchemaDecoder] = {}
        self._providers: dict[str, SchemaProvider] = {}

    def register(self, format: SchemaFormat | str, decoder: SchemaDecoder) -> None:
        self._decoders[SchemaFormat(format)] = decoder

    def register_provider(self, protocol: str, provider: SchemaProvider) -> None:
        self._providers[protocol.lower()] = provider

    def resolve(self, reference: SchemaRef) -> ResolvedSchema:
        protocol = urlsplit(reference.url).scheme.lower()
        provider = self._providers.get(protocol, _resolve_fsspec_document)
        document = provider(reference)
        if reference.format is not None and SchemaFormat(reference.format) != document.format:
            raise ValueError(f"schema reference requested {SchemaFormat(reference.format).value!r} but provider returned {document.format.value!r}")
        try:
            decoder = self._decoders[document.format]
        except KeyError as error:
            raise ValueError(f"no schema decoder registered for {document.format.value!r}") from error
        return ResolvedSchema(
            decoder(document.data),
            replace(document.provenance, format=document.format.value),
        )


def normalize_schema(value: SchemaInput) -> pa.Schema | SchemaRef | None:
    if value is None or isinstance(value, (pa.Schema, SchemaRef)):
        return value
    if isinstance(value, str):
        return SchemaRef(value)
    if isinstance(value, Mapping) and "url" in value:
        unknown = set(value) - {"url", "format", "storage_options"}
        if unknown:
            raise ValueError(f"unknown schema reference options: {', '.join(sorted(unknown))}")
        return SchemaRef(
            url=value["url"],
            format=value.get("format"),
            storage_options=value.get("storage_options", {}),
        )
    return schema_from_options(value)


def resolve_schema(
    value: pa.Schema | SchemaRef | None,
    registry: SchemaResolverRegistry | None = None,
) -> ResolvedSchema | None:
    if value is None:
        return None
    if isinstance(value, pa.Schema):
        return ResolvedSchema(value, SchemaProvenance(source="inline", format="arrow", provider="inline"))
    return (registry or DEFAULT_SCHEMA_RESOLVERS).resolve(value)


def schema_from_options(value: Mapping[str, Any] | Sequence[Mapping[str, Any]]) -> pa.Schema:
    fields = value.get("fields") if isinstance(value, Mapping) else value
    if not isinstance(fields, Sequence) or isinstance(fields, (str, bytes)):
        raise TypeError("schema options must contain a sequence of fields")
    parsed_fields = []
    for schema_field in fields:
        if not isinstance(schema_field, Mapping):
            raise TypeError("schema fields must be mappings")
        parsed_fields.append(_field_from_options(cast(Mapping[str, Any], schema_field)))
    return pa.schema(parsed_fields)


def _field_from_options(value: Mapping[str, Any]) -> pa.Field:
    if not isinstance(value, Mapping):
        raise TypeError("schema fields must be mappings")
    try:
        name = value["name"]
        data_type = value["type"]
    except KeyError as error:
        raise ValueError(f"schema field is missing {error.args[0]!r}") from error
    if not isinstance(name, str) or not isinstance(data_type, (str, pa.DataType)):
        raise TypeError("schema field name and type must be strings or PyArrow data types")
    if isinstance(data_type, str):
        data_type = pa.type_for_alias(data_type)
    nullable = value.get("nullable", True)
    if not isinstance(nullable, bool):
        raise TypeError("schema field nullable must be a boolean")
    return pa.field(name, data_type, nullable=nullable)


def _format_from_url(url: str) -> SchemaFormat:
    path = url.split("?", 1)[0].lower()
    if path.endswith((".arrow", ".ipc", ".arrowschema")):
        return SchemaFormat.ARROW_IPC
    raise ValueError(f"schema format cannot be inferred from {url!r}; provide format explicitly")


def _decode_arrow_json(data: bytes) -> pa.Schema:
    value = json.loads(data)
    if not isinstance(value, Mapping):
        raise TypeError("Arrow JSON schema must contain an object")
    return schema_from_options(value)


def _decode_arrow_ipc(data: bytes) -> pa.Schema:
    return pa.ipc.read_schema(pa.BufferReader(data))


def _decode_json_schema(data: bytes) -> pa.Schema:
    value = json.loads(data)
    if not isinstance(value, Mapping):
        raise TypeError("JSON Schema must contain an object")
    return json_schema_to_arrow(value)


def _resolve_fsspec_document(reference: SchemaRef) -> SchemaDocument:
    format = SchemaFormat(reference.format) if reference.format is not None else _format_from_url(reference.url)
    with fsspec.open(reference.url, "rb", **reference.storage_options) as file:
        data = file.read()
    return SchemaDocument(
        data=data,
        format=format,
        provenance=SchemaProvenance(source=reference.url, format=format.value, provider="fsspec"),
    )


def _resolve_confluent_document(reference: SchemaRef) -> SchemaDocument:
    document = fetch_confluent_schema(reference.url, reference.storage_options)
    if document.references:
        raise ValueError("Confluent schema references are not supported yet")
    if document.schema_type != "JSON":
        raise ValueError(f"Confluent schema type {document.schema_type!r} is not supported")
    return SchemaDocument(
        data=document.schema.encode(),
        format=SchemaFormat.JSON_SCHEMA,
        provenance=SchemaProvenance(
            source=reference.url,
            format=SchemaFormat.JSON_SCHEMA.value,
            provider="confluent",
            identifier=document.schema_id,
            version=document.version,
        ),
    )


DEFAULT_SCHEMA_RESOLVERS = SchemaResolverRegistry()
DEFAULT_SCHEMA_RESOLVERS.register(SchemaFormat.ARROW_JSON, _decode_arrow_json)
DEFAULT_SCHEMA_RESOLVERS.register(SchemaFormat.ARROW_IPC, _decode_arrow_ipc)
DEFAULT_SCHEMA_RESOLVERS.register(SchemaFormat.JSON_SCHEMA, _decode_json_schema)
DEFAULT_SCHEMA_RESOLVERS.register_provider("confluent", _resolve_confluent_document)
