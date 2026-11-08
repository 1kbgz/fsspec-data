use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use arrow::datatypes::{DataType, Field, Schema};
use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

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

/// A JSON value whose objects keep their key order, so fields follow the order of
/// `properties` without enabling `preserve_order` on `serde_json` for every dependent crate.
#[derive(Debug)]
enum Json {
    Null,
    Bool,
    Number,
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

type Object = [(String, Json)];

impl Json {
    fn as_object(&self) -> Option<&Object> {
        match self {
            Self::Object(entries) => Some(entries),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }
}

/// Top-level keys of JSON records in order of first appearance, reading at most
/// `max_records` records.
pub(crate) fn record_keys(records: &[u8], max_records: Option<usize>) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let stream = serde_json::Deserializer::from_slice(records).into_iter::<Json>();
    for record in stream.take(max_records.unwrap_or(usize::MAX)).flatten() {
        if let Json::Object(entries) = record {
            for (key, _) in entries {
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
    }
    keys
}

fn get<'a>(object: &'a Object, key: &str) -> Option<&'a Json> {
    object
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct JsonVisitor;

        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = Json;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON value")
            }
            fn visit_unit<E>(self) -> std::result::Result<Json, E> {
                Ok(Json::Null)
            }
            fn visit_bool<E>(self, _: bool) -> std::result::Result<Json, E> {
                Ok(Json::Bool)
            }
            fn visit_i64<E>(self, _: i64) -> std::result::Result<Json, E> {
                Ok(Json::Number)
            }
            fn visit_u64<E>(self, _: u64) -> std::result::Result<Json, E> {
                Ok(Json::Number)
            }
            fn visit_f64<E>(self, _: f64) -> std::result::Result<Json, E> {
                Ok(Json::Number)
            }
            fn visit_str<E>(self, value: &str) -> std::result::Result<Json, E> {
                Ok(Json::String(value.to_string()))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Json, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element()? {
                    values.push(value);
                }
                Ok(Json::Array(values))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Json, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(Json::Object(entries))
            }
        }

        deserializer.deserialize_any(JsonVisitor)
    }
}

/// Maps the documented JSON Schema subset to Arrow: primitive types, arrays with one
/// `items` schema, objects with `properties` and `required`, and unions of one type with
/// `null`. Fields follow the order of `properties`. Unsupported or ambiguous constructs are
/// errors that name their location.
pub fn json_schema_to_arrow(document: &str) -> Result<Schema> {
    let value: Json = serde_json::from_str(document)?;
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

fn data_type(value: &Object, path: &str) -> Result<(DataType, bool)> {
    reject_unsupported(value, path)?;
    if let Some(format) = get(value, "format") {
        let format = match format {
            Json::String(format) => format!("{format:?}"),
            other => format!("{other:?}"),
        };
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
        let items = get(value, "items")
            .and_then(Json::as_object)
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

fn object_fields(value: &Object, path: &str) -> Result<Vec<Field>> {
    reject_unsupported(value, path)?;
    if get(value, "additionalProperties")
        .is_some_and(|additional| !matches!(additional, Json::Bool | Json::Null))
    {
        return Err(error(format!(
            "unsupported JSON Schema at {path}: schema-valued 'additionalProperties'"
        )));
    }
    let properties: &Object = match get(value, "properties") {
        None => &[],
        Some(Json::Object(properties)) => properties,
        Some(_) => {
            return Err(error(format!(
                "JSON Schema at {path} has non-object 'properties'"
            )))
        }
    };
    let required: HashSet<&str> = match get(value, "required") {
        None => HashSet::new(),
        Some(Json::Array(names)) => names
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
        .filter(|name| get(properties, name).is_none())
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

fn schema_kind<'a>(value: &'a Object, path: &str) -> Result<(&'a str, bool)> {
    reject_unsupported(value, path)?;
    let types: Vec<&str> = match get(value, "type") {
        Some(Json::String(kind)) => vec![kind.as_str()],
        Some(Json::Array(kinds)) if kinds.iter().all(|kind| kind.as_str().is_some()) => {
            kinds.iter().filter_map(Json::as_str).collect()
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

fn reject_unsupported(value: &Object, path: &str) -> Result<()> {
    match UNSUPPORTED_KEYWORDS
        .iter()
        .find(|keyword| get(value, keyword).is_some())
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
        let schema = json_schema_to_arrow(
            r#"{
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
                        "properties": {"name": {"type": "string"}, "active": {"type": "boolean"}}
                    },
                    "nothing": {"type": "null"}
                }
            }"#,
        )
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
    fn ignores_validation_keywords() {
        let schema = json_schema_to_arrow(
            r#"{"type": "object", "enum": [1, -1, 2.5, null, true], "properties": {}}"#,
        )
        .unwrap();
        assert!(schema.fields().is_empty());
        assert!(json_schema_to_arrow("{").is_err());
    }

    #[test]
    fn rejects_unsupported_and_ambiguous_constructs() {
        let cases = [
            (json!([]), "must contain an object"),
            (
                json!({"type": "object", "properties": {"a": {"type": "string", "format": 1}}}),
                "format Number",
            ),
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
            let error = json_schema_to_arrow(&value.to_string())
                .unwrap_err()
                .to_string();
            assert!(error.contains(message), "{error}");
        }
    }
}
