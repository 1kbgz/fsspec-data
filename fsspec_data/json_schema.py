from __future__ import annotations

from collections.abc import Mapping, Sequence
from typing import Any, cast

import pyarrow as pa

_UNSUPPORTED_KEYWORDS = (
    "$dynamicRef",
    "$ref",
    "allOf",
    "anyOf",
    "contains",
    "contentEncoding",
    "contentMediaType",
    "dependentSchemas",
    "else",
    "if",
    "not",
    "nullable",
    "oneOf",
    "patternProperties",
    "prefixItems",
    "then",
    "unevaluatedItems",
    "unevaluatedProperties",
)
_PRIMITIVE_TYPES = {
    "boolean": pa.bool_(),
    "integer": pa.int64(),
    "null": pa.null(),
    "number": pa.float64(),
    "string": pa.string(),
}


def json_schema_to_arrow(value: Mapping[str, Any]) -> pa.Schema:
    if not isinstance(value, Mapping):
        raise TypeError("JSON Schema must contain an object")
    kind, allows_null = _schema_kind(value, "$")
    if kind != "object":
        raise ValueError(f"JSON Schema root must have type 'object', got {kind!r}")
    if allows_null:
        raise ValueError("JSON Schema root cannot allow null")
    return pa.schema(_object_fields(value, "$"))


def _data_type(value: Mapping[str, Any], path: str) -> tuple[pa.DataType, bool]:
    _reject_unsupported(value, path)
    if "format" in value:
        raise ValueError(f"unsupported JSON Schema at {path}: format {value['format']!r}")
    kind, allows_null = _schema_kind(value, path)
    if kind in _PRIMITIVE_TYPES:
        return _PRIMITIVE_TYPES[kind], allows_null or kind == "null"
    if kind == "array":
        items = value.get("items")
        if not isinstance(items, Mapping):
            raise ValueError(f"unsupported JSON Schema at {path}: arrays require one object-valued 'items' schema")
        item_type, item_nullable = _data_type(cast(Mapping[str, Any], items), f"{path}.items")
        return pa.list_(pa.field("item", item_type, nullable=item_nullable)), allows_null
    if kind == "object":
        return pa.struct(_object_fields(value, path)), allows_null
    raise ValueError(f"unsupported JSON Schema at {path}: type {kind!r}")


def _object_fields(value: Mapping[str, Any], path: str) -> list[pa.Field]:
    _reject_unsupported(value, path)
    additional = value.get("additionalProperties")
    if additional is not None and not isinstance(additional, bool):
        raise ValueError(f"unsupported JSON Schema at {path}: schema-valued 'additionalProperties'")

    properties = value.get("properties", {})
    if not isinstance(properties, Mapping):
        raise TypeError(f"JSON Schema at {path} has non-object 'properties'")
    required = value.get("required", [])
    if not isinstance(required, Sequence) or isinstance(required, (str, bytes)) or not all(isinstance(name, str) for name in required):
        raise TypeError(f"JSON Schema at {path} has non-string 'required' entries")
    required_names = set(required)
    unknown_required = required_names - set(properties)
    if unknown_required:
        names = ", ".join(sorted(unknown_required))
        raise ValueError(f"JSON Schema at {path} requires unknown properties: {names}")

    fields = []
    for name, property_schema in properties.items():
        if not isinstance(name, str) or not isinstance(property_schema, Mapping):
            raise TypeError(f"JSON Schema at {path} must map string property names to schema objects")
        data_type, allows_null = _data_type(cast(Mapping[str, Any], property_schema), f"{path}.properties.{name}")
        fields.append(pa.field(name, data_type, nullable=name not in required_names or allows_null))
    return fields


def _schema_kind(value: Mapping[str, Any], path: str) -> tuple[str, bool]:
    _reject_unsupported(value, path)
    raw_type = value.get("type")
    if isinstance(raw_type, str):
        types = [raw_type]
    elif isinstance(raw_type, Sequence) and not isinstance(raw_type, (str, bytes)) and all(isinstance(item, str) for item in raw_type):
        types = [cast(str, item) for item in raw_type]
    else:
        raise TypeError(f"JSON Schema at {path} requires a string or string-array 'type'")

    distinct = list(dict.fromkeys(types))
    unknown = set(distinct) - {*_PRIMITIVE_TYPES, "array", "object"}
    if unknown:
        raise ValueError(f"unsupported JSON Schema at {path}: type {min(unknown)!r}")
    allows_null = "null" in distinct
    non_null = [item for item in distinct if item != "null"]
    if len(non_null) > 1:
        raise ValueError(f"unsupported JSON Schema at {path}: unions may contain only one non-null type")
    return (non_null[0] if non_null else "null"), allows_null


def _reject_unsupported(value: Mapping[str, Any], path: str) -> None:
    for keyword in _UNSUPPORTED_KEYWORDS:
        if keyword in value:
            raise ValueError(f"unsupported JSON Schema at {path}: keyword {keyword!r}")
