# Schema and converter reference

All names below are exported from `fsspec_data`.

## Schema values

| Type               | Fields                                                                     |
| ------------------ | -------------------------------------------------------------------------- |
| `SchemaRef`        | `url`, optional `format`, `storage_options` (default empty mapping).       |
| `SchemaProvenance` | `source`, `format`, `provider`, optional `identifier`, optional `version`. |
| `ResolvedSchema`   | `schema: pyarrow.Schema`, `provenance: SchemaProvenance`.                  |
| `SchemaDocument`   | `data: bytes`, `format: SchemaFormat`, `provenance: SchemaProvenance`.     |

`SchemaRef` copies storage options and omits them from its representation.
`SchemaFormat` values are `arrow-json`, `arrow-ipc-schema`, and `json-schema`.
`.arrow`, `.ipc`, and `.arrowschema` infer `arrow-ipc-schema`; other suffixes require
an explicit format.

`arrow-json` is an object with a `fields` list. Each field has `name`, `type`, and
optional `nullable` (default true). Types accept PyArrow aliases.
`arrow-ipc-schema` accepts a serialized Arrow schema or Arrow IPC stream.

### `resolve_schema(value, registry=None)`

Accepts a PyArrow schema, `SchemaRef`, or `None`. Returns `ResolvedSchema` or
`None`. Inline Arrow schemas have `source="inline"`, `format="arrow"`, and
`provider="inline"`. References use `registry` or `DEFAULT_SCHEMA_RESOLVERS`.
The public function expects an already constructed reference; `DataFileSystem`
also accepts URL strings and reference mappings and normalizes them first.

### `SchemaResolverRegistry`

`register(format, decoder)` installs a `bytes -> pyarrow.Schema` decoder.
`register_provider(protocol, provider)` installs a `SchemaRef -> SchemaDocument`
provider. `resolve(reference)` invokes the provider and decoder and returns
`ResolvedSchema`. Unregistered protocols use fsspec document loading. A reference
format that disagrees with the provider's document format raises `ValueError`.

### `json_schema_to_arrow(document)`

Translates a structural JSON Schema rooted at an object. Objects, homogeneous
arrays, strings, booleans, integers, numbers, and nulls are supported. Unions may
contain null and one other type. Required properties control field nullability;
optional properties and explicit null unions are nullable.

References, composition keywords, multiple non-null union members, schema-valued
`additionalProperties`, tuple arrays, and `format` annotations raise `ValueError`
with the unsupported field path. Validation-only constraints such as numeric
bounds and string lengths are not represented or enforced.

### Confluent provider

`confluent://SUBJECT/versions/VERSION` accepts `latest`, `-1`, or a positive 32-bit
integer. Its storage options are required `registry_url`, optional Basic auth
`username` and `password`, optional string-valued `headers`, and optional numeric
`timeout` in seconds. JSON Schema records without external references are
supported; Avro, Protobuf, and referenced documents raise `ValueError`.

## Resource converters

### `Converter(source_type, target_type, handler)`

Frozen route descriptor. Route names are stripped and lowercased. Empty or
non-string names raise `ValueError`; a non-callable handler raises `TypeError`.

`convert(source, target=None, *, source_options=None, target_options=None, conversion_options=None)` calls the handler with independent copied option
mappings. Invalid option mappings raise `TypeError`. Return values are handler-defined.

### `ConverterRegistry(*, entry_point_group="fsspec_data.converters", discover_entry_points=True)`

| Method                                                              | Behavior                                                                                                                                    |
| ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `register(converter, replace=False)`                                | Registers a `Converter`; duplicate routes raise `ValueError` unless replacement is requested.                                               |
| `load_entry_points()`                                               | Discovers descriptors once, unless discovery is disabled. Non-converter descriptors raise `TypeError`; duplicate routes raise `ValueError`. |
| `get(source_type, target_type)`                                     | Loads entry points and returns a route; missing routes raise `ValueError`.                                                                  |
| `convert(source_type, target_type, source, target=None, **options)` | Invokes the selected converter with the three option mappings.                                                                              |

`CONVERTER_ENTRY_POINT_GROUP` is `fsspec_data.converters`.
`DEFAULT_CONVERTERS` includes `zarr -> xarray` and `xarray -> zarr`; dependencies
are installed with the `xarray` extra.

`zarr -> xarray` returns `xarray.open_zarr(source, **conversion_options)` and
accepts source storage options. It rejects a target and target options.
`xarray -> zarr` requires a Dataset or DataArray, calls `to_zarr`, and accepts
target storage options. It rejects source options. Storage options supplied both
through their dedicated mapping and `conversion_options` raise `ValueError`.
