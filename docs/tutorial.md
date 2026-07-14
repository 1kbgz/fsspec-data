# Convert tabular data

This tutorial converts CSV records with string identifiers into Parquet records with
integer identifiers. Along the way, you will create a conversion plan and consume its
output in bounded Arrow batches.

## Install fsspec-data

Install the package and PyArrow:

```console
python -m pip install fsspec-data pyarrow
```

## Create CSV input

Define the schema supplied by the producer and create one Arrow record batch:

```python
import pyarrow as pa

from fsspec_data import DEFAULT_REGISTRY, DataFormat

source_schema = pa.schema(
    [
        pa.field("id", pa.string(), nullable=False),
        pa.field("name", pa.string()),
    ]
)
source_batch = pa.record_batch(
    [["1", "2", "3"], ["Ada", "Grace", "Margaret"]],
    schema=source_schema,
)
csv_data = DEFAULT_REGISTRY.get(DataFormat.CSV).encode_batches([source_batch])

print(csv_data.decode())
```

The encoded input contains a header and three rows:

```text
id,name
1,Ada
2,Grace
3,Margaret
```

## Plan the conversion

Define the schema required by the consumer. Request `coerce` because converting strings to
integers requires runtime value checks:

```python
from fsspec_data import InterchangeRequest, SchemaPolicy

target_schema = pa.schema(
    [
        pa.field("id", pa.int64(), nullable=False),
        pa.field("name", pa.string()),
    ]
)
plan = InterchangeRequest(
    provided_format=DataFormat.CSV,
    requested_format=DataFormat.PARQUET,
    provided_schema=source_schema,
    requested_schema=target_schema,
    policy=SchemaPolicy.COERCE,
).plan()

print(plan.mappings)
```

The first field reports a `runtime_checked` cast; the second passes through unchanged.

## Consume bounded batches

Iterate with a two-row batch size:

```python
for batch in plan.iter_batches(csv_data, batch_size=2):
    print(batch.to_pylist())
```

The plan decodes and converts each batch as it is requested:

```text
[{'id': 1, 'name': 'Ada'}, {'id': 2, 'name': 'Grace'}]
[{'id': 3, 'name': 'Margaret'}]
```

## Produce Parquet output

Convert the complete input when the consumer needs encoded bytes:

```python
parquet_data = plan.convert(csv_data, batch_size=2)
result = DEFAULT_REGISTRY.get(DataFormat.PARQUET).decode_batches(parquet_data)

print(result.schema)
print([row for batch in result.batches for row in batch.to_pylist()])
```

The result uses the requested schema and contains integer identifiers:

```text
id: int64 not null
name: string
[{'id': 1, 'name': 'Ada'}, {'id': 2, 'name': 'Grace'}, {'id': 3, 'name': 'Margaret'}]
```

You have now used one plan for lazy Arrow consumption and encoded Parquet output. See the
[integration guide](how-to-integrate.md) to connect an existing producer or consumer.
