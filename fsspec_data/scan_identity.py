"""Immutable comparison keys for scan requests; no caching or read sharing."""

from __future__ import annotations

import json
import math
from collections.abc import Sequence
from dataclasses import dataclass, field
from importlib import import_module
from typing import Any, TypeAlias

_rust = import_module(".fsspec_data", __package__)

JsonValue: TypeAlias = None | bool | int | float | str | list[Any] | dict[str, Any]


def _json(value: JsonValue) -> str:
    def validate(item: JsonValue) -> None:
        if item is None or type(item) in (bool, str):
            return
        if type(item) is int:
            if not -(2**63) <= item < 2**64:
                raise ValueError("identity integers must fit signed or unsigned 64 bits")
        elif type(item) is float:
            if not math.isfinite(item):
                raise ValueError("identity floats must be finite")
        elif type(item) is list:
            for child in item:
                validate(child)
        elif type(item) is dict:
            for key, child in item.items():
                if type(key) is not str:
                    raise TypeError("identity JSON object keys must be strings")
                validate(child)
        else:
            raise TypeError("identity values must be JSON scalars, lists or dictionaries")

    validate(value)
    return json.dumps(value, allow_nan=False)


@dataclass(frozen=True, repr=False)
class SourceIdentity:
    """One source configuration and credential context; retain this token for reuse.

    Create a fresh token when configuration or effective authorization changes.
    Tokens are process-local, opaque, and contain no credentials.
    """

    _native: Any = field(default_factory=_rust.SourceIdentity, init=False)

    def __repr__(self) -> str:
        return "SourceIdentity(...)"


@dataclass(frozen=True, init=False, repr=False)
class ScanIdentity:
    """Compare resolved scan requests without exposing contents in repr.

    JSON objects compare independently of key order; arrays retain order. A missing
    snapshot makes each construction distinct. Filters are compared, not executed.
    """

    _native: Any

    def __init__(
        self,
        source: SourceIdentity,
        path: str,
        *,
        snapshot: JsonValue = None,
        columns: Sequence[str] | None = None,
        row_offset: int = 0,
        batch_size: int = 1024,
        row_limit: int | None = None,
        byte_limit: int | None = None,
        filter: JsonValue = None,
        start_position: JsonValue = None,
        backend_options: JsonValue = None,
    ) -> None:
        native = _rust.scan_identity(
            source._native,
            path,
            _json(snapshot),
            None if columns is None else list(columns),
            row_offset,
            batch_size,
            row_limit,
            byte_limit,
            _json(filter),
            _json(start_position),
            _json(backend_options),
        )
        object.__setattr__(self, "_native", native)

    def __repr__(self) -> str:
        return "ScanIdentity(...)"

    @classmethod
    def _from_native(cls, native: Any) -> ScanIdentity:
        identity = object.__new__(cls)
        object.__setattr__(identity, "_native", native)
        return identity
