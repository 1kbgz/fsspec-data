import copy
import math
from dataclasses import FrozenInstanceError

import pytest

from fsspec_data import ScanIdentity, SourceIdentity


def request(source, **changes):
    options = {
        "path": "orders",
        "snapshot": {"namespace_id": "n", "table_id": "t", "generation": 2},
        "columns": ["b", "a", "b"],
        "row_offset": 3,
        "batch_size": 16,
        "row_limit": 8,
        "byte_limit": 4096,
        "filter": {"op": "gt", "column": "a", "value": 1},
        "start_position": {"partition": 0, "offset": 10},
        "backend_options": {"codec": "parquet", "schema": {"b": "int64", "a": "int64"}},
    }
    options.update(changes)
    return ScanIdentity(source, **options)


def test_source_tokens_separate_credential_contexts_and_survive_copy():
    source = SourceIdentity()
    key = request(source)
    assert request(copy.copy(source)) == key
    assert {key: "batch"}[request(source)] == "batch"
    assert request(SourceIdentity()) != key
    assert source != SourceIdentity()
    assert {source, copy.copy(source)} == {source}


@pytest.mark.parametrize(
    "change",
    [
        {"path": "other/orders"},
        {"path": "./orders"},
        {"snapshot": {"namespace_id": "n", "table_id": "t", "generation": 3}},
        {"snapshot": {"namespace_id": "n", "table_id": "replacement", "generation": 2}},
        {"snapshot": {"namespace_id": "other", "table_id": "t", "generation": 2}},
        {"columns": None},
        {"columns": []},
        {"columns": ["a", "b", "b"]},
        {"columns": ["b", "a"]},
        {"row_offset": 4},
        {"batch_size": 32},
        {"row_limit": None},
        {"row_limit": 0},
        {"byte_limit": None},
        {"filter": {"op": "gt", "column": "a", "value": 2}},
        {"start_position": {"partition": 1, "offset": 10}},
        {"backend_options": {"codec": "csv"}},
    ],
)
def test_request_differences_prevent_reuse(change):
    source = SourceIdentity()
    key = request(source)
    assert request(source, **change) not in {key}


def test_nested_object_order_is_canonical_but_arrays_and_types_are_distinct():
    source = SourceIdentity()
    key = request(source)
    assert request(source, backend_options={"schema": {"a": "int64", "b": "int64"}, "codec": "parquet"}) == key
    assert request(source, filter={"value": 1, "column": "a", "op": "gt"}) == key
    values = [None, False, 0, 0.0, "0", [], {}, [1, 2], [2, 1], 1.0, math.nextafter(1.0, 2.0)]
    keys = {request(source, filter=value) for value in values}
    assert len(keys) == len(values)


def test_unpinned_requests_are_distinct_but_copy_preserves_identity():
    source = SourceIdentity()
    first = ScanIdentity(source, "mutable.parquet")
    assert first != ScanIdentity(source, "mutable.parquet")
    assert copy.copy(first) == first


def test_mutating_inputs_does_not_change_stored_identity():
    source = SourceIdentity()
    columns = ["a"]
    snapshot = {"generation": 1}
    predicate = {"values": [1, 2]}
    key = request(source, columns=columns, snapshot=snapshot, filter=predicate)
    keys = {key}
    columns.append("b")
    snapshot["generation"] = 2
    predicate["values"].append(3)
    assert request(source, columns=["a"], snapshot={"generation": 1}, filter={"values": [1, 2]}) in keys
    assert request(source, columns=columns, snapshot=snapshot, filter=predicate) not in keys
    with pytest.raises(FrozenInstanceError):
        key._native = None


def test_repr_hides_paths_predicates_and_provider_options():
    source = SourceIdentity()
    key = request(source, path="s3://user:password@bucket/private", filter={"secret": "value"}, backend_options={"token": "secret"})
    assert repr(source) == "SourceIdentity(...)"
    assert repr(source._native) == "SourceIdentity(...)"
    assert repr(key) == "ScanIdentity(...)"
    assert repr(key._native) == "ScanIdentity(...)"


@pytest.mark.parametrize("value", [float("nan"), float("inf"), -(2**63) - 1, 2**64, {1: "x"}, (1, 2), object(), {"nested": [2**64]}])
def test_invalid_json_cannot_silently_collapse_different_requests(value):
    with pytest.raises((TypeError, ValueError)):
        request(SourceIdentity(), filter=value)


def test_integer_boundaries_are_exact():
    source = SourceIdentity()
    values = [-(2**63), -(2**63) + 1, 2**63 - 1, 2**63, 2**64 - 2, 2**64 - 1]
    assert len({request(source, filter=value) for value in values}) == len(values)


@pytest.mark.parametrize("options", [{"row_offset": -1}, {"batch_size": 0}, {"row_limit": -1}, {"byte_limit": -1}])
def test_invalid_scan_limits_are_rejected(options):
    with pytest.raises((ValueError, OverflowError)):
        request(SourceIdentity(), **options)
