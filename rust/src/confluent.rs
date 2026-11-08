use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;

use crate::schema::schema_error;
use crate::{
    Result, SchemaDocument, SchemaFormat, SchemaProvenance, SchemaProvider, SchemaReference,
};

/// One registered schema version of a Confluent Schema Registry subject.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfluentSchemaDocument {
    pub schema: String,
    pub schema_type: String,
    pub subject: String,
    pub version: String,
    pub schema_id: Option<String>,
    pub references: Vec<Value>,
}

/// Resolves `confluent://SUBJECT/versions/VERSION` references.
///
/// Options: `registry_url` (required), `username` and `password` together for basic
/// authentication, and `timeout` in seconds.
pub struct ConfluentProvider;

impl SchemaProvider for ConfluentProvider {
    fn fetch(&self, reference: &SchemaReference) -> Result<SchemaDocument> {
        let document = fetch_confluent_schema(&reference.url, &reference.options)?;
        if !document.references.is_empty() {
            return Err(schema_error(
                "Confluent schema references are not supported yet",
            ));
        }
        let format = match document.schema_type.as_str() {
            "JSON" => SchemaFormat::JsonSchema,
            "AVRO" => SchemaFormat::Avro,
            other => {
                return Err(schema_error(format!(
                    "Confluent schema type {other:?} is not supported"
                )))
            }
        };
        Ok(SchemaDocument {
            data: document.schema.into_bytes(),
            format,
            provenance: SchemaProvenance {
                source: reference.url.clone(),
                format: format.as_str().to_string(),
                provider: "confluent".to_string(),
                identifier: document.schema_id,
                version: Some(document.version),
            },
        })
    }
}

pub fn fetch_confluent_schema(
    url: &str,
    options: &HashMap<String, String>,
) -> Result<ConfluentSchemaDocument> {
    let (subject, version) = parse_reference(url)?;
    let payload = get(
        &format!(
            "subjects/{}/versions/{}",
            encode(&subject),
            encode(&version)
        ),
        options,
    )?;
    document(payload, &subject, &version)
}

/// Every registered version of the subject in `url`, oldest first. The version in `url`
/// is ignored.
pub fn fetch_confluent_versions(
    url: &str,
    options: &HashMap<String, String>,
) -> Result<Vec<ConfluentSchemaDocument>> {
    let (subject, _) = parse_reference(url)?;
    let versions = get(&format!("subjects/{}/versions", encode(&subject)), options)?;
    let versions: Vec<i64> = versions
        .as_array()
        .and_then(|versions| versions.iter().map(Value::as_i64).collect())
        .ok_or_else(|| {
            schema_error("Confluent Schema Registry versions response must be a list of integers")
        })?;
    versions
        .into_iter()
        .map(|version| {
            let payload = get(
                &format!("subjects/{}/versions/{version}", encode(&subject)),
                options,
            )?;
            document(payload, &subject, &version.to_string())
        })
        .collect()
}

fn get(path: &str, options: &HashMap<String, String>) -> Result<Value> {
    let unknown: Vec<&str> = {
        let mut unknown: Vec<&str> = options
            .keys()
            .map(String::as_str)
            .filter(|key| !["registry_url", "username", "password", "timeout"].contains(key))
            .collect();
        unknown.sort_unstable();
        unknown
    };
    if !unknown.is_empty() {
        return Err(schema_error(format!(
            "unknown Confluent options: {}",
            unknown.join(", ")
        )));
    }
    let registry_url = options.get("registry_url").ok_or_else(|| {
        schema_error("Confluent schema references require the 'registry_url' option")
    })?;
    if !registry_url.starts_with("http://") && !registry_url.starts_with("https://") {
        return Err(schema_error(
            "Confluent registry_url must be an HTTP or HTTPS URL",
        ));
    }
    let timeout = options
        .get("timeout")
        .map(|timeout| {
            timeout
                .parse::<f64>()
                .ok()
                .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
                .ok_or_else(|| schema_error("Confluent timeout must be a number of seconds"))
        })
        .transpose()?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(timeout)
        .http_status_as_error(false)
        .build()
        .into();
    let mut request = agent
        .get(format!("{}/{path}", registry_url.trim_end_matches('/')))
        .header(
            "Accept",
            "application/vnd.schemaregistry.v1+json, application/json",
        );
    match (options.get("username"), options.get("password")) {
        (Some(username), Some(password)) => {
            request = request.header(
                "Authorization",
                format!("Basic {}", base64(&format!("{username}:{password}"))),
            );
        }
        (None, None) => {}
        _ => {
            return Err(schema_error(
                "Confluent username and password must be provided together",
            ))
        }
    }
    let mut response = request
        .call()
        .map_err(|_| schema_error("Confluent Schema Registry request failed"))?;
    let status = response.status().as_u16();
    if status >= 400 {
        return Err(schema_error(format!(
            "Confluent Schema Registry returned HTTP {status}"
        )));
    }
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|_| schema_error("Confluent Schema Registry request failed"))?;
    Ok(serde_json::from_str(&body)?)
}

fn document(payload: Value, subject: &str, version: &str) -> Result<ConfluentSchemaDocument> {
    let invalid =
        |message: &str| schema_error(format!("Confluent Schema Registry response {message}"));
    let payload = payload
        .as_object()
        .ok_or_else(|| invalid("must be an object"))?;
    let schema = payload
        .get("schema")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("is missing string 'schema'"))?;
    let schema_type = match payload.get("schemaType") {
        None => "AVRO",
        Some(Value::String(schema_type)) => schema_type,
        Some(_) => return Err(invalid("has invalid 'schemaType'")),
    };
    let scalar = |value: Option<&Value>, fallback: &str| match value {
        None => Some(fallback.to_string()),
        Some(Value::String(value)) => Some(value.clone()),
        Some(Value::Number(value)) if value.is_i64() => Some(value.to_string()),
        Some(_) => None,
    };
    let response_subject = match payload.get("subject") {
        None => subject.to_string(),
        Some(Value::String(subject)) => subject.clone(),
        Some(_) => return Err(invalid("has invalid subject or version")),
    };
    let response_version = scalar(payload.get("version"), version)
        .ok_or_else(|| invalid("has invalid subject or version"))?;
    let schema_id = match payload.get("id") {
        None | Some(Value::Null) => None,
        id => Some(scalar(id, "").ok_or_else(|| invalid("has invalid schema ID"))?),
    };
    let references = match payload.get("references") {
        None => Vec::new(),
        Some(Value::Array(references)) if references.iter().all(Value::is_object) => {
            references.clone()
        }
        Some(_) => return Err(invalid("has invalid references")),
    };
    Ok(ConfluentSchemaDocument {
        schema: schema.to_string(),
        schema_type: schema_type.to_uppercase(),
        subject: response_subject,
        version: response_version,
        schema_id,
        references,
    })
}

fn parse_reference(url: &str) -> Result<(String, String)> {
    let invalid =
        || schema_error("Confluent schema URL must be confluent://SUBJECT/versions/VERSION");
    let rest = url.strip_prefix("confluent://").ok_or_else(invalid)?;
    if rest.contains(['?', '#']) {
        return Err(invalid());
    }
    let (subject, path) = rest.split_once('/').ok_or_else(invalid)?;
    let parts: Vec<String> = path
        .split('/')
        .filter(|part| !part.is_empty())
        .map(decode)
        .collect();
    let [versions, version] = parts.as_slice() else {
        return Err(invalid());
    };
    if subject.is_empty() || versions != "versions" {
        return Err(invalid());
    }
    if version != "latest"
        && !version
            .parse::<i64>()
            .is_ok_and(|number| number == -1 || (1..=i64::from(i32::MAX)).contains(&number))
    {
        return Err(schema_error(
            "Confluent schema VERSION must be 'latest', -1, or a positive integer",
        ));
    }
    Ok((decode(subject), version.clone()))
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes
            .get(index + 1..index + 3)
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (bytes[index], hex) {
            (b'%', Some(byte)) => {
                output.push(byte);
                index += 3;
            }
            (byte, _) => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn base64(value: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    for chunk in value.as_bytes().chunks(3) {
        let bits = chunk.iter().enumerate().fold(0u32, |bits, (index, byte)| {
            bits | u32::from(*byte) << (16 - 8 * index)
        });
        for index in 0..4 {
            if index <= chunk.len() {
                output.push(ALPHABET[(bits >> (18 - 6 * index) & 63) as usize] as char);
            } else {
                output.push('=');
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    use arrow::datatypes::DataType;

    use super::*;
    use crate::DEFAULT_SCHEMA_RESOLVERS;

    /// Serves one canned response per request, in order, and sends back each request's
    /// path and authorization header.
    fn registry(
        responses: Vec<(u16, String)>,
    ) -> (String, mpsc::Receiver<(String, Option<String>)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for (status, body) in responses {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap().to_string();
                let mut authorization = None;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header.trim().is_empty() {
                        break;
                    }
                    if let Some(value) = header.strip_prefix("authorization: ") {
                        authorization = Some(value.trim().to_string());
                    }
                }
                let _ = sender.send((path, authorization));
                let mut stream = stream;
                write!(
                    stream,
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (url, receiver)
    }

    fn options(url: &str) -> HashMap<String, String> {
        HashMap::from([("registry_url".to_string(), url.to_string())])
    }

    #[test]
    fn resolves_registered_avro_and_json_schemas() {
        let avro = serde_json::json!({
            "subject": "orders-value", "version": 3, "id": 42,
            "schema": r#"{"type": "record", "name": "Order", "fields": [{"name": "id", "type": "long"}]}"#,
        });
        let json = serde_json::json!({
            "schemaType": "JSON", "id": "7",
            "schema": r#"{"type": "object", "properties": {"id": {"type": "integer"}}}"#,
        });
        let (url, requests) = registry(vec![(200, avro.to_string()), (200, json.to_string())]);
        let reference = SchemaReference::new("confluent://orders-value/versions/latest")
            .with_option("registry_url", &url)
            .with_option("username", "user")
            .with_option("password", "pass")
            .with_option("timeout", "5");

        let resolved = DEFAULT_SCHEMA_RESOLVERS.resolve(&reference).unwrap();
        assert_eq!(resolved.schema.field(0).data_type(), &DataType::Int64);
        assert_eq!(
            resolved.provenance,
            SchemaProvenance {
                source: "confluent://orders-value/versions/latest".to_string(),
                format: "avro-schema".to_string(),
                provider: "confluent".to_string(),
                identifier: Some("42".to_string()),
                version: Some("3".to_string()),
            }
        );
        assert_eq!(
            requests.recv().unwrap(),
            (
                "/subjects/orders-value/versions/latest".to_string(),
                Some("Basic dXNlcjpwYXNz".to_string())
            )
        );

        let reference = SchemaReference::new("confluent://a%2Fb/versions/2")
            .with_option("registry_url", format!("{url}/"));
        let resolved = DEFAULT_SCHEMA_RESOLVERS.resolve(&reference).unwrap();
        assert_eq!(resolved.provenance.format, "json-schema");
        assert_eq!(resolved.provenance.version.as_deref(), Some("2"));
        assert_eq!(requests.recv().unwrap().0, "/subjects/a%2Fb/versions/2");
    }

    #[test]
    fn versions_fetch_every_registered_schema() {
        let version = |number: i64| {
            serde_json::json!({"version": number, "id": number + 10, "schema": "{}"}).to_string()
        };
        let (url, requests) = registry(vec![
            (200, "[1, 2]".to_string()),
            (200, version(1)),
            (200, version(2)),
        ]);
        let versions =
            fetch_confluent_versions("confluent://orders/versions/latest", &options(&url)).unwrap();
        assert_eq!(
            versions
                .iter()
                .map(|document| (document.version.as_str(), document.schema_id.as_deref()))
                .collect::<Vec<_>>(),
            [("1", Some("11")), ("2", Some("12"))]
        );
        let paths: Vec<String> = requests.iter().take(3).map(|(path, _)| path).collect();
        assert_eq!(
            paths,
            [
                "/subjects/orders/versions",
                "/subjects/orders/versions/1",
                "/subjects/orders/versions/2"
            ]
        );

        let (url, _) = registry(vec![(200, r#"["1"]"#.to_string())]);
        let error =
            fetch_confluent_versions("confluent://orders/versions/1", &options(&url)).unwrap_err();
        assert!(error.to_string().contains("list of integers"));
    }

    #[test]
    fn rejects_unsupported_documents_and_responses() {
        let cases = [
            (
                200,
                r#"{"schema": "{}", "schemaType": "PROTOBUF"}"#,
                "type \"PROTOBUF\"",
            ),
            (
                200,
                r#"{"schema": "{}", "references": [{"name": "x"}]}"#,
                "references are not supported",
            ),
            (200, r#"[]"#, "must be an object"),
            (200, r#"{"schema": 1}"#, "missing string 'schema'"),
            (
                200,
                r#"{"schema": "{}", "schemaType": 1}"#,
                "invalid 'schemaType'",
            ),
            (
                200,
                r#"{"schema": "{}", "subject": 1}"#,
                "invalid subject or version",
            ),
            (
                200,
                r#"{"schema": "{}", "version": 1.5}"#,
                "invalid subject or version",
            ),
            (200, r#"{"schema": "{}", "id": []}"#, "invalid schema ID"),
            (
                200,
                r#"{"schema": "{}", "references": [1]}"#,
                "invalid references",
            ),
            (404, r#"{}"#, "returned HTTP 404"),
        ];
        let (url, _) = registry(
            cases
                .iter()
                .map(|(status, body, _)| (*status, body.to_string()))
                .collect(),
        );
        for (_, _, message) in cases {
            let reference = SchemaReference::new("confluent://orders/versions/1")
                .with_option("registry_url", &url);
            let error = DEFAULT_SCHEMA_RESOLVERS
                .resolve(&reference)
                .unwrap_err()
                .to_string();
            assert!(error.contains(message), "{error}");
        }
    }

    #[test]
    fn rejects_invalid_references_and_options() {
        let fetch = |url: &str, options: &[(&str, &str)]| {
            let options = options
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect();
            fetch_confluent_schema(url, &options)
                .unwrap_err()
                .to_string()
        };
        let registry_url = ("registry_url", "http://127.0.0.1:9");
        for url in [
            "http://orders/versions/1",
            "confluent://orders",
            "confluent:///versions/1",
            "confluent://orders/schemas/1",
            "confluent://orders/versions/1?x=1",
        ] {
            assert!(
                fetch(url, &[registry_url]).contains("confluent://SUBJECT"),
                "{url}"
            );
        }
        for version in ["0", "abc", "2147483648"] {
            let url = format!("confluent://orders/versions/{version}");
            assert!(fetch(&url, &[registry_url]).contains("VERSION must be"));
        }
        let url = "confluent://orders/versions/-1";
        assert!(fetch(url, &[]).contains("require the 'registry_url' option"));
        assert!(fetch(url, &[("registry_url", "ftp://x")]).contains("HTTP or HTTPS"));
        assert!(fetch(url, &[registry_url, ("token", "x")])
            .contains("unknown Confluent options: token"));
        assert!(fetch(url, &[registry_url, ("username", "x")]).contains("provided together"));
        assert!(fetch(url, &[registry_url, ("timeout", "soon")]).contains("number of seconds"));
        assert!(fetch(url, &[registry_url, ("timeout", "1")]).contains("request failed"));
    }

    #[test]
    fn encodes_basic_credentials() {
        assert_eq!(base64("user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64("a"), "YQ==");
        assert_eq!(base64("ab"), "YWI=");
        assert_eq!(decode("a%2fb%zz%"), "a/b%zz%");
    }
}
