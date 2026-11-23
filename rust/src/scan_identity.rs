//! Equality keys for scan requests. These types do not cache or share reads.
use std::fmt;

use serde_json::Value;
use uuid::Uuid;

use crate::{InterchangeError, Result, ScanSelection, StreamOptions};

/// Opaque scope for one source configuration and credential context.
///
/// Retain or clone this token for requests in the same context. Create a new token when
/// configuration, credentials, or effective authorization changes. Never derive it from
/// a URL or raw credentials. Tokens are process-local and are not serializable.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct SourceIdentity(Uuid);

impl SourceIdentity {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SourceIdentity {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for SourceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SourceIdentity { .. }")
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
enum Snapshot {
    Pinned(String),
    Unpinned(Uuid),
}

/// Immutable identity of a logical scan, scoped to a source and pinned snapshot.
///
/// JSON object keys are canonicalized recursively; array order and JSON types remain
/// significant. Filters and backend options are compared, not interpreted or executed.
/// Columns preserve order and duplicates; selecting all differs from selecting none.
/// Debug output omits request contents, which may contain sensitive predicates or paths.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct ScanIdentity {
    source: SourceIdentity,
    path: String,
    snapshot: Snapshot,
    selection: ScanSelection,
    options: StreamOptions,
    filter: String,
    start_position: String,
    backend_options: String,
}

fn canonical(mut value: Value) -> String {
    value.sort_all_objects();
    value.to_string()
}

impl ScanIdentity {
    /// Construct an identity from the resolved request before execution.
    ///
    /// `snapshot` must describe the version actually pinned by the provider. For manifests,
    /// include namespace ID, table ID and generation; for objects use a checked version ID
    /// or ETag. Null means unpinned and gives each construction a distinct identity.
    /// Paths are compared exactly, without filesystem-dependent normalization.
    ///
    /// Include any additional result-affecting settings (schema, codec options, etc.) in
    /// `backend_options`. Cancellation, deadlines and execution buffer limits belong to
    /// individual consumers and are not part of logical identity. Equal identities do not
    /// authorize sharing their execution state or bypassing authorization/version checks.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source: SourceIdentity,
        path: String,
        snapshot: Value,
        selection: ScanSelection,
        options: StreamOptions,
        filter: Value,
        start_position: Value,
        backend_options: Value,
    ) -> Result<Self> {
        if options.batch_size == 0 {
            return Err(InterchangeError::InvalidBatchSize);
        }
        Ok(Self {
            source,
            path,
            snapshot: if snapshot.is_null() {
                Snapshot::Unpinned(Uuid::new_v4())
            } else {
                Snapshot::Pinned(canonical(snapshot))
            },
            selection,
            options,
            filter: canonical(filter),
            start_position: canonical(start_position),
            backend_options: canonical(backend_options),
        })
    }
}

impl fmt::Debug for ScanIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ScanIdentity { .. }")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;

    fn identity(source: &SourceIdentity) -> ScanIdentity {
        ScanIdentity::new(
            source.clone(),
            "orders".into(),
            json!({"namespace_id": "n", "table_id": "t", "generation": 2}),
            ScanSelection {
                columns: Some(vec!["b".into(), "a".into(), "b".into()]),
                row_offset: 3,
            },
            StreamOptions {
                batch_size: 16,
                row_limit: Some(8),
                byte_limit: Some(4096),
            },
            json!({"op": "gt", "column": "a", "value": 1}),
            json!({"partition": 0, "offset": 10}),
            json!({"codec": "parquet", "schema": {"b": "int64", "a": "int64"}}),
        )
        .unwrap()
    }

    #[test]
    fn credential_scopes_and_clones() {
        let source = SourceIdentity::new();
        let key = identity(&source);
        let keys = HashSet::from([key.clone()]);
        assert!(keys.contains(&identity(&source.clone())));
        assert!(!keys.contains(&identity(&SourceIdentity::new())));
    }

    #[test]
    fn request_fields_are_independent() {
        let original = identity(&SourceIdentity::new());
        let mut variants = vec![];
        macro_rules! changed {
            ($field:ident, $value:expr) => {{
                let mut key = original.clone();
                key.$field = $value;
                variants.push(key);
            }};
        }
        changed!(path, "other/orders".into());
        changed!(
            snapshot,
            Snapshot::Pinned(canonical(
                json!({"namespace_id":"n", "table_id":"t", "generation":3})
            ))
        );
        changed!(
            snapshot,
            Snapshot::Pinned(canonical(
                json!({"namespace_id":"n", "table_id":"replacement", "generation":2})
            ))
        );
        changed!(
            selection,
            ScanSelection {
                columns: None,
                row_offset: 3
            }
        );
        changed!(
            selection,
            ScanSelection {
                columns: Some(vec![]),
                row_offset: 3
            }
        );
        changed!(
            selection,
            ScanSelection {
                columns: Some(vec!["a".into(), "b".into(), "b".into()]),
                row_offset: 3
            }
        );
        changed!(
            selection,
            ScanSelection {
                row_offset: 4,
                ..original.selection.clone()
            }
        );
        changed!(
            options,
            StreamOptions {
                batch_size: 32,
                ..original.options
            }
        );
        changed!(
            options,
            StreamOptions {
                row_limit: None,
                ..original.options
            }
        );
        changed!(
            options,
            StreamOptions {
                byte_limit: None,
                ..original.options
            }
        );
        changed!(
            filter,
            canonical(json!({"op": "gt", "column": "a", "value": 2}))
        );
        changed!(
            start_position,
            canonical(json!({"partition": 1, "offset": 10}))
        );
        changed!(backend_options, canonical(json!({"codec": "csv"})));
        let count = variants.len();
        let keys: HashSet<_> = variants.into_iter().collect();
        assert_eq!(keys.len(), count);
        assert!(!keys.contains(&original));
    }

    #[test]
    fn canonical_json_preserves_order_and_types() {
        let left: Value = serde_json::from_str(r#"{"z":[{"b":1,"a":2}],"a":false}"#).unwrap();
        let right: Value = serde_json::from_str(r#"{"a":false,"z":[{"a":2,"b":1}]}"#).unwrap();
        assert_eq!(canonical(left), canonical(right));
        let values = [
            json!(null),
            json!(false),
            json!(0),
            json!(0.0),
            json!("0"),
            json!([]),
            json!({}),
            json!([1, 2]),
            json!([2, 1]),
        ];
        assert_eq!(
            values
                .iter()
                .cloned()
                .map(canonical)
                .collect::<HashSet<_>>()
                .len(),
            values.len()
        );
    }

    #[test]
    fn unpinned_requests_never_reuse_an_identity() {
        let source = SourceIdentity::new();
        let make = || {
            ScanIdentity::new(
                source.clone(),
                "mutable".into(),
                Value::Null,
                ScanSelection::default(),
                StreamOptions::default(),
                Value::Null,
                Value::Null,
                Value::Null,
            )
            .unwrap()
        };
        let key = make();
        assert_eq!(key, key.clone());
        assert_ne!(key, make());
    }

    #[test]
    fn debug_does_not_expose_request_contents() {
        let source = SourceIdentity::new();
        assert_eq!(format!("{source:?}"), "SourceIdentity { .. }");
        assert_eq!(format!("{:?}", identity(&source)), "ScanIdentity { .. }");
    }
}
