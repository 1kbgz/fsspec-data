# fsspec-data

Convert tabular formats and reconcile Arrow schemas over fsspec.

[![Build Status](https://github.com/1kbgz/fsspec-data/actions/workflows/build.yaml/badge.svg?branch=main&event=push)](https://github.com/1kbgz/fsspec-data/actions/workflows/build.yaml)
[![codecov](https://codecov.io/gh/1kbgz/fsspec-data/branch/main/graph/badge.svg)](https://codecov.io/gh/1kbgz/fsspec-data)
[![License](https://img.shields.io/github/license/1kbgz/fsspec-data)](https://github.com/1kbgz/fsspec-data)
[![PyPI](https://img.shields.io/pypi/v/fsspec-data.svg)](https://pypi.python.org/pypi/fsspec-data)

`fsspec-data` plans schema mappings and executes them over Arrow record batches.
Codecs handle Arrow IPC, Parquet, CSV, and JSONL. A read-only chained filesystem
exposes converted files; a separate converter registry handles resources such as
Xarray datasets and Zarr stores.

## Start here

[Convert CSV identifiers to Parquet](docs/tutorial.md) walks through planning,
batch iteration, and encoded output.

## How-to guides

For application work, [connect a batch producer or consumer](docs/how-to-integrate.md),
[convert a file through an fsspec chain](docs/how-to-chain-filesystems.md), or
[resolve schemas from files and registries](docs/how-to-resolve-schemas.md).
[Convert Xarray and Zarr resources](docs/how-to-convert-resources.md) covers native
resource routes and converter registration.

## Reference and explanation

The [interchange API reference](docs/api.md) covers codecs, policies, limits, and
planning. [Schema and converter reference](docs/resources.md) describes resolution
and resource routes. Read [Interchange design](docs/explanation.md) for the
boundaries between schemas, data, and consumers.
