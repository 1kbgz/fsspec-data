import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from fsspec_data import DEFAULT_REGISTRY, DataFormat, FieldMapping, InterchangeRequest, SchemaPolicy, plan_schema


def schemas():
    provided = pa.schema([pa.field("id", pa.int64(), nullable=False), pa.field("name", pa.string())])
    projected = pa.schema([pa.field("name", pa.string())])
    return provided, projected


def test_exact_schema_plan():
    provided, _ = schemas()

    plan = plan_schema(provided, provided, SchemaPolicy.EXACT)

    assert plan.mappings == (FieldMapping(0, 0), FieldMapping(1, 1))


def test_projection_selects_and_reorders_table():
    provided, projected = schemas()
    table = pa.table({"id": [1, 2], "name": ["ada", "grace"]}, schema=provided)

    result = plan_schema(provided, projected, "projection").apply_table(table)

    assert result.schema == projected
    assert result.to_pylist() == [{"name": "ada"}, {"name": "grace"}]


def test_projection_applies_to_record_batch():
    provided, projected = schemas()
    batch = pa.RecordBatch.from_arrays([pa.array([1]), pa.array(["ada"])], schema=provided)

    result = plan_schema(provided, projected, SchemaPolicy.PROJECTION).apply_batch(batch)

    assert result.schema == projected
    assert result.to_pylist() == [{"name": "ada"}]


def test_projection_rejects_missing_field():
    provided, _ = schemas()
    requested = pa.schema([("missing", pa.string())])

    with pytest.raises(ValueError, match="requested field not found"):
        plan_schema(provided, requested, SchemaPolicy.PROJECTION)


def test_projection_rejects_type_change():
    provided, _ = schemas()
    requested = pa.schema([("id", pa.string())])

    with pytest.raises(ValueError, match="type mismatch"):
        plan_schema(provided, requested, SchemaPolicy.PROJECTION)


def test_projection_rejects_nullability_narrowing():
    provided, _ = schemas()
    requested = pa.schema([pa.field("name", pa.string(), nullable=False)])

    with pytest.raises(ValueError, match="cannot narrow nullable"):
        plan_schema(provided, requested, SchemaPolicy.PROJECTION)


def test_exact_rejects_nullability_widening():
    provided, _ = schemas()
    requested = pa.schema([pa.field("id", pa.int64()), pa.field("name", pa.string())])

    with pytest.raises(ValueError, match="nullability mismatch"):
        plan_schema(provided, requested, SchemaPolicy.EXACT)


def test_compatible_widens_type_and_nullability():
    provided = pa.schema([pa.field("id", pa.int32(), nullable=False)])
    requested = pa.schema([pa.field("id", pa.int64(), nullable=True)])
    batch = pa.record_batch([[1, 2]], schema=provided)

    plan = plan_schema(provided, requested, SchemaPolicy.COMPATIBLE)
    result = plan.apply_batch(batch)

    assert plan.mappings == (FieldMapping(0, 0, "safe"),)
    assert result.schema == requested
    assert result.to_pylist() == [{"id": 1}, {"id": 2}]


def test_compatible_rejects_lossy_cast():
    provided = pa.schema([pa.field("id", pa.int64(), nullable=False)])
    requested = pa.schema([pa.field("id", pa.int32(), nullable=False)])

    with pytest.raises(ValueError, match="unsafe cast"):
        plan_schema(provided, requested, SchemaPolicy.COMPATIBLE)


def test_coerce_applies_runtime_checked_cast():
    provided = pa.schema([pa.field("id", pa.string(), nullable=False)])
    requested = pa.schema([pa.field("id", pa.int64(), nullable=False)])
    batch = pa.record_batch([["1", "2"]], schema=provided)

    plan = plan_schema(provided, requested, SchemaPolicy.COERCE)
    result = plan.apply_batch(batch)

    assert plan.mappings == (FieldMapping(0, 0, "runtime_checked"),)
    assert result.to_pylist() == [{"id": 1}, {"id": 2}]


def test_coerce_checks_nullable_to_required_at_runtime():
    provided = pa.schema([pa.field("id", pa.int64(), nullable=True)])
    requested = pa.schema([pa.field("id", pa.int64(), nullable=False)])
    plan = plan_schema(provided, requested, SchemaPolicy.COERCE)

    with pytest.raises(ValueError, match="contains nulls"):
        plan.apply_batch(pa.record_batch([[1, None]], schema=provided))


def test_coerce_rejects_unregistered_cast():
    provided = pa.schema([pa.field("id", pa.list_(pa.int64()))])
    requested = pa.schema([pa.field("id", pa.int64())])

    with pytest.raises(ValueError, match="no registered coercion"):
        plan_schema(provided, requested, SchemaPolicy.COERCE)


def test_interchange_request_plans_arrow_to_arrow():
    provided, projected = schemas()
    request = InterchangeRequest(
        DataFormat.ARROW,
        DataFormat.ARROW,
        provided,
        projected,
        SchemaPolicy.PROJECTION,
    )

    assert request.plan().mappings == (FieldMapping(1, 0),)


def test_interchange_request_plans_registered_format_path():
    provided, projected = schemas()
    request = InterchangeRequest(
        DataFormat.PARQUET,
        DataFormat.ARROW,
        provided,
        projected,
        SchemaPolicy.PROJECTION,
    )

    plan = request.plan()

    assert plan.provided_format is DataFormat.PARQUET
    assert plan.requested_format is DataFormat.ARROW


def test_interchange_plan_converts_parquet_to_arrow_with_compatible_cast():
    provided = pa.schema([pa.field("id", pa.int32(), nullable=False)])
    requested = pa.schema([pa.field("id", pa.int64(), nullable=True)])
    batch = pa.record_batch([[1, 2]], schema=provided)
    parquet = DEFAULT_REGISTRY.get(DataFormat.PARQUET).encode_batches([batch])
    plan = InterchangeRequest(
        DataFormat.PARQUET,
        DataFormat.ARROW,
        provided,
        requested,
        SchemaPolicy.COMPATIBLE,
    ).plan()

    arrow = plan.convert(parquet)
    result = DEFAULT_REGISTRY.get(DataFormat.ARROW).decode_batches(arrow)

    assert result.schema == requested
    assert result.batches[0].to_pylist() == [{"id": 1}, {"id": 2}]


def test_plan_rejects_different_input_schema():
    provided, projected = schemas()
    plan = plan_schema(provided, projected, SchemaPolicy.PROJECTION)

    with pytest.raises(ValueError, match="input schema"):
        plan.apply_table(pa.table({"id": [1], "name": ["ada"]}))


def test_arrow_codec_declares_capabilities():
    capabilities = DEFAULT_REGISTRY.get(DataFormat.ARROW).capabilities

    assert capabilities.encode
    assert capabilities.decode
    assert capabilities.streaming


def test_registry_contains_all_formats():
    assert {DEFAULT_REGISTRY.get(format).format for format in DataFormat} == set(DataFormat)


def test_arrow_ipc_round_trip_preserves_schema_and_batch_boundaries():
    provided, _ = schemas()
    batches = (
        pa.RecordBatch.from_arrays([pa.array([1, 2]), pa.array(["ada", "grace"])], schema=provided),
        pa.RecordBatch.from_arrays([pa.array([3]), pa.array(["margaret"])], schema=provided),
    )
    codec = DEFAULT_REGISTRY.get(DataFormat.ARROW)

    decoded = codec.decode_batches(codec.encode_batches(batches))

    assert decoded.schema == provided
    assert [batch.num_rows for batch in decoded.batches] == [2, 1]
    assert pa.Table.from_batches(decoded.batches).to_pylist() == [
        {"id": 1, "name": "ada"},
        {"id": 2, "name": "grace"},
        {"id": 3, "name": "margaret"},
    ]


def test_arrow_ipc_round_trip_preserves_empty_stream_schema():
    provided, _ = schemas()
    codec = DEFAULT_REGISTRY.get(DataFormat.ARROW)

    decoded = codec.decode_batches(codec.encode_batches((), schema=provided))

    assert decoded.schema == provided
    assert decoded.batches == ()


def test_arrow_codec_requires_schema_for_empty_stream():
    codec = DEFAULT_REGISTRY.get(DataFormat.ARROW)

    with pytest.raises(ValueError, match="schema is required"):
        codec.encode_batches(())


def test_parquet_round_trip_preserves_schema_and_rows():
    provided, _ = schemas()
    batches = (
        pa.RecordBatch.from_arrays([pa.array([1, 2]), pa.array(["ada", "grace"])], schema=provided),
        pa.RecordBatch.from_arrays([pa.array([3]), pa.array(["margaret"])], schema=provided),
    )
    codec = DEFAULT_REGISTRY.get(DataFormat.PARQUET)

    encoded = codec.encode_batches(batches)
    decoded = codec.decode_batches(encoded)

    assert encoded.startswith(b"PAR1")
    assert decoded.schema == provided
    assert pa.Table.from_batches(decoded.batches).to_pylist() == [
        {"id": 1, "name": "ada"},
        {"id": 2, "name": "grace"},
        {"id": 3, "name": "margaret"},
    ]
    assert pq.read_table(pa.BufferReader(encoded)).to_pylist() == pa.Table.from_batches(batches).to_pylist()


def test_parquet_round_trip_preserves_empty_file_schema():
    provided, _ = schemas()
    codec = DEFAULT_REGISTRY.get(DataFormat.PARQUET)

    decoded = codec.decode_batches(codec.encode_batches((), schema=provided))

    assert decoded.schema == provided
    assert decoded.batches == ()


@pytest.mark.parametrize(
    ("format", "prefix"),
    [
        (DataFormat.CSV, b"id,name\n"),
        (DataFormat.JSONL, b'{"id":1,"name":"ada"}\n'),
    ],
)
def test_text_codec_round_trip_preserves_schema_and_rows(format, prefix):
    provided, _ = schemas()
    batches = (
        pa.RecordBatch.from_arrays([pa.array([1, 2]), pa.array(["ada", None])], schema=provided),
        pa.RecordBatch.from_arrays([pa.array([3]), pa.array(["margaret"])], schema=provided),
    )
    codec = DEFAULT_REGISTRY.get(format)

    encoded = codec.encode_batches(batches)
    decoded = codec.decode_batches(encoded, schema=provided)

    assert encoded.startswith(prefix)
    assert decoded.schema == provided
    assert pa.Table.from_batches(decoded.batches).to_pylist() == pa.Table.from_batches(batches).to_pylist()


@pytest.mark.parametrize("format", [DataFormat.CSV, DataFormat.JSONL])
def test_text_codec_decode_requires_schema(format):
    codec = DEFAULT_REGISTRY.get(format)

    with pytest.raises(ValueError, match="requires an Arrow schema"):
        codec.decode_batches(b"")


def test_lazy_decode_applies_batch_and_row_limits():
    schema = pa.schema([("id", pa.int64())])
    batch = pa.record_batch([[1, 2, 3, 4, 5]], schema=schema)
    codec = DEFAULT_REGISTRY.get(DataFormat.ARROW)
    encoded = codec.encode_batches([batch])

    stream = codec.iter_batches(encoded, batch_size=2, row_limit=4)
    batches = tuple(stream)

    assert stream.schema == schema
    assert [batch.num_rows for batch in batches] == [2, 2]
    assert pa.Table.from_batches(batches).column("id").to_pylist() == [1, 2, 3, 4]


def test_lazy_decode_observes_cancellation():
    schema = pa.schema([("id", pa.int64())])
    codec = DEFAULT_REGISTRY.get(DataFormat.ARROW)
    encoded = codec.encode_batches([pa.record_batch([[1]], schema=schema)])
    stream = codec.iter_batches(encoded)

    stream.cancel()

    with pytest.raises(RuntimeError, match="cancelled"):
        next(stream)
    with pytest.raises(StopIteration):
        next(stream)


def test_lazy_decode_enforces_byte_limit():
    schema = pa.schema([("id", pa.int64())])
    codec = DEFAULT_REGISTRY.get(DataFormat.ARROW)
    encoded = codec.encode_batches([pa.record_batch([[1]], schema=schema)])

    with pytest.raises(ValueError, match="byte limit"):
        next(codec.iter_batches(encoded, byte_limit=1))


def test_lazy_decode_rejects_zero_batch_size():
    schema = pa.schema([("id", pa.int64())])
    codec = DEFAULT_REGISTRY.get(DataFormat.ARROW)
    encoded = codec.encode_batches([pa.record_batch([[1]], schema=schema)])

    with pytest.raises(ValueError, match="batch size"):
        codec.iter_batches(encoded, batch_size=0)
