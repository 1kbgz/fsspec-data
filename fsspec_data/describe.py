"""The description contract between a backend and the data views built on it.

A backend describes nodes in its own namespace. Each node offers some facets, and a
consumer reads a facet through the matching ``DataSource`` method. fsspec-data renders
facets as files; the backend never knows which forms are requested.
"""

from __future__ import annotations

from collections.abc import Iterable, Sequence
from dataclasses import dataclass
from enum import Enum
from typing import Any, BinaryIO

import pyarrow as pa
from fsspec import AbstractFileSystem

from .interchange import DEFAULT_REGISTRY, DataFormat, DecodedBatchStream, _rust


class Facet(str, Enum):
    """Something a node offers."""

    CHILDREN = "children"
    TABLE = "table"
    RAW = "raw"
    METADATA = "metadata"


class FacetNotSupportedError(NotImplementedError):
    def __init__(self, path: str, facet: Facet) -> None:
        super().__init__(f"{path} has no {facet.value} facet")
        self.path = path
        self.facet = facet


@dataclass(frozen=True)
class Node:
    """A node in a backend's namespace and the facets it offers. ``native_format`` is the
    encoding of the raw bytes when they hold the node's rows, such as a CSV object."""

    path: str
    facets: frozenset[Facet]
    native_format: DataFormat | None = None

    def __init__(self, path: str, facets: Iterable[Facet | str], native_format: DataFormat | str | None = None) -> None:
        object.__setattr__(self, "path", path)
        object.__setattr__(self, "facets", frozenset(Facet(facet) for facet in facets))
        object.__setattr__(self, "native_format", DataFormat(native_format) if native_format is not None else None)

    def has(self, facet: Facet | str) -> bool:
        return Facet(facet) in self.facets


class DataSource:
    """A backend that describes its nodes. Methods for facets a backend never offers keep
    their defaults, which raise ``FacetNotSupportedError``."""

    def describe(self, path: str) -> Node:
        """Describes the node at ``path``, including paths that no listing returns."""
        raise NotImplementedError

    def children(self, path: str) -> list[Node]:
        """The children of a container. Encoded aliases of other nodes are left out, so a
        view does not show the same rows twice."""
        raise FacetNotSupportedError(path, Facet.CHILDREN)

    def schema(self, path: str) -> pa.Schema:
        raise FacetNotSupportedError(path, Facet.TABLE)

    def scan(
        self,
        path: str,
        *,
        batch_size: int = 1024,
        row_limit: int | None = None,
        byte_limit: int | None = None,
    ) -> DecodedBatchStream:
        raise FacetNotSupportedError(path, Facet.TABLE)

    def scan_columns(
        self,
        path: str,
        columns: Sequence[str],
        *,
        batch_size: int = 1024,
        row_limit: int | None = None,
        byte_limit: int | None = None,
    ) -> DecodedBatchStream:
        """Scan top-level columns in requested order, preserving duplicates and row counts.

        Existing backends use batch projection. Output limits apply after projection.
        """
        source = self.scan(path, batch_size=batch_size, row_limit=row_limit, byte_limit=None)
        schema = getattr(source, "schema", None)
        if schema is None:
            schema = self.schema(path)
        schema, native = _rust.project_batches(schema, iter(source), columns, batch_size, row_limit, byte_limit)
        return DecodedBatchStream(schema, native, on_cancel=getattr(source, "cancel", None))

    def open_raw(self, path: str) -> BinaryIO:
        raise FacetNotSupportedError(path, Facet.RAW)

    def metadata(self, path: str) -> dict[str, Any]:
        raise FacetNotSupportedError(path, Facet.METADATA)


class FileDataSource(DataSource):
    """Describes any fsspec filesystem: directories are containers, and files with a
    recognized data suffix (``.csv``, ``.parquet``, ...) are tables whose raw bytes are their
    native encoding. Every node has metadata from its file information. CSV and JSONL
    schemas are inferred from the first ``sample_size`` bytes."""

    def __init__(self, fs: AbstractFileSystem, sample_size: int = 1024 * 1024) -> None:
        self.fs = fs
        self.sample_size = sample_size

    def _node(self, info: dict[str, Any]) -> Node:
        path = self.fs._strip_protocol(info["name"])
        if info["type"] == "directory":
            return Node(path, [Facet.CHILDREN, Facet.METADATA])
        format = DataFormat.from_path(path)
        if format is None:
            return Node(path, [Facet.RAW, Facet.METADATA])
        return Node(path, [Facet.TABLE, Facet.RAW, Facet.METADATA], format)

    def _table_format(self, path: str) -> DataFormat:
        format = self.describe(path).native_format
        if format is None:
            raise FacetNotSupportedError(path, Facet.TABLE)
        return format

    def _decode_schema(self, path: str, format: DataFormat) -> pa.Schema | None:
        if format not in (DataFormat.CSV, DataFormat.JSONL):
            return None
        with self.fs.open(path, "rb") as file:
            sample = file.read(self.sample_size + 1)
        truncated = len(sample) > self.sample_size
        return DEFAULT_REGISTRY.get(format).infer_schema(sample[: self.sample_size], truncated=truncated)

    def describe(self, path: str) -> Node:
        return self._node(self.fs.info(path))

    def children(self, path: str) -> list[Node]:
        return sorted((self._node(info) for info in self.fs.ls(path, detail=True)), key=lambda node: node.path)

    def schema(self, path: str) -> pa.Schema:
        format = self._table_format(path)
        schema = self._decode_schema(path, format)
        return schema if schema is not None else self.scan(path).schema

    def scan(
        self,
        path: str,
        *,
        batch_size: int = 1024,
        row_limit: int | None = None,
        byte_limit: int | None = None,
    ) -> DecodedBatchStream:
        format = self._table_format(path)
        schema = self._decode_schema(path, format)
        return DEFAULT_REGISTRY.get(format).iter_batches(
            self.open_raw(path),
            schema=schema,
            batch_size=batch_size,
            row_limit=row_limit,
            byte_limit=byte_limit,
        )

    def scan_columns(
        self,
        path: str,
        columns: Sequence[str],
        *,
        batch_size: int = 1024,
        row_limit: int | None = None,
        byte_limit: int | None = None,
    ) -> DecodedBatchStream:
        format = self._table_format(path)
        schema = self._decode_schema(path, format)
        return DEFAULT_REGISTRY.get(format).iter_batches(
            self.open_raw(path),
            schema=schema,
            columns=columns,
            batch_size=batch_size,
            row_limit=row_limit,
            byte_limit=byte_limit,
        )

    def open_raw(self, path: str) -> BinaryIO:
        return self.fs.open(path, "rb")

    def metadata(self, path: str) -> dict[str, Any]:
        info = self.fs.info(path)
        metadata = {}
        for key, value in info.items():
            if key in ("name", "type") or (key == "size" and info["type"] == "directory"):
                continue
            metadata[key] = value if isinstance(value, (str, int, float, bool, type(None))) else str(value)
        return metadata
