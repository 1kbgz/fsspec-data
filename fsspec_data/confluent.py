from __future__ import annotations

import json
import urllib.error
import urllib.request
from base64 import b64encode
from collections.abc import Mapping
from dataclasses import dataclass
from typing import Any
from urllib.parse import quote, unquote, urlsplit


@dataclass(frozen=True)
class ConfluentSchemaDocument:
    schema: str
    schema_type: str
    subject: str
    version: str
    schema_id: str | None
    references: tuple[Mapping[str, Any], ...]


def fetch_confluent_schema(url: str, storage_options: Mapping[str, Any]) -> ConfluentSchemaDocument:
    subject, requested_version = _parse_reference(url)
    options = dict(storage_options)
    try:
        registry_url = options.pop("registry_url")
    except KeyError as error:
        raise ValueError("Confluent schema references require storage_options['registry_url']") from error
    if not isinstance(registry_url, str) or not registry_url.startswith(("http://", "https://")):
        raise ValueError("Confluent registry_url must be an HTTP or HTTPS URL")

    username = options.pop("username", None)
    password = options.pop("password", None)
    if (
        (username is None) != (password is None)
        or (username is not None and not isinstance(username, str))
        or (password is not None and not isinstance(password, str))
    ):
        raise ValueError("Confluent username and password must be strings provided together")
    headers = options.pop("headers", {})
    if not isinstance(headers, Mapping) or not all(isinstance(key, str) and isinstance(value, str) for key, value in headers.items()):
        raise TypeError("Confluent headers must map strings to strings")
    timeout = options.pop("timeout", None)
    if timeout is not None and not isinstance(timeout, (int, float)):
        raise TypeError("Confluent timeout must be a number")
    if options:
        raise ValueError(f"unknown Confluent storage options: {', '.join(sorted(options))}")

    endpoint = f"{registry_url.rstrip('/')}/subjects/{quote(subject, safe='')}/versions/{quote(requested_version, safe='')}"
    request_headers = {
        "Accept": "application/vnd.schemaregistry.v1+json, application/json",
        **dict(headers),
    }
    if username is not None and password is not None:
        token = b64encode(f"{username}:{password}".encode()).decode()
        request_headers["Authorization"] = f"Basic {token}"
    request = urllib.request.Request(endpoint, headers=request_headers, method="GET")

    try:
        if timeout is None:
            response = urllib.request.urlopen(request)
        else:
            response = urllib.request.urlopen(request, timeout=float(timeout))
        with response:
            payload = json.load(response)
    except urllib.error.HTTPError as error:
        raise ValueError(f"Confluent Schema Registry returned HTTP {error.code}") from error
    except urllib.error.URLError as error:
        raise ValueError("Confluent Schema Registry request failed") from error

    if not isinstance(payload, Mapping):
        raise TypeError("Confluent Schema Registry response must be an object")
    schema = payload.get("schema")
    if not isinstance(schema, str):
        raise TypeError("Confluent Schema Registry response is missing string 'schema'")
    schema_type = payload.get("schemaType", "AVRO")
    if not isinstance(schema_type, str):
        raise TypeError("Confluent Schema Registry response has invalid 'schemaType'")
    response_subject = payload.get("subject", subject)
    response_version = payload.get("version", requested_version)
    schema_id = payload.get("id")
    references = payload.get("references", [])
    if not isinstance(response_subject, str) or not isinstance(response_version, (str, int)):
        raise TypeError("Confluent Schema Registry response has invalid subject or version")
    if schema_id is not None and not isinstance(schema_id, (str, int)):
        raise TypeError("Confluent Schema Registry response has invalid schema ID")
    if not isinstance(references, list) or not all(isinstance(reference, Mapping) for reference in references):
        raise TypeError("Confluent Schema Registry response has invalid references")
    return ConfluentSchemaDocument(
        schema=schema,
        schema_type=schema_type.upper(),
        subject=response_subject,
        version=str(response_version),
        schema_id=str(schema_id) if schema_id is not None else None,
        references=tuple(references),
    )


def _parse_reference(url: str) -> tuple[str, str]:
    parsed = urlsplit(url)
    if parsed.scheme != "confluent" or not parsed.netloc or parsed.query or parsed.fragment:
        raise ValueError("Confluent schema URL must be confluent://SUBJECT/versions/VERSION")
    parts = [unquote(part) for part in parsed.path.strip("/").split("/") if part]
    if len(parts) != 2 or parts[0] != "versions" or not parts[1]:
        raise ValueError("Confluent schema URL must be confluent://SUBJECT/versions/VERSION")
    version = parts[1]
    if version != "latest":
        try:
            number = int(version)
        except ValueError as error:
            raise ValueError("Confluent schema VERSION must be 'latest', -1, or a positive integer") from error
        if number != -1 and not 1 <= number <= 2**31 - 1:
            raise ValueError("Confluent schema VERSION must be 'latest', -1, or a positive integer")
    return unquote(parsed.netloc), version
