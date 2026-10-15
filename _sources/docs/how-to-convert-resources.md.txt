# How to convert Xarray and Zarr resources

Use the converter registry for datasets and stores that span multiple objects.

## Convert a dataset or store

Install the optional dependencies:

```console
pip install "fsspec-data[xarray]"
```

Open a Zarr store as an Xarray dataset while keeping storage credentials scoped to the source:

```python
from fsspec_data import DEFAULT_CONVERTERS

dataset = DEFAULT_CONVERTERS.convert(
    "zarr",
    "xarray",
    "s3://weather-input/forecast.zarr",
    source_options={"profile": "weather-reader"},
    conversion_options={"group": "forecast", "chunks": "auto"},
)
```

Write a dataset to another Zarr store with independent target credentials:

```python
DEFAULT_CONVERTERS.convert(
    "xarray",
    "zarr",
    dataset,
    "s3://weather-output/forecast.zarr",
    target_options={"profile": "weather-writer"},
    conversion_options={"mode": "w-", "zarr_format": 3},
)
```

Use the converter API rather than `DataFileSystem.open()` for Zarr because a Zarr hierarchy
contains multiple metadata and chunk objects.

## Register a converter through an entry point

Expose one `Converter` object from the integration package:

```python
from fsspec_data import Converter


def geotiff_to_xarray(
    source,
    target,
    *,
    source_options,
    target_options,
    conversion_options,
):
    ...


GEOTIFF_TO_XARRAY = Converter("geotiff", "xarray", geotiff_to_xarray)
```

Register that object in the package's `pyproject.toml`:

```toml
[project.entry-points."fsspec_data.converters"]
geotiff-xarray = "my_package.converters:GEOTIFF_TO_XARRAY"
```

Installed entry points are discovered on the registry's first lookup. Keep optional heavy
imports inside the converter handler so loading its descriptor remains inexpensive. Route
names are case-insensitive. Duplicate source-target routes raise an error instead of selecting
one by installation order.

Call `DEFAULT_CONVERTERS.register(converter)` instead when registration is local to one
process and does not need package discovery.
