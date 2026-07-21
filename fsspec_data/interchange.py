from __future__ import annotations

from collections.abc import Iterable
from dataclasses import dataclass, replace
from enum import Enum
from importlib import import_module
from itertools import chain
from typing import Any, BinaryIO

import pyarrow as pa
import pyarrow.compute as pc

_rust = import_module(".fsspec_data", __package__)


class DataFormat(str, Enum):
    ARROW = "arrow"
    PARQUET = "parquet"
    CSV = "csv"
    JSONL = "jsonl"


class SchemaPolicy(str, Enum):
    EXACT = "exact"
    PROJECTION = "projection"
    COMPATIBLE = "compatible"
    COERCE = "coerce"


@dataclass(frozen=True)
class CodecCapabilities:
    encode: bool
    decode: bool
    streaming: bool


@dataclass(frozen=True)
class DecodedBatches:
    schema: pa.Schema
    batches: tuple[pa.RecordBatch, ...]


class DecodedBatchStream:
    def __init__(self, schema: pa.Schema, native: Any) -> None:
        self.schema = schema
        self._native = native

    def __iter__(self) -> DecodedBatchStream:
        return self

    def __next__(self) -> pa.RecordBatch:
        return next(self._native)

    def cancel(self) -> None:
        self._native.cancel()

    def collect(self) -> DecodedBatches:
        return DecodedBatches(self.schema, tuple(self))


class PlannedBatchStream:
    def __init__(self, plan: InterchangePlan, source: DecodedBatchStream) -> None:
        self.schema = plan.requested_schema
        self._plan = plan
        self._source = source

    def __iter__(self) -> PlannedBatchStream:
        return self

    def __next__(self) -> pa.RecordBatch:
        return self._plan.apply_batch(next(self._source))

    def cancel(self) -> None:
        self._source.cancel()


@dataclass(frozen=True)
class Codec:
    format: DataFormat
    capabilities: CodecCapabilities

    def encode_batches(
        self,
        batches: list[pa.RecordBatch] | tuple[pa.RecordBatch, ...],
        *,
        schema: pa.Schema | None = None,
    ) -> bytes:
        batches = tuple(batches)
        if schema is None:
            if not batches:
                raise ValueError("schema is required when encoding no record batches")
            schema = batches[0].schema
        schema = _ensure_pyarrow(schema)
        for batch in batches:
            if not isinstance(batch, pa.RecordBatch):
                raise TypeError("batches must contain pyarrow.RecordBatch objects")
        return _rust.encode_batches(self.format.value, schema, batches)

    def encode_batches_to(
        self,
        batches: Iterable[pa.RecordBatch],
        output: BinaryIO,
        *,
        schema: pa.Schema | None = None,
    ) -> None:
        batches = iter(batches)
        if schema is None:
            try:
                first = next(batches)
            except StopIteration as error:
                raise ValueError("schema is required when encoding no record batches") from error
            if not isinstance(first, pa.RecordBatch):
                raise TypeError("batches must contain pyarrow.RecordBatch objects")
            schema = first.schema
            batches = chain((first,), batches)
        schema = _ensure_pyarrow(schema)
        writer = _rust.start_codec_writer(self.format.value, schema, output)
        for batch in batches:
            if not isinstance(batch, pa.RecordBatch):
                raise TypeError("batches must contain pyarrow.RecordBatch objects")
            writer.write_batch(batch)
        writer.finish()

    def decode_batches(
        self,
        data: bytes | bytearray | memoryview | BinaryIO,
        *,
        schema: pa.Schema | None = None,
        batch_size: int = 1024,
        row_limit: int | None = None,
        byte_limit: int | None = None,
    ) -> DecodedBatches:
        return self.iter_batches(
            data,
            schema=schema,
            batch_size=batch_size,
            row_limit=row_limit,
            byte_limit=byte_limit,
        ).collect()

    def iter_batches(
        self,
        data: bytes | bytearray | memoryview | BinaryIO,
        *,
        schema: pa.Schema | None = None,
        batch_size: int = 1024,
        row_limit: int | None = None,
        byte_limit: int | None = None,
    ) -> DecodedBatchStream:
        if schema is not None:
            schema = _ensure_pyarrow(schema)
        if isinstance(data, (bytes, bytearray, memoryview)):
            decoded_schema, native = _rust.decode_stream(
                self.format.value,
                bytes(data),
                schema,
                batch_size,
                row_limit,
                byte_limit,
            )
        elif hasattr(data, "read"):
            decoded_schema, native = _rust.decode_reader(
                self.format.value,
                data,
                schema,
                batch_size,
                row_limit,
                byte_limit,
            )
        else:
            raise TypeError("data must be bytes-like or a binary file-like object")
        return DecodedBatchStream(decoded_schema, native)


class CodecRegistry:
    def get(self, format: DataFormat | str) -> Codec:
        format = DataFormat(format)
        encode, decode, streaming = _rust.codec_capabilities(format.value)
        return Codec(format, CodecCapabilities(encode, decode, streaming))


DEFAULT_REGISTRY = CodecRegistry()


@dataclass(frozen=True)
class FieldMapping:
    source_index: int
    target_index: int
    cast: str | None = None
    check_nulls: bool = False


@dataclass(frozen=True)
class InterchangeRequest:
    provided_format: DataFormat
    requested_format: DataFormat
    provided_schema: pa.Schema
    requested_schema: pa.Schema
    policy: SchemaPolicy

    def plan(self) -> InterchangePlan:
        DEFAULT_REGISTRY.get(self.provided_format)
        DEFAULT_REGISTRY.get(self.requested_format)
        return replace(
            plan_schema(self.provided_schema, self.requested_schema, self.policy),
            provided_format=self.provided_format,
            requested_format=self.requested_format,
        )


@dataclass(frozen=True)
class InterchangePlan:
    provided_schema: pa.Schema
    requested_schema: pa.Schema
    policy: SchemaPolicy
    mappings: tuple[FieldMapping, ...]
    provided_format: DataFormat = DataFormat.ARROW
    requested_format: DataFormat = DataFormat.ARROW

    def apply_table(self, table: pa.Table) -> pa.Table:
        self._validate_input_schema(table.schema)
        arrays = [self._apply_mapping(table.column(mapping.source_index), mapping) for mapping in self.mappings]
        return pa.Table.from_arrays(arrays, schema=self.requested_schema)

    def apply_batch(self, batch: pa.RecordBatch) -> pa.RecordBatch:
        self._validate_input_schema(batch.schema)
        arrays = [self._apply_mapping(batch.column(mapping.source_index), mapping) for mapping in self.mappings]
        return pa.RecordBatch.from_arrays(arrays, schema=self.requested_schema)

    def _apply_mapping(self, array: pa.Array | pa.ChunkedArray, mapping: FieldMapping):
        field = self.requested_schema.field(mapping.target_index)
        if mapping.check_nulls and array.null_count:
            raise ValueError(f"field {field.name!r} contains nulls required by the requested schema")
        if mapping.cast is not None:
            return pc.cast(array, field.type, safe=mapping.cast != "lossy")
        return array

    def _validate_input_schema(self, schema: pa.Schema) -> None:
        if not schema.equals(self.provided_schema, check_metadata=False):
            raise ValueError("input schema does not match the schema used to create the plan")

    def iter_batches(
        self,
        data: bytes | bytearray | memoryview | BinaryIO,
        *,
        batch_size: int = 1024,
        row_limit: int | None = None,
        byte_limit: int | None = None,
    ) -> PlannedBatchStream:
        decode_schema = self.provided_schema if self.provided_format in {DataFormat.CSV, DataFormat.JSONL} else None
        source = DEFAULT_REGISTRY.get(self.provided_format).iter_batches(
            data,
            schema=decode_schema,
            batch_size=batch_size,
            row_limit=row_limit,
            byte_limit=byte_limit,
        )
        return PlannedBatchStream(self, source)

    def convert(
        self,
        data: bytes | bytearray | memoryview | BinaryIO,
        *,
        batch_size: int = 1024,
        row_limit: int | None = None,
        byte_limit: int | None = None,
    ) -> bytes:
        batches = tuple(
            self.iter_batches(
                data,
                batch_size=batch_size,
                row_limit=row_limit,
                byte_limit=byte_limit,
            )
        )
        return DEFAULT_REGISTRY.get(self.requested_format).encode_batches(
            batches,
            schema=self.requested_schema,
        )


def plan_schema(
    provided_schema: pa.Schema,
    requested_schema: pa.Schema,
    policy: SchemaPolicy | str,
) -> InterchangePlan:
    provided_schema = _ensure_pyarrow(provided_schema)
    requested_schema = _ensure_pyarrow(requested_schema)
    policy = SchemaPolicy(policy)
    mappings = _rust.plan_schema(
        provided_schema,
        requested_schema,
        policy.value,
    )
    return InterchangePlan(
        provided_schema=provided_schema,
        requested_schema=requested_schema,
        policy=policy,
        mappings=tuple(FieldMapping(source, target, cast, check_nulls) for source, target, cast, check_nulls in mappings),
    )


def _ensure_pyarrow(value: Any) -> pa.Schema:
    if not isinstance(value, pa.Schema):
        raise TypeError("schema must be a pyarrow.Schema")
    return value
