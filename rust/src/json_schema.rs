use std::collections::HashSet;
use std::sync::Arc;

use arrow::datatypes::{DataType, Field, Schema};
use serde_json::{Map, Value};

use crate::{InterchangeError, Result};

const UNSUPPORTED_KEYWORDS: [&str; 18] = [
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
];

/// Maps the documented JSON Schema subset to Arrow: primitive types, arrays with one
/// `items` schema, objects with `properties` and `required`, and unions of one type with
/// `null`. Unsupported or ambiguous constructs are errors that name their location.
pub fn json_schema_to_arrow(value: &Value) -> Result<Schema> {
    let value = value
        .as_object()
        .ok_or_else(|| error("JSON Schema must contain an object"))?;
    let (kind, allows_null) = schema_kind(value, "$")?;
    if kind != "object" {
        return Err(error(format!(
            "JSON Schema root must have type 'object', got {kind:?}"
        )));
    }
    if allows_null {
        return Err(error("JSON Schema root cannot allow null"));
    }
    Ok(Schema::new(object_fields(value, "$")?))
}

fn data_type(value: &Map<String, Value>, path: &str) -> Result<(DataType, bool)> {
    reject_unsupported(value, path)?;
    if let Some(format) = value.get("format") {
        return Err(error(format!(
            "unsupported JSON Schema at {path}: format {format}"
        )));
    }
    let (kind, allows_null) = schema_kind(value, path)?;
    let primitive = match kind {
        "boolean" => Some(DataType::Boolean),
        "integer" => Some(DataType::Int64),
        "null" => Some(DataType::Null),
        "number" => Some(DataType::Float64),
        "string" => Some(DataType::Utf8),
        _ => None,
    };
    if let Some(primitive) = primitive {
        return Ok((primitive, allows_null || kind == "null"));
    }
    if kind == "array" {
        let items = value
            .get("items")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                error(format!(
                "unsupported JSON Schema at {path}: arrays require one object-valued 'items' schema"
            ))
            })?;
        let (item_type, item_nullable) = data_type(items, &format!("{path}.items"))?;
        return Ok((
            DataType::List(Arc::new(Field::new("item", item_type, item_nullable))),
            allows_null,
        ));
    }
    Ok((
        DataType::Struct(object_fields(value, path)?.into()),
        allows_null,
    ))
}

fn object_fields(value: &Map<String, Value>, path: &str) -> Result<Vec<Field>> {
    reject_unsupported(value, path)?;
    if value
        .get("additionalProperties")
        .is_some_and(|additional| !additional.is_boolean() && !additional.is_null())
    {
        return Err(error(format!(
            "unsupported JSON Schema at {path}: schema-valued 'additionalProperties'"
        )));
    }
    let empty = Map::new();
    let properties = match value.get("properties") {
        None => &empty,
        Some(Value::Object(properties)) => properties,
        Some(_) => {
            return Err(error(format!(
                "JSON Schema at {path} has non-object 'properties'"
            )))
        }
    };
    let required: HashSet<&str> = match value.get("required") {
        None => HashSet::new(),
        Some(Value::Array(names)) => names
            .iter()
            .map(|name| {
                name.as_str().ok_or_else(|| {
                    error(format!(
                        "JSON Schema at {path} has non-string 'required' entries"
                    ))
                })
            })
            .collect::<Result<_>>()?,
        Some(_) => {
            return Err(error(format!(
                "JSON Schema at {path} has non-string 'required' entries"
            )))
        }
    };
    let mut unknown: Vec<&str> = required
        .iter()
        .copied()
        .filter(|name| !properties.contains_key(*name))
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        return Err(error(format!(
            "JSON Schema at {path} requires unknown properties: {}",
            unknown.join(", ")
        )));
    }
    properties
        .iter()
        .map(|(name, property)| {
            let property = property.as_object().ok_or_else(|| {
                error(format!(
                    "JSON Schema at {path} must map string property names to schema objects"
                ))
            })?;
            let (data_type, allows_null) =
                data_type(property, &format!("{path}.properties.{name}"))?;
            Ok(Field::new(
                name,
                data_type,
                !required.contains(name.as_str()) || allows_null,
            ))
        })
        .collect()
}

fn schema_kind<'a>(value: &'a Map<String, Value>, path: &str) -> Result<(&'a str, bool)> {
    reject_unsupported(value, path)?;
    let types: Vec<&str> = match value.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) if kinds.iter().all(Value::is_string) => {
            kinds.iter().filter_map(Value::as_str).collect()
        }
        _ => {
            return Err(error(format!(
                "JSON Schema at {path} requires a string or string-array 'type'"
            )))
        }
    };
    let mut distinct = Vec::new();
    for kind in types {
        if !distinct.contains(&kind) {
            distinct.push(kind);
        }
    }
    let known = [
        "boolean", "integer", "null", "number", "string", "array", "object",
    ];
    if let Some(unknown) = distinct.iter().filter(|kind| !known.contains(kind)).min() {
        return Err(error(format!(
            "unsupported JSON Schema at {path}: type {unknown:?}"
        )));
    }
    let allows_null = distinct.contains(&"null");
    let non_null: Vec<&str> = distinct
        .into_iter()
        .filter(|kind| *kind != "null")
        .collect();
    if non_null.len() > 1 {
        return Err(error(format!(
            "unsupported JSON Schema at {path}: unions may contain only one non-null type"
        )));
    }
    Ok((non_null.first().copied().unwrap_or("null"), allows_null))
}

fn reject_unsupported(value: &Map<String, Value>, path: &str) -> Result<()> {
    match UNSUPPORTED_KEYWORDS
        .iter()
        .find(|keyword| value.contains_key(**keyword))
    {
        Some(keyword) => Err(error(format!(
            "unsupported JSON Schema at {path}: keyword {keyword:?}"
        ))),
        None => Ok(()),
    }
}

fn error(message: impl Into<String>) -> InterchangeError {
    InterchangeError::JsonSchema(message.into())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn maps_structural_types_and_nullability() {
        let schema = json_schema_to_arrow(&json!({
            "type": "object",
            "required": ["id", "tags", "owner"],
            "additionalProperties": false,
            "properties": {
                "id": {"type": "integer"},
                "score": {"type": ["number", "null"]},
                "tags": {"type": "array", "items": {"type": "string"}},
                "owner": {
                    "type": ["object", "null"],
                    "required": ["name"],
                    "properties": {"name": {"type": "string"}, "active": {"type": "boolean"}},
                },
                "nothing": {"type": "null"},
            },
        }))
        .unwrap();

        let owner = DataType::Struct(
            vec![
                Field::new("name", DataType::Utf8, false),
                Field::new("active", DataType::Boolean, true),
            ]
            .into(),
        );
        assert_eq!(
            schema,
            Schema::new(vec![
                Field::new("id", DataType::Int64, false),
                Field::new("score", DataType::Float64, true),
                Field::new(
                    "tags",
                    DataType::List(Arc::new(Field::new("item", DataType::Utf8, false))),
                    false,
                ),
                Field::new("owner", owner, true),
                Field::new("nothing", DataType::Null, true),
            ])
        );
    }

    #[test]
    fn rejects_unsupported_and_ambiguous_constructs() {
        let cases = [
            (json!([]), "must contain an object"),
            (json!({"type": "string"}), "root must have type 'object'"),
            (
                json!({"type": ["object", "null"]}),
                "root cannot allow null",
            ),
            (json!({"type": "object", "oneOf": []}), "keyword \"oneOf\""),
            (json!({"type": 3}), "string or string-array 'type'"),
            (
                json!({"type": "object", "properties": {"a": {"type": "date"}}}),
                "type \"date\"",
            ),
            (
                json!({"type": "object", "properties": {"a": {"type": ["string", "integer"]}}}),
                "only one non-null type",
            ),
            (
                json!({"type": "object", "properties": {"a": {"type": "string", "format": "date"}}}),
                "$.properties.a: format \"date\"",
            ),
            (
                json!({"type": "object", "properties": {"a": {"type": "array"}}}),
                "object-valued 'items'",
            ),
            (
                json!({"type": "object", "additionalProperties": {"type": "string"}}),
                "schema-valued 'additionalProperties'",
            ),
            (
                json!({"type": "object", "properties": []}),
                "non-object 'properties'",
            ),
            (
                json!({"type": "object", "required": "a"}),
                "non-string 'required'",
            ),
            (
                json!({"type": "object", "required": [1]}),
                "non-string 'required'",
            ),
            (
                json!({"type": "object", "required": ["b", "a"]}),
                "unknown properties: a, b",
            ),
            (
                json!({"type": "object", "properties": {"a": 1}}),
                "to schema objects",
            ),
        ];
        for (value, message) in cases {
            let error = json_schema_to_arrow(&value).unwrap_err().to_string();
            assert!(error.contains(message), "{error}");
        }
    }
}
