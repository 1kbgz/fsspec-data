from __future__ import annotations

import importlib.metadata
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from typing import Any, TypeAlias

from .xarray_zarr import xarray_to_zarr, zarr_to_xarray

CONVERTER_ENTRY_POINT_GROUP = "fsspec_data.converters"
ConverterHandler: TypeAlias = Callable[..., Any]


@dataclass(frozen=True)
class Converter:
    source_type: str
    target_type: str
    handler: ConverterHandler = field(repr=False)

    def __post_init__(self) -> None:
        object.__setattr__(self, "source_type", _normalize_type(self.source_type))
        object.__setattr__(self, "target_type", _normalize_type(self.target_type))
        if not callable(self.handler):
            raise TypeError("converter handler must be callable")

    def convert(
        self,
        source: Any,
        target: Any | None = None,
        *,
        source_options: Mapping[str, Any] | None = None,
        target_options: Mapping[str, Any] | None = None,
        conversion_options: Mapping[str, Any] | None = None,
    ) -> Any:
        return self.handler(
            source,
            target,
            source_options=_copy_options(source_options, "source_options"),
            target_options=_copy_options(target_options, "target_options"),
            conversion_options=_copy_options(conversion_options, "conversion_options"),
        )


class ConverterRegistry:
    def __init__(
        self,
        *,
        entry_point_group: str = CONVERTER_ENTRY_POINT_GROUP,
        discover_entry_points: bool = True,
    ) -> None:
        self.entry_point_group = entry_point_group
        self.discover_entry_points = discover_entry_points
        self._converters: dict[tuple[str, str], Converter] = {}
        self._entry_points_loaded = False

    def register(self, converter: Converter, *, replace: bool = False) -> None:
        if not isinstance(converter, Converter):
            raise TypeError("converter must be a Converter")
        key = (converter.source_type, converter.target_type)
        if key in self._converters and not replace:
            raise ValueError(f"converter from {key[0]!r} to {key[1]!r} is already registered")
        self._converters[key] = converter

    def load_entry_points(self) -> None:
        if self._entry_points_loaded or not self.discover_entry_points:
            return
        discovered: list[tuple[str, Converter]] = []
        for entry_point in sorted(
            importlib.metadata.entry_points(group=self.entry_point_group),
            key=lambda item: (item.name, item.value),
        ):
            converter = entry_point.load()
            if not isinstance(converter, Converter):
                raise TypeError(f"converter entry point {entry_point.name!r} must load a Converter")
            discovered.append((entry_point.name, converter))

        keys = set(self._converters)
        for name, converter in discovered:
            key = (converter.source_type, converter.target_type)
            if key in keys:
                raise ValueError(f"converter entry point {name!r} duplicates the route from {key[0]!r} to {key[1]!r}")
            keys.add(key)
        self._converters.update(((converter.source_type, converter.target_type), converter) for _, converter in discovered)
        self._entry_points_loaded = True

    def get(self, source_type: str, target_type: str) -> Converter:
        self.load_entry_points()
        key = (_normalize_type(source_type), _normalize_type(target_type))
        try:
            return self._converters[key]
        except KeyError as error:
            raise ValueError(f"no converter registered from {key[0]!r} to {key[1]!r}") from error

    def convert(
        self,
        source_type: str,
        target_type: str,
        source: Any,
        target: Any | None = None,
        *,
        source_options: Mapping[str, Any] | None = None,
        target_options: Mapping[str, Any] | None = None,
        conversion_options: Mapping[str, Any] | None = None,
    ) -> Any:
        return self.get(source_type, target_type).convert(
            source,
            target,
            source_options=source_options,
            target_options=target_options,
            conversion_options=conversion_options,
        )


def _normalize_type(value: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ValueError("converter types must be non-empty strings")
    return value.strip().lower()


def _copy_options(value: Mapping[str, Any] | None, name: str) -> dict[str, Any]:
    if value is None:
        return {}
    if not isinstance(value, Mapping):
        raise TypeError(f"{name} must be a mapping")
    return dict(value)


XARRAY_TO_ZARR = Converter("xarray", "zarr", xarray_to_zarr)
ZARR_TO_XARRAY = Converter("zarr", "xarray", zarr_to_xarray)

DEFAULT_CONVERTERS = ConverterRegistry()
DEFAULT_CONVERTERS.register(XARRAY_TO_ZARR)
DEFAULT_CONVERTERS.register(ZARR_TO_XARRAY)
