from __future__ import annotations

import json
import tempfile
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Any

import pyarrow as pa
from fsspec import AbstractFileSystem, filesystem
from fsspec.core import url_to_fs
from fsspec.implementations.chained import ChainedFileSystem

from .describe import DataSource, Facet, FileDataSource, Node
from .interchange import DEFAULT_REGISTRY, DataFormat, SchemaPolicy, plan_schema
from .schema import SchemaInput, SchemaProvenance, normalize_schema, resolve_schema

DEFAULT_MAPPING: dict[str, list[str]] = {
    "table": ["raw", "parquet"],
    "raw": ["raw"],
    "metadata": ["json"],
    "schema": ["json"],
    "index": ["json"],
}

_TABULAR_FORMS = {"json", *(format.value for format in DataFormat)}
_FORMS = {
    "table": {"raw"} | _TABULAR_FORMS,
    "raw": {"raw"},
    "metadata": {"json"},
    "schema": {"json"},
    "index": _TABULAR_FORMS,
}


@dataclass(frozen=True)
class ViewEntry:
    """A file or directory of a data view and what it renders. ``facet`` is ``children``,
    ``table``, ``raw``, ``metadata``, ``schema``, or ``index``; ``form`` is ``dir``, ``raw``,
    or a format name. ``url`` is the node's URL on the wrapped filesystem, when known."""

    name: str
    node: Node
    facet: str
    form: str
    url: str | None = None


class DataViewFileSystem(ChainedFileSystem):
    """Read-only view of a data source, rendering each node's facets as files.

    ``mapping`` lists the forms to show for each facet, for example
    ``{"table": ["raw", "parquet"], "metadata": ["json"]}``; facets it leaves out are not
    shown. A container becomes a directory, a table ``N.parquet`` (``raw`` keeps the
    native file), metadata ``N.metadata.json``, a table's schema ``N.schema.json``, and a
    container's index of its children ``N.json``.
    """

    protocol = "dataview"
    cachable = False

    def __init__(
        self,
        fo: str = "",
        target_protocol: str | None = None,
        target_options: dict[str, Any] | None = None,
        fs: AbstractFileSystem | None = None,
        source: DataSource | None = None,
        mapping: Mapping[str, Sequence[str]] | None = None,
        provided_schema: SchemaInput = None,
        requested_schema: SchemaInput = None,
        schema_policy: SchemaPolicy | str = SchemaPolicy.EXACT,
        renames: Mapping[str, str] | None = None,
        defaults: Mapping[str, Any] | None = None,
        drop: Sequence[str] | None = None,
        casts: Mapping[str, str] | None = None,
        batch_size: int = 1024,
        row_limit: int | None = None,
        byte_limit: int | None = None,
        spool_max_size: int = 8 * 1024 * 1024,
        **kwargs: Any,
    ) -> None:
        super().__init__(**kwargs)
        if sum(value is not None for value in (fs, target_protocol, source)) > 1:
            raise ValueError("provide only one of fs, target_protocol, and source")
        if source is None:
            if fs is None:
                if target_protocol is None:
                    fs, fo = url_to_fs(fo, **(target_options or {}))
                else:
                    fs = filesystem(target_protocol, **(target_options or {}))
            fo = fs._strip_protocol(fo)
            source = fs if isinstance(fs, DataSource) else FileDataSource(fs)
        self.fs = fs
        self.source = source
        self.root = fo
        self.mapping = _validate_mapping(DEFAULT_MAPPING if mapping is None else mapping)
        self.provided_schema = normalize_schema(provided_schema)
        self.requested_schema = normalize_schema(requested_schema)
        self.schema_policy = SchemaPolicy(schema_policy)
        self.renames = renames
        self.defaults = defaults
        self.drop = drop
        self.casts = casts
        self.batch_size = batch_size
        self.row_limit = row_limit
        self.byte_limit = byte_limit
        self.provided_schema_provenance: SchemaProvenance | None = None
        self.requested_schema_provenance: SchemaProvenance | None = None
        self.spool_max_size = spool_max_size
        self._listings: dict[str, dict[str, ViewEntry]] = {}
        self._sizes: dict[str, int] = {}

    def resolve(self, path: str) -> ViewEntry:
        """The node, facet, and form a view path renders."""
        path = self._view_path(path)
        if not path:
            return self._entry_for(self.source.describe(self.root), "", "children", "dir")
        parent, _, name = path.rpartition("/")
        source_dir = self._source_dir(parent)
        entries = self._entries(source_dir)
        if name in entries:
            return entries[name]
        entry = self._unlisted(source_dir, name)
        if entry is None:
            raise FileNotFoundError(path)
        return entry

    def ls(self, path: str, detail: bool = True, **kwargs: Any):
        del kwargs
        # Names keep the form the caller used, relative to the view or under its root.
        prefix = self._strip_protocol(path).strip("/")
        entries = self._entries(self._source_dir(self._view_path(path)))
        infos = [self._info(f"{prefix}/{name}" if prefix else name, entry) for name, entry in sorted(entries.items())]
        return infos if detail else [info["name"] for info in infos]

    def info(self, path: str, **kwargs: Any) -> dict[str, Any]:
        del kwargs
        path = self._view_path(path)
        info = self._info(path, self.resolve(path))
        if info["type"] == "file" and info["size"] is None:
            with self._open(path) as file:
                info["size"] = file.seek(0, 2)
        return info

    def invalidate_cache(self, path: str | None = None) -> None:
        self._listings.clear()
        self._sizes.clear()

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
            raise ValueError("dataview is a read-only filesystem")
        path = self._view_path(path)
        entry = self.resolve(path)
        if entry.facet == "children":
            raise IsADirectoryError(path)
        file = tempfile.SpooledTemporaryFile(max_size=self.spool_max_size, mode="w+b")  # noqa: SIM115
        try:
            self._render(entry, file)
            self._sizes[path] = file.tell()
            file.seek(0)
        except Exception:
            file.close()
            raise
        return file

    def _view_path(self, path: str) -> str:
        """``path`` relative to the view. Paths under the root are accepted too, since fsspec
        hands a chained URL without an outer path (``dataview::file:///data``) the wrapped
        filesystem's path."""
        path = self._strip_protocol(path)
        root = self.root.rstrip("/")
        if root and (path.rstrip("/") == root or path.startswith(f"{root}/")):
            path = path[len(root) :]
        return path.strip("/")

    def _info(self, path: str, entry: ViewEntry) -> dict[str, Any]:
        if entry.facet == "children":
            return {"name": path, "size": 0, "type": "directory"}
        return {"name": path, "size": self._sizes.get(path), "type": "file"}

    def _source_dir(self, path: str) -> str:
        if not path:
            return self.root
        entry = self.resolve(path)
        if entry.facet != "children":
            raise NotADirectoryError(path)
        return entry.node.path

    def _entries(self, source_dir: str) -> dict[str, ViewEntry]:
        if source_dir in self._listings:
            return self._listings[source_dir]
        entries: dict[str, ViewEntry] = {}
        if source_dir == self.root:
            root = self.source.describe(self.root)
            if not root.has(Facet.CHILDREN):
                # A single object as the root: the view holds that object's renderings.
                entries = {entry.name: entry for entry in self._render_node(root, _basename(root.path))}
                self._listings[source_dir] = entries
                return entries
            for entry in self._render_node(root, ""):
                if entry.facet != "children":
                    entries[entry.name] = entry
        for node in self.source.children(source_dir):
            for entry in self._render_node(node, _basename(node.path)):
                other = entries.get(entry.name)
                if other is not None:
                    raise ValueError(f"{entry.name!r} is rendered from both {other.node.path!r} and {node.path!r}")
                entries[entry.name] = entry
        self._listings[source_dir] = entries
        return entries

    def _unlisted(self, source_dir: str, name: str) -> ViewEntry | None:
        """Resolves a name no listing returns by describing the node it would come from."""
        stems = [name]
        for suffix in (".metadata.json", ".schema.json"):
            if name.endswith(suffix):
                stems.append(name[: -len(suffix)])
        if "." in name:
            stems.append(name.rsplit(".", 1)[0])
        for stem in stems:
            try:
                node = self.source.describe(f"{source_dir.rstrip('/')}/{stem}")
            except FileNotFoundError:
                continue
            for entry in self._render_node(node, stem):
                if entry.name == name:
                    return entry
        return None

    def _render_node(self, node: Node, base: str) -> list[ViewEntry]:
        """The view entries for one node, named after ``base``."""
        stem = base
        if node.native_format is not None and "." in base:
            stem = base.rsplit(".", 1)[0]
        entries = []
        if node.has(Facet.CHILDREN):
            entries.append(self._entry_for(node, base, "children", "dir"))
            if not node.has(Facet.TABLE):
                entries += [self._entry_for(node, f"{stem}.{form}", "index", form) for form in self.mapping.get("index", ())]
        if node.has(Facet.TABLE):
            forms = self.mapping.get("table", ())
            native = node.native_format.value if node.native_format is not None else None
            # The native file and a conversion to its own format share a name: with schema
            # options the conversion wins, otherwise the native file does.
            keep_raw = "raw" in forms and node.has(Facet.RAW) and not (self._converts and native in forms)
            for form in forms:
                if form == "raw":
                    if keep_raw:
                        entries.append(self._entry_for(node, base, "raw", "raw"))
                elif not (form == native and keep_raw):
                    entries.append(self._entry_for(node, f"{stem}.{form}", "table", form))
            entries += [self._entry_for(node, f"{stem}.schema.{form}", "schema", form) for form in self.mapping.get("schema", ())]
        elif node.has(Facet.RAW) and "raw" in self.mapping.get("raw", ()):
            entries.append(self._entry_for(node, base, "raw", "raw"))
        if node.has(Facet.METADATA):
            entries += [self._entry_for(node, f"{stem}.metadata.{form}", "metadata", form) for form in self.mapping.get("metadata", ())]
        return entries

    def _entry_for(self, node: Node, name: str, facet: str, form: str) -> ViewEntry:
        url = self.fs.unstrip_protocol(node.path) if self.fs is not None else None
        return ViewEntry(name, node, facet, form, url)

    @property
    def _converts(self) -> bool:
        """Whether tables go through a schema plan instead of passing through."""
        return any(
            value is not None for value in (self.provided_schema, self.requested_schema, self.renames, self.defaults, self.drop, self.casts)
        ) or (self.schema_policy is not SchemaPolicy.EXACT)

    def _rows(self, node: Node) -> tuple[pa.Schema, Any]:
        """The schema and batches a table renders, after the view's schema options."""
        limits = {"batch_size": self.batch_size, "row_limit": self.row_limit, "byte_limit": self.byte_limit}
        provided = resolve_schema(self.provided_schema)
        requested = resolve_schema(self.requested_schema)
        self.provided_schema_provenance = provided.provenance if provided is not None else None
        self.requested_schema_provenance = requested.provenance if requested is not None else None
        if provided is not None and node.native_format is not None and node.has(Facet.RAW):
            decoded = DEFAULT_REGISTRY.get(node.native_format).iter_batches(self.source.open_raw(node.path), schema=provided.schema, **limits)
        else:
            decoded = self.source.scan(node.path, **limits)
        if provided is not None and not decoded.schema.equals(provided.schema, check_metadata=False):
            raise ValueError("provided schema does not match the source schema")
        if not self._converts:
            return decoded.schema, decoded
        plan = plan_schema(
            decoded.schema,
            requested.schema if requested is not None else decoded.schema,
            self.schema_policy,
            renames=self.renames,
            defaults=self.defaults,
            drop=self.drop,
            casts=self.casts,
        )
        return plan.requested_schema, (plan.apply_batch(batch) for batch in decoded)

    def _render(self, entry: ViewEntry, output) -> None:
        path = entry.node.path
        native = entry.node.native_format
        if entry.facet == "raw" or (entry.facet == "table" and native is not None and entry.form == native.value and not self._converts):
            with self.source.open_raw(path) as raw:
                while chunk := raw.read(1024 * 1024):
                    output.write(chunk)
        elif entry.facet == "table":
            schema, batches = self._rows(entry.node)
            if entry.form == "json":
                _write_json(pa.Table.from_batches(list(batches), schema=schema).to_pylist(), output)
            else:
                DEFAULT_REGISTRY.get(entry.form).encode_batches_to(batches, output, schema=schema)
        elif entry.facet == "metadata":
            _write_json(self.source.metadata(path), output)
        elif entry.facet == "schema":
            schema = self._rows(entry.node)[0] if self._converts else self.source.schema(path)
            _write_json({"fields": [{"name": field.name, "type": str(field.type), "nullable": field.nullable} for field in schema]}, output)
        elif entry.facet == "index":
            rows = [self._index_row(child) for child in self.source.children(path)]
            if entry.form == "json":
                _write_json(rows, output)
            else:
                table = pa.Table.from_pylist([{key: _scalar(value) for key, value in row.items()} for row in rows])
                DEFAULT_REGISTRY.get(entry.form).encode_batches_to(table.to_batches(), output, schema=table.schema)

    def _index_row(self, node: Node) -> dict[str, Any]:
        row: dict[str, Any] = {"name": _basename(node.path)}
        if node.has(Facet.METADATA):
            for key, value in self.source.metadata(node.path).items():
                row.setdefault(key, value)
        return row


def _validate_mapping(mapping: Mapping[str, Sequence[str]]) -> dict[str, list[str]]:
    validated = {}
    for facet, forms in mapping.items():
        if facet not in _FORMS:
            raise ValueError(f"unknown facet in mapping: {facet!r}")
        if isinstance(forms, str):
            raise TypeError(f"forms for {facet!r} must be a list, not a string")
        unknown = [form for form in forms if form not in _FORMS[facet]]
        if unknown:
            raise ValueError(f"unknown forms for {facet!r}: {unknown}")
        validated[facet] = list(forms)
    return validated


def _basename(path: str) -> str:
    return path.rstrip("/").rsplit("/", 1)[-1]


def _scalar(value: Any) -> Any:
    return value if isinstance(value, (str, int, float, bool, type(None))) else json.dumps(value, default=str)


def _write_json(value: Any, output) -> None:
    output.write(json.dumps(value, indent=2, default=str).encode())
    output.write(b"\n")
