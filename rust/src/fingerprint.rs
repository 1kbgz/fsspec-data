//! Version-one logical batch fingerprints for the sink's flat scalar types.

use arrow::array::*;
use arrow::datatypes::{DataType, Schema, TimeUnit};
use fsspec_rs::{ErrorKind, OperationError};
use sha2::{Digest, Sha256};

use crate::manifest::Checksum;
use crate::sink::error;

fn bytes(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_le_bytes());
    hash.update(value);
}

fn metadata(hash: &mut Sha256, values: &arrow_schema::Metadata) {
    let mut entries: Vec<_> = values.iter().collect();
    entries.sort_unstable();
    hash.update((entries.len() as u64).to_le_bytes());
    for (key, value) in entries {
        bytes(hash, key.as_bytes());
        bytes(hash, value.as_bytes());
    }
}

pub(crate) fn validate_schema(schema: &Schema) -> Result<(), OperationError> {
    for field in schema.fields() {
        match field.data_type() {
            DataType::Boolean
            | DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
            | DataType::Float32
            | DataType::Float64
            | DataType::Utf8
            | DataType::LargeUtf8
            | DataType::Binary
            | DataType::LargeBinary
            | DataType::Date32
            | DataType::Date64
            | DataType::Timestamp(_, _)
            | DataType::Time32(TimeUnit::Second | TimeUnit::Millisecond)
            | DataType::Time64(TimeUnit::Microsecond | TimeUnit::Nanosecond)
            | DataType::Duration(_)
            | DataType::Decimal128(_, _) => {}
            other => {
                return Err(error(
                    ErrorKind::NotSupported,
                    format!("unsupported sink type: {other}"),
                ))
            }
        }
    }
    Ok(())
}

pub(crate) fn fingerprint(batch: &RecordBatch, position: u64) -> Result<Checksum, OperationError> {
    validate_schema(batch.schema().as_ref())?;
    let mut hash = Sha256::new();
    hash.update(b"fsspec-data/batch/1\0");
    hash.update(position.to_le_bytes());
    bytes(&mut hash, b"rows");
    hash.update((batch.num_rows() as u64).to_le_bytes());
    hash.update((batch.num_columns() as u64).to_le_bytes());
    metadata(&mut hash, batch.schema().metadata());
    for field in batch.schema().fields() {
        bytes(&mut hash, field.name().as_bytes());
        // These scalar type encodings contain no unordered maps.
        bytes(
            &mut hash,
            &serde_json::to_vec(field.data_type())
                .map_err(|e| error(ErrorKind::Invalid, e.to_string()))?,
        );
        hash.update([u8::from(field.is_nullable())]);
        metadata(&mut hash, field.metadata());
    }
    for array in batch.columns() {
        for row in 0..batch.num_rows() {
            hash.update([u8::from(array.is_valid(row))]);
            if array.is_null(row) {
                continue;
            }
            macro_rules! scalar {
                ($ty:ty) => {
                    hash.update(
                        array
                            .as_any()
                            .downcast_ref::<$ty>()
                            .unwrap()
                            .value(row)
                            .to_le_bytes(),
                    )
                };
            }
            macro_rules! variable {
                ($ty:ty) => {
                    bytes(
                        &mut hash,
                        array
                            .as_any()
                            .downcast_ref::<$ty>()
                            .unwrap()
                            .value(row)
                            .as_ref(),
                    )
                };
            }
            match array.data_type() {
                DataType::Boolean => hash.update([u8::from(
                    array
                        .as_any()
                        .downcast_ref::<BooleanArray>()
                        .unwrap()
                        .value(row),
                )]),
                DataType::Int8 => scalar!(Int8Array),
                DataType::Int16 => scalar!(Int16Array),
                DataType::Int32 => scalar!(Int32Array),
                DataType::Int64 => scalar!(Int64Array),
                DataType::UInt8 => scalar!(UInt8Array),
                DataType::UInt16 => scalar!(UInt16Array),
                DataType::UInt32 => scalar!(UInt32Array),
                DataType::UInt64 => scalar!(UInt64Array),
                DataType::Float32 => scalar!(Float32Array),
                DataType::Float64 => scalar!(Float64Array),
                DataType::Date32 => scalar!(Date32Array),
                DataType::Date64 => scalar!(Date64Array),
                DataType::Decimal128(_, _) => scalar!(Decimal128Array),
                DataType::Timestamp(TimeUnit::Second, _) => scalar!(TimestampSecondArray),
                DataType::Timestamp(TimeUnit::Millisecond, _) => scalar!(TimestampMillisecondArray),
                DataType::Timestamp(TimeUnit::Microsecond, _) => scalar!(TimestampMicrosecondArray),
                DataType::Timestamp(TimeUnit::Nanosecond, _) => scalar!(TimestampNanosecondArray),
                DataType::Time32(TimeUnit::Second) => scalar!(Time32SecondArray),
                DataType::Time32(TimeUnit::Millisecond) => scalar!(Time32MillisecondArray),
                DataType::Time64(TimeUnit::Microsecond) => scalar!(Time64MicrosecondArray),
                DataType::Time64(TimeUnit::Nanosecond) => scalar!(Time64NanosecondArray),
                DataType::Duration(TimeUnit::Second) => scalar!(DurationSecondArray),
                DataType::Duration(TimeUnit::Millisecond) => scalar!(DurationMillisecondArray),
                DataType::Duration(TimeUnit::Microsecond) => scalar!(DurationMicrosecondArray),
                DataType::Duration(TimeUnit::Nanosecond) => scalar!(DurationNanosecondArray),
                DataType::Utf8 => variable!(StringArray),
                DataType::LargeUtf8 => variable!(LargeStringArray),
                DataType::Binary => variable!(BinaryArray),
                DataType::LargeBinary => variable!(LargeBinaryArray),
                _ => unreachable!("validated scalar type"),
            }
        }
    }
    Ok(Checksum(hash.finalize().into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::buffer::{NullBuffer, ScalarBuffer};
    use arrow::datatypes::{Field, Int32Type};
    use serde_json::Value;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn fixture(case: &Value) -> RecordBatch {
        let mut fields = Vec::new();
        let mut arrays: Vec<ArrayRef> = Vec::new();
        for column in case["columns"].as_array().unwrap() {
            let dtype: DataType = serde_json::from_value(column["data_type"].clone()).unwrap();
            let values = column["values"].as_array().unwrap();
            let array: ArrayRef = match &dtype {
                DataType::Int64 => {
                    Arc::new(Int64Array::from_iter(values.iter().map(Value::as_i64)))
                }
                DataType::Boolean => {
                    Arc::new(BooleanArray::from_iter(values.iter().map(Value::as_bool)))
                }
                DataType::Utf8 => {
                    Arc::new(StringArray::from_iter(values.iter().map(Value::as_str)))
                }
                DataType::Float64 => Arc::new(Float64Array::from_iter(values.iter().map(|v| {
                    v.as_str()
                        .map(|s| f64::from_bits(u64::from_str_radix(s, 16).unwrap()))
                }))),
                DataType::Binary => {
                    let decoded: Vec<Option<Vec<u8>>> = values
                        .iter()
                        .map(|v| {
                            v.as_str().map(|s| {
                                (0..s.len())
                                    .step_by(2)
                                    .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                                    .collect()
                            })
                        })
                        .collect();
                    Arc::new(BinaryArray::from_iter(decoded.iter().map(|v| v.as_deref())))
                }
                DataType::Timestamp(TimeUnit::Microsecond, zone) => Arc::new(
                    TimestampMicrosecondArray::from_iter(values.iter().map(Value::as_i64))
                        .with_timezone_opt(zone.clone()),
                ),
                DataType::Decimal128(p, s) => Arc::new(
                    Decimal128Array::from_iter(values.iter().map(|v| v.as_i64().map(i128::from)))
                        .with_precision_and_scale(*p, *s)
                        .unwrap(),
                ),
                _ => panic!("fixture type"),
            };
            let metadata: HashMap<String, String> =
                serde_json::from_value(column["metadata"].clone()).unwrap();
            fields.push(
                Field::new(
                    column["name"].as_str().unwrap(),
                    dtype,
                    column["nullable"].as_bool().unwrap(),
                )
                .with_metadata(metadata),
            );
            arrays.push(array);
        }
        let metadata: HashMap<String, String> =
            serde_json::from_value(case["metadata"].clone()).unwrap();
        RecordBatch::try_new(
            Arc::new(Schema::new_with_metadata(fields, metadata)),
            arrays,
        )
        .unwrap()
    }

    #[test]
    fn matches_independent_python_fixtures() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/batch_fingerprints.json"))
                .unwrap();
        for case in &cases {
            let batch = fixture(case);
            assert_eq!(
                fingerprint(&batch, case["position"].as_u64().unwrap())
                    .unwrap()
                    .to_string(),
                case["sha256"].as_str().unwrap(),
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn offsets_null_backing_values_and_metadata_order_do_not_change_identity() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/batch_fingerprints.json"))
                .unwrap();
        let original = fixture(&cases[0]);
        let integer = Int64Array::new(
            ScalarBuffer::from(vec![9999, 123, 777, -1, 9999]),
            Some(NullBuffer::from(vec![true, true, false, true, true])),
        )
        .slice(1, 3);
        let text = StringArray::from(vec![
            Some("unused"),
            Some("é"),
            Some(""),
            None,
            Some("unused"),
        ])
        .slice(1, 3);
        let flags =
            BooleanArray::from(vec![Some(false), Some(true), Some(false), None]).slice(1, 3);
        let changed = RecordBatch::try_new(
            original.schema(),
            vec![Arc::new(integer), Arc::new(text), Arc::new(flags)],
        )
        .unwrap();
        assert_eq!(
            fingerprint(&original, 7).unwrap(),
            fingerprint(&changed, 7).unwrap()
        );
        assert_ne!(
            fingerprint(&original, 7).unwrap(),
            fingerprint(&changed, 8).unwrap()
        );
    }

    #[test]
    fn floating_point_bits_remain_distinct_and_dictionary_is_unsupported() {
        let scalar = |bits| {
            RecordBatch::try_from_iter([(
                "value",
                Arc::new(Float64Array::from(vec![f64::from_bits(bits)])) as ArrayRef,
            )])
            .unwrap()
        };
        assert_ne!(
            fingerprint(&scalar(0), 0).unwrap(),
            fingerprint(&scalar(1 << 63), 0).unwrap()
        );
        assert_ne!(
            fingerprint(&scalar(0x7ff8000000000001), 0).unwrap(),
            fingerprint(&scalar(0x7ff8000000000002), 0).unwrap()
        );
        let mut dictionary = StringDictionaryBuilder::<Int32Type>::new();
        dictionary.append("value").unwrap();
        let batch =
            RecordBatch::try_from_iter([("value", Arc::new(dictionary.finish()) as ArrayRef)])
                .unwrap();
        assert_eq!(
            fingerprint(&batch, 0).unwrap_err().kind(),
            ErrorKind::NotSupported
        );
    }
}
