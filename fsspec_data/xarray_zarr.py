from __future__ import annotations

from collections.abc import Mapping
from importlib import import_module
from typing import Any


def zarr_to_xarray(
    source: Any,
    target: Any | None,
    *,
    source_options: Mapping[str, Any],
    target_options: Mapping[str, Any],
    conversion_options: Mapping[str, Any],
) -> Any:
    if target is not None:
        raise ValueError("zarr-to-xarray conversion returns an object and does not accept a target")
    if target_options:
        raise ValueError("zarr-to-xarray conversion does not accept target_options")
    options = dict(conversion_options)
    _add_storage_options(options, source_options)
    return _import_xarray().open_zarr(source, **options)


def xarray_to_zarr(
    source: Any,
    target: Any | None,
    *,
    source_options: Mapping[str, Any],
    target_options: Mapping[str, Any],
    conversion_options: Mapping[str, Any],
) -> Any:
    xarray = _import_xarray()
    if not isinstance(source, (xarray.Dataset, xarray.DataArray)):
        raise TypeError("xarray-to-zarr conversion requires an xarray Dataset or DataArray")
    if source_options:
        raise ValueError("xarray-to-zarr conversion does not accept source_options")
    options = dict(conversion_options)
    _add_storage_options(options, target_options)
    return source.to_zarr(store=target, **options)


def _add_storage_options(options: dict[str, Any], storage_options: Mapping[str, Any]) -> None:
    if storage_options and "storage_options" in options:
        raise ValueError("provide storage options through source_options or target_options, not conversion_options")
    if storage_options:
        options["storage_options"] = dict(storage_options)


def _import_xarray():
    try:
        return import_module("xarray")
    except ImportError as error:
        raise ImportError("Xarray/Zarr conversion requires 'fsspec-data[xarray]'") from error
