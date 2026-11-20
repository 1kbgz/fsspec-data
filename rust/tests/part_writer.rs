use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::{ArrayRef, Int64Array, RecordBatch, StringArray};
use arrow::compute::concat_batches;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use bytes::Bytes;
use fsspec_data::manifest::Checksum;
use fsspec_data::parts::{EncodedPart, ParquetPartWriter, PartLimits};
use fsspec_data::{InterchangeError, Result};
use fsspec_rs::{ErrorKind, OperationError};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

fn batch(start: i64, rows: usize) -> RecordBatch {
    RecordBatch::try_from_iter([
        (
            "id",
            Arc::new(Int64Array::from_iter_values(start..start + rows as i64)) as ArrayRef,
        ),
        (
            "label",
            Arc::new(StringArray::from_iter((0..rows).map(|i| {
                (i % 3 != 0).then(|| format!("value-{}", start + i as i64))
            }))) as ArrayRef,
        ),
    ])
    .unwrap()
}

fn decode(parts: Vec<EncodedPart>, schema: &SchemaRef) -> RecordBatch {
    let mut batches = Vec::new();
    for part in parts {
        assert_eq!(part.sha256, Checksum::of(&part.bytes));
        assert_eq!(&part.bytes[..4], b"PAR1");
        assert_eq!(&part.bytes[part.bytes.len() - 4..], b"PAR1");
        let builder = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(part.bytes)).unwrap();
        assert_eq!(
            builder.metadata().file_metadata().num_rows() as u64,
            part.num_rows
        );
        assert_eq!(builder.metadata().num_row_groups(), 1);
        assert_eq!(builder.schema(), schema);
        batches.extend(builder.build().unwrap().map(|b| b.unwrap()));
    }
    concat_batches(schema, batches.iter()).unwrap()
}

fn assert_invalid<T>(result: Result<T>) {
    match result {
        Err(InterchangeError::Operation(error)) => {
            assert_eq!(error.kind(), ErrorKind::Invalid, "{error}");
        }
        Err(error) => panic!("expected Invalid, got {error}"),
        Ok(_) => panic!("expected Invalid"),
    }
}

#[test]
fn rotation_preserves_rows_schema_and_checksums_across_batches() {
    let a = batch(0, 5000);
    let b = batch(5000, 3456);
    let limits = PartLimits {
        target_part_bytes: 16 * 1024,
        memory_limit: 8 * 1024 * 1024,
    };
    let mut parts = Vec::new();
    let input_bytes = a.get_array_memory_size().max(b.get_array_memory_size());
    let mut writer = ParquetPartWriter::new(a.schema(), limits, |part| {
        assert!(part.bytes.capacity() + input_bytes <= limits.memory_limit);
        parts.push(part);
        Ok(())
    })
    .unwrap();
    writer.write_batch(&a).unwrap();
    writer.write_batch(&b).unwrap();
    writer.finish().unwrap();
    assert!(parts.len() > 2);
    assert_eq!(
        decode(parts, &a.schema()),
        concat_batches(&a.schema(), [&a, &b]).unwrap()
    );
}

#[test]
fn flush_keeps_encoder_open_and_empty_operations_emit_no_parts() {
    let a = batch(0, 5);
    let empty = a.slice(0, 0);
    let mut parts = Vec::new();
    let mut writer = ParquetPartWriter::new(a.schema(), PartLimits::default(), |part| {
        parts.push(part);
        Ok(())
    })
    .unwrap();
    writer.flush().unwrap();
    writer.write_batch(&empty).unwrap();
    writer.flush().unwrap();
    writer.write_batch(&a).unwrap();
    writer.flush().unwrap();
    writer.flush().unwrap();
    writer.write_batch(&a).unwrap();
    writer.finish().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(
        decode(parts, &a.schema()),
        concat_batches(&a.schema(), [&a, &a]).unwrap()
    );
}

#[test]
fn rejected_input_does_not_discard_previous_rows() {
    let a = batch(0, 5);
    let mut parts = Vec::new();
    let mut writer = ParquetPartWriter::new(
        a.schema(),
        PartLimits {
            target_part_bytes: 32 * 1024,
            memory_limit: 256 * 1024,
        },
        |part| {
            parts.push(part);
            Ok(())
        },
    )
    .unwrap();
    writer.write_batch(&a).unwrap();
    let wrong =
        RecordBatch::try_from_iter([("other", Arc::new(Int64Array::from(vec![1])) as ArrayRef)])
            .unwrap();
    assert!(matches!(
        writer.write_batch(&wrong),
        Err(InterchangeError::InputSchema)
    ));
    // A tiny slice retains the large allocation; logical row size is not the budget.
    let large = batch(0, 100_000);
    assert_invalid(writer.write_batch(&large.slice(0, 1)));
    writer.write_batch(&a).unwrap();
    writer.finish().unwrap();
    assert_eq!(
        decode(parts, &a.schema()),
        concat_batches(&a.schema(), [&a, &a]).unwrap()
    );
}

#[test]
fn dropping_or_aborting_does_not_flush_pending_rows() {
    let a = batch(0, 5);
    let mut emitted = 0;
    {
        let mut writer = ParquetPartWriter::new(a.schema(), PartLimits::default(), |_| {
            emitted += 1;
            Ok(())
        })
        .unwrap();
        writer.write_batch(&a).unwrap();
    }
    assert_eq!(emitted, 0);
    let mut writer = ParquetPartWriter::new(a.schema(), PartLimits::default(), |_| {
        emitted += 1;
        Ok(())
    })
    .unwrap();
    writer.write_batch(&a).unwrap();
    writer.flush().unwrap();
    writer.write_batch(&a).unwrap();
    writer.abort();
    assert_eq!(emitted, 1);
}

#[test]
fn consumer_failure_propagates_and_prevents_later_emission() {
    let a = batch(0, 3000);
    let mut calls = 0;
    let mut accepted_rows = 0;
    let mut writer = ParquetPartWriter::new(
        a.schema(),
        PartLimits {
            target_part_bytes: 1,
            ..PartLimits::default()
        },
        |part| {
            calls += 1;
            if calls == 2 {
                return Err(OperationError::Detailed {
                    kind: ErrorKind::Unavailable,
                    message: "sink unavailable".into(),
                    retry_safe: false,
                }
                .into());
            }
            accepted_rows += part.num_rows;
            Ok(())
        },
    )
    .unwrap();
    match writer.write_batch(&a) {
        Err(InterchangeError::Operation(error)) => assert_eq!(error.kind(), ErrorKind::Unavailable),
        _ => panic!("expected sink failure"),
    }
    assert_invalid(writer.write_batch(&a));
    assert_invalid(writer.flush());
    assert_invalid(writer.finish());
    assert_eq!(calls, 2);
    assert_eq!(accepted_rows, 1024);
}

#[test]
fn tiny_budget_fails_without_handing_off_partial_parquet() {
    let a = batch(0, 1);
    let memory_limit = a.get_array_memory_size() + 16;
    let mut calls = 0;
    let mut writer = ParquetPartWriter::new(
        a.schema(),
        PartLimits {
            target_part_bytes: 1,
            memory_limit,
        },
        |_| {
            calls += 1;
            Ok(())
        },
    )
    .unwrap();
    assert_invalid(writer.write_batch(&a));
    assert_invalid(writer.finish());
    assert_eq!(calls, 0);
}

#[test]
fn oversized_footer_fails_without_emitting_a_truncated_file() {
    let schema = Arc::new(Schema::new_with_metadata(
        vec![Field::new("value", DataType::Int64, false)],
        HashMap::from([("large".into(), "x".repeat(1_000_000))]),
    ));
    let a =
        RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![42]))]).unwrap();
    let mut calls = 0;
    let mut writer = ParquetPartWriter::new(
        schema,
        PartLimits {
            target_part_bytes: 128 * 1024,
            memory_limit: 256 * 1024,
        },
        |_| {
            calls += 1;
            Ok(())
        },
    )
    .unwrap();
    writer.write_batch(&a).unwrap();
    assert_invalid(writer.flush());
    assert_invalid(writer.write_batch(&a));
    assert_invalid(writer.finish());
    assert_eq!(calls, 0);
}

#[test]
fn memory_pressure_rotates_parts_before_the_size_target() {
    let a = batch(0, 256);
    let limits = PartLimits {
        target_part_bytes: 200 * 1024,
        memory_limit: 256 * 1024,
    };
    let mut parts = Vec::new();
    let mut writer = ParquetPartWriter::new(a.schema(), limits, |part| {
        parts.push(part);
        Ok(())
    })
    .unwrap();
    for _ in 0..100 {
        writer.write_batch(&a).unwrap();
    }
    writer.finish().unwrap();
    assert!(parts.len() > 1);
    assert!(parts
        .iter()
        .all(|p| p.bytes.len() < limits.target_part_bytes));
    assert_eq!(
        decode(parts, &a.schema()),
        concat_batches(&a.schema(), std::iter::repeat_n(&a, 100)).unwrap()
    );
}

#[test]
fn invalid_configuration_and_empty_streams() {
    let a = batch(0, 1);
    for limits in [
        PartLimits {
            target_part_bytes: 0,
            memory_limit: 1024,
        },
        PartLimits {
            target_part_bytes: 1024,
            memory_limit: 1024,
        },
        PartLimits {
            target_part_bytes: 1024,
            memory_limit: 0,
        },
    ] {
        assert_invalid(ParquetPartWriter::new(a.schema(), limits, |_| Ok(())));
    }
    assert_invalid(ParquetPartWriter::new(
        Arc::new(Schema::empty()),
        PartLimits::default(),
        |_| Ok(()),
    ));
    ParquetPartWriter::new(a.schema(), PartLimits::default(), |_| {
        panic!("empty stream emitted a part")
    })
    .unwrap()
    .finish()
    .unwrap();
}

#[test]
fn schema_metadata_is_preserved() {
    let schema = Arc::new(Schema::new_with_metadata(
        vec![Field::new("value", DataType::Int64, false)
            .with_metadata(HashMap::from([("unit".into(), "events".into())]))],
        HashMap::from([("source".into(), "test".into())]),
    ));
    let a =
        RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![42]))]).unwrap();
    let mut parts = Vec::new();
    let mut writer = ParquetPartWriter::new(schema.clone(), PartLimits::default(), |part| {
        parts.push(part);
        Ok(())
    })
    .unwrap();
    writer.write_batch(&a).unwrap();
    writer.finish().unwrap();
    assert_eq!(decode(parts, &schema), a);
}
