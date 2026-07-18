from __future__ import annotations

import tempfile
from collections.abc import Mapping, Sequence
from typing import Any

import pyarrow as pa
from fsspec import AbstractFileSystem, filesystem
from fsspec.core import url_to_fs
from fsspec.implementations.chained import ChainedFileSystem

from .interchange import DEFAULT_REGISTRY, DataFormat, InterchangeRequest, SchemaPolicy


class DataFileSystem(ChainedFileSystem):
    """Read-only format and schema conversion layered over another filesystem."""

    protocol = "fsspec-data"

    def __init__(
        self,
        fo: str,
        target_protocol: str | None = None,
        target_options: dict[str, Any] | None = None,
        fs: AbstractFileSystem | None = None,
        provided_format: DataFormat | str | None = None,
        requested_format: DataFormat | str | None = None,
        provided_schema: pa.Schema | Mapping[str, Any] | Sequence[Mapping[str, Any]] | None = None,
        requested_schema: pa.Schema | Mapping[str, Any] | Sequence[Mapping[str, Any]] | None = None,
        schema_policy: SchemaPolicy | str = SchemaPolicy.EXACT,
        batch_size: int = 1024,
        row_limit: int | None = None,
        byte_limit: int | None = None,
        spool_max_size: int = 8 * 1024 * 1024,
        **kwargs: Any,
    ) -> None:
        super().__init__(**kwargs)
        if fs is not None and target_protocol is not None:
            raise ValueError("provide either fs or target_protocol, not both")
        if fs is None:
            if target_protocol is None:
                fs, fo = url_to_fs(fo, **(target_options or {}))
            else:
                fs = filesystem(target_protocol, **(target_options or {}))

        self.fs = fs
        self.fo = fs._strip_protocol(fo)
        self.provided_format = DataFormat(provided_format) if provided_format is not None else None
        self.requested_format = DataFormat(requested_format) if requested_format is not None else None
        self.provided_schema = _schema_from_options(provided_schema)
        self.requested_schema = _schema_from_options(requested_schema)
        self.schema_policy = SchemaPolicy(schema_policy)
        self.batch_size = batch_size
        self.row_limit = row_limit
        self.byte_limit = byte_limit
        self.spool_max_size = spool_max_size
        self._sizes: dict[str, int] = {}

    def _open(
        self,
        path: str,
        mode: str = "rb",
        block_size: int | None = None,
        autocommit: bool = True,
        cache_options: dict[str, Any] | None = None,
        **kwargs: Any,
    ):
        del block_size, autocommit, cache_options, kwargs
        if mode != "rb":
            raise ValueError("fsspec-data is a read-only filesystem")

        path = self._strip_protocol(path)
        output = self._convert(path)
        file = tempfile.SpooledTemporaryFile(max_size=self.spool_max_size, mode="w+b")
        file.write(output)
        file.seek(0)
        self._sizes[path] = len(output)
        return file

    def info(self, path: str, **kwargs: Any) -> dict[str, Any]:
        del kwargs
        path = self._strip_protocol(path)
        size = self._sizes.get(path)
        if size is None:
            with self._open(path) as file:
                file.seek(0, 2)
                size = file.tell()
        return {"name": path, "size": size, "type": "file"}

    def ls(self, path: str, detail: bool = True, **kwargs: Any):
        path = self._strip_protocol(path)
        if not path:
            return []
        entry = self.info(path, **kwargs)
        return [entry] if detail else [entry["name"]]

    def _convert(self, path: str) -> bytes:
        provided_format = self.provided_format or _format_from_path(self.fo)
        requested_format = self.requested_format or _format_from_path(path)
        source = self.fs.cat_file(self.fo)
        decoded = DEFAULT_REGISTRY.get(provided_format).decode_batches(
            source,
            schema=self.provided_schema,
            batch_size=self.batch_size,
            row_limit=self.row_limit,
            byte_limit=self.byte_limit,
        )
        if self.provided_schema is not None and not decoded.schema.equals(self.provided_schema, check_metadata=False):
            raise ValueError("provided schema does not match the source schema")

        provided_schema = self.provided_schema or decoded.schema
        requested_schema = self.requested_schema or provided_schema
        plan = InterchangeRequest(
            provided_format,
            requested_format,
            provided_schema,
            requested_schema,
            self.schema_policy,
        ).plan()
        batches = tuple(plan.apply_batch(batch) for batch in decoded.batches)
        return DEFAULT_REGISTRY.get(requested_format).encode_batches(batches, schema=requested_schema)


_SUFFIX_FORMATS = {
    ".arrow": DataFormat.ARROW,
    ".csv": DataFormat.CSV,
    ".ipc": DataFormat.ARROW,
    ".jsonl": DataFormat.JSONL,
    ".ndjson": DataFormat.JSONL,
    ".parquet": DataFormat.PARQUET,
    ".pq": DataFormat.PARQUET,
}


def _format_from_path(path: str) -> DataFormat:
    suffix = "." + path.rsplit(".", 1)[-1].lower() if "." in path.rsplit("/", 1)[-1] else ""
    try:
        return _SUFFIX_FORMATS[suffix]
    except KeyError as error:
        raise ValueError(f"cannot infer tabular format from path {path!r}") from error


def _schema_from_options(
    value: pa.Schema | Mapping[str, Any] | Sequence[Mapping[str, Any]] | None,
) -> pa.Schema | None:
    if value is None or isinstance(value, pa.Schema):
        return value
    fields = value.get("fields") if isinstance(value, Mapping) else value
    if not isinstance(fields, Sequence) or isinstance(fields, (str, bytes)):
        raise TypeError("schema options must contain a sequence of fields")
    return pa.schema([_field_from_options(field) for field in fields])


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
