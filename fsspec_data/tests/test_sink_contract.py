from typing import TypeVar

import pyarrow as pa
import pytest

from fsspec_data import BatchReceipt, DataSink, DataWriter, ManifestTable, PublicationReceipt, RecoveryToken, SinkOptions, TableWriter
from fsspec_data.tests.test_tables import s3_store  # noqa: F401

_Result = TypeVar("_Result")
_Options = TypeVar("_Options")


def commit_batch(sink: DataSink[_Options, DataWriter[object, _Result, object]], batch: pa.RecordBatch, options: _Options | None = None) -> _Result:
    with sink.open_writer(batch.schema, options=options) as writer:
        assert writer.progress_unit == "rows"
        assert writer.schema.equals(batch.schema)
        writer.write_batch(batch, expected_position=0, key="first")
        writer.flush()
        return writer.commit()


def test_manifest_table_is_a_structural_sink_with_typed_receipts(tmp_path):
    table = ManifestTable(tmp_path)
    sink: DataSink[SinkOptions, TableWriter] = table
    assert isinstance(sink, DataSink)
    batch = pa.record_batch({"value": [1, 2]})
    receipt: PublicationReceipt = commit_batch(sink, batch, SinkOptions())
    assert receipt.progress == 2 and not receipt.complete
    assert receipt.recovery.root == str(tmp_path)
    assert table.snapshot().num_rows == 2
    assert next(table.snapshot().scan()).equals(batch)


def test_generic_writer_preserves_retry_commit_finish_and_abort(tmp_path):
    table = ManifestTable(tmp_path)
    batch = pa.record_batch({"value": [1, 2]})
    writer: DataWriter[BatchReceipt, PublicationReceipt, RecoveryToken] = table.open_writer(batch.schema)
    assert isinstance(writer, DataWriter)
    receipt = writer.write_batch(batch, expected_position=0, key="first")
    assert writer.write_batch(batch, expected_position=0, key="first") == receipt
    token = writer.recovery_token
    assert token.generation == 0
    assert writer.position == 2
    for position, key, expected in [(3, "gap", "gap"), (0, "other", "conflict")]:
        with pytest.raises(OSError) as error:
            writer.write_batch(batch, expected_position=position, key=key)
        assert getattr(error.value, "kind", None) == expected
    writer.flush()
    with pytest.raises(FileNotFoundError):
        table.snapshot()
    assert writer.commit().progress == 2
    with pytest.raises(ValueError, match="no unresolved publication"):
        writer.resolve()
    assert writer.recovery_token != token
    writer.write_batch(batch, expected_position=2, key="second")
    writer.close()
    assert table.snapshot().num_rows == 2 and not table.snapshot().complete
    with table.open_writer(batch.schema, mode="append") as resumed:
        assert resumed.write_batch(batch, expected_position=0, key="first") == receipt
        resumed.write_batch(batch, expected_position=2, key="second")
        assert resumed.finish().complete
    assert table.snapshot().num_rows == 4


def test_protocol_context_aborts_on_exception_and_clean_exit_does_not_finish(tmp_path):
    sink: DataSink[SinkOptions, DataWriter[BatchReceipt, PublicationReceipt, RecoveryToken]] = ManifestTable(tmp_path)
    batch = pa.record_batch({"value": [1]})
    with pytest.raises(RuntimeError, match="stop"), sink.open_writer(batch.schema) as writer:
        writer.write_batch(batch, expected_position=0, key="first")
        writer.flush()
        raise RuntimeError("stop")
    with pytest.raises(FileNotFoundError):
        ManifestTable(tmp_path).snapshot()
    with sink.open_writer(batch.schema) as writer:
        writer.write_batch(batch, expected_position=0, key="first")
    assert not ManifestTable(tmp_path).snapshot().complete


def test_generic_writer_keeps_recovery_token_after_uncertain_publication(s3_store):  # noqa: F811
    table, state, _ = s3_store
    batch = pa.record_batch({"value": [1, 2]})
    writer: DataWriter[BatchReceipt, PublicationReceipt, RecoveryToken] = table.open_writer(batch.schema)
    token = writer.recovery_token
    writer.write_batch(batch, expected_position=0, key="first")
    state["lose_ack"] = True
    with pytest.raises(OSError) as error:
        writer.commit()
    assert getattr(error.value, "kind", None) == "unknown_outcome"
    assert getattr(error.value, "recovery_token", None) == token == writer.recovery_token
    state.update(lose_ack=False, hide_manifest=False)
    receipt = writer.resolve()
    assert receipt.recovery == token and receipt.progress == 2
    assert writer.recovery_token != token
    writer.close()
    assert next(table.snapshot().scan()).equals(batch)
