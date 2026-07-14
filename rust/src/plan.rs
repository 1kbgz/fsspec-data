use std::sync::Arc;

use arrow::array::RecordBatch;
use arrow::compute::{cast_with_options, CastOptions};
use arrow::datatypes::{DataType, Schema, SchemaRef};

use crate::error::{InterchangeError, Result};
use crate::DEFAULT_REGISTRY;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DataFormat {
    Arrow,
    Parquet,
    Csv,
    JsonLines,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaPolicy {
    Exact,
    Projection,
    Compatible,
    Coerce,
}

impl SchemaPolicy {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "exact" => Ok(Self::Exact),
            "projection" => Ok(Self::Projection),
            "compatible" => Ok(Self::Compatible),
            "coerce" => Ok(Self::Coerce),
            value => Err(InterchangeError::UnsupportedPolicy(value.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Projection => "projection",
            Self::Compatible => "compatible",
            Self::Coerce => "coerce",
        }
    }
}

impl DataFormat {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "arrow" => Ok(Self::Arrow),
            "parquet" => Ok(Self::Parquet),
            "csv" => Ok(Self::Csv),
            "jsonl" => Ok(Self::JsonLines),
            value => Err(InterchangeError::UnknownFormat(value.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Arrow => "arrow",
            Self::Parquet => "parquet",
            Self::Csv => "csv",
            Self::JsonLines => "jsonl",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldDescriptor {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CastKind {
    Safe,
    Lossy,
    RuntimeChecked,
}

impl CastKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Lossy => "lossy",
            Self::RuntimeChecked => "runtime_checked",
        }
    }
}

pub struct CastRegistry;

pub static DEFAULT_CAST_REGISTRY: CastRegistry = CastRegistry;

impl CastRegistry {
    pub fn classify(&self, source: &DataType, target: &DataType) -> Option<CastKind> {
        use DataType::*;

        if source == target {
            return None;
        }
        if source == &Null {
            return Some(CastKind::Safe);
        }
        if let (Some(source), Some(target)) = (signed_rank(source), signed_rank(target)) {
            return Some(if source < target {
                CastKind::Safe
            } else {
                CastKind::Lossy
            });
        }
        if let (Some(source), Some(target)) = (unsigned_rank(source), unsigned_rank(target)) {
            return Some(if source < target {
                CastKind::Safe
            } else {
                CastKind::Lossy
            });
        }
        if let (Some(source), Some(target)) = (unsigned_rank(source), signed_rank(target)) {
            return Some(if source < target {
                CastKind::Safe
            } else {
                CastKind::Lossy
            });
        }
        match (source, target) {
            (Float16, Float32 | Float64)
            | (Float32, Float64)
            | (Utf8, LargeUtf8)
            | (Date32, Date64) => Some(CastKind::Safe),
            (Float32 | Float64, Float16) | (Float64, Float32) | (Date64, Date32) => {
                Some(CastKind::Lossy)
            }
            (source, target) if is_numeric(source) && is_numeric(target) => Some(CastKind::Lossy),
            (source, Utf8 | LargeUtf8) if is_numeric(source) || source == &Boolean => {
                Some(CastKind::RuntimeChecked)
            }
            (Utf8 | LargeUtf8, target) if is_numeric(target) || target == &Boolean => {
                Some(CastKind::RuntimeChecked)
            }
            (LargeUtf8, Utf8) => Some(CastKind::RuntimeChecked),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldMapping {
    pub source_index: usize,
    pub target_index: usize,
    pub cast: Option<CastKind>,
    pub check_nulls: bool,
}

#[derive(Clone, Debug)]
pub struct InterchangeRequest {
    pub provided_format: DataFormat,
    pub requested_format: DataFormat,
    pub provided_schema: SchemaRef,
    pub requested_schema: SchemaRef,
    pub policy: SchemaPolicy,
}

#[derive(Clone, Debug)]
pub struct InterchangePlan {
    pub provided_format: DataFormat,
    pub requested_format: DataFormat,
    pub provided_schema: SchemaRef,
    pub requested_schema: SchemaRef,
    pub policy: SchemaPolicy,
    pub mappings: Vec<FieldMapping>,
}

impl InterchangePlan {
    pub fn apply_batch(&self, batch: &RecordBatch) -> Result<RecordBatch> {
        if !schema_fields_equal(batch.schema().as_ref(), self.provided_schema.as_ref()) {
            return Err(InterchangeError::InputSchema);
        }
        let columns = self
            .mappings
            .iter()
            .map(|mapping| {
                let source = batch.column(mapping.source_index);
                if mapping.check_nulls && source.null_count() > 0 {
                    return Err(InterchangeError::RuntimeNullability(
                        self.requested_schema
                            .field(mapping.target_index)
                            .name()
                            .clone(),
                    ));
                }
                match mapping.cast {
                    Some(_) => Ok(cast_with_options(
                        source,
                        self.requested_schema
                            .field(mapping.target_index)
                            .data_type(),
                        &CastOptions {
                            safe: false,
                            ..CastOptions::default()
                        },
                    )?),
                    None => Ok(source.clone()),
                }
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(RecordBatch::try_new(
            self.requested_schema.clone(),
            columns,
        )?)
    }

    pub fn apply_stream<'a, I>(
        &'a self,
        batches: I,
    ) -> impl Iterator<Item = Result<RecordBatch>> + 'a
    where
        I: IntoIterator<Item = Result<RecordBatch>> + 'a,
        I::IntoIter: 'a,
    {
        batches
            .into_iter()
            .map(|batch| batch.and_then(|batch| self.apply_batch(&batch)))
    }
}

pub fn plan(request: &InterchangeRequest) -> Result<InterchangePlan> {
    DEFAULT_REGISTRY.get(request.provided_format)?;
    DEFAULT_REGISTRY.get(request.requested_format)?;
    let mut plan = plan_schema(
        request.provided_schema.clone(),
        request.requested_schema.clone(),
        request.policy,
    )?;
    plan.provided_format = request.provided_format;
    plan.requested_format = request.requested_format;
    Ok(plan)
}

pub fn plan_schema(
    provided_schema: SchemaRef,
    requested_schema: SchemaRef,
    policy: SchemaPolicy,
) -> Result<InterchangePlan> {
    let provided = schema_descriptors(provided_schema.as_ref());
    let requested = schema_descriptors(requested_schema.as_ref());
    let mappings = plan_field_descriptors(&provided, &requested, policy)?;
    Ok(InterchangePlan {
        provided_format: DataFormat::Arrow,
        requested_format: DataFormat::Arrow,
        provided_schema,
        requested_schema,
        policy,
        mappings,
    })
}

pub fn plan_field_descriptors(
    provided: &[FieldDescriptor],
    requested: &[FieldDescriptor],
    policy: SchemaPolicy,
) -> Result<Vec<FieldMapping>> {
    match policy {
        SchemaPolicy::Exact => plan_exact(provided, requested),
        SchemaPolicy::Projection => plan_projection(provided, requested),
        SchemaPolicy::Compatible => plan_compatible(provided, requested),
        SchemaPolicy::Coerce => plan_coerce(provided, requested),
    }
}

fn plan_exact(
    provided: &[FieldDescriptor],
    requested: &[FieldDescriptor],
) -> Result<Vec<FieldMapping>> {
    if provided.len() != requested.len() {
        return Err(InterchangeError::FieldCount {
            provided: provided.len(),
            requested: requested.len(),
        });
    }
    provided
        .iter()
        .zip(requested)
        .enumerate()
        .map(|(index, (source, target))| {
            if source.name != target.name {
                return Err(InterchangeError::FieldName {
                    index,
                    provided: source.name.clone(),
                    requested: target.name.clone(),
                });
            }
            validate_field(source, target)?;
            if source.nullable != target.nullable {
                return Err(InterchangeError::ExactNullability {
                    field: target.name.clone(),
                    provided: source.nullable,
                    requested: target.nullable,
                });
            }
            Ok(FieldMapping {
                source_index: index,
                target_index: index,
                cast: None,
                check_nulls: false,
            })
        })
        .collect()
}

fn plan_projection(
    provided: &[FieldDescriptor],
    requested: &[FieldDescriptor],
) -> Result<Vec<FieldMapping>> {
    requested
        .iter()
        .enumerate()
        .map(|(target_index, target)| {
            let matches = provided
                .iter()
                .enumerate()
                .filter(|(_, source)| source.name == target.name)
                .collect::<Vec<_>>();
            let (source_index, source) = match matches.as_slice() {
                [] => return Err(InterchangeError::MissingField(target.name.clone())),
                [entry] => *entry,
                _ => return Err(InterchangeError::AmbiguousField(target.name.clone())),
            };
            validate_field(source, target)?;
            Ok(FieldMapping {
                source_index,
                target_index,
                cast: None,
                check_nulls: false,
            })
        })
        .collect()
}

fn validate_field(source: &FieldDescriptor, target: &FieldDescriptor) -> Result<()> {
    if source.data_type != target.data_type {
        return Err(InterchangeError::TypeMismatch {
            field: target.name.clone(),
            provided: format!("{:?}", source.data_type),
            requested: format!("{:?}", target.data_type),
        });
    }
    if source.nullable && !target.nullable {
        return Err(InterchangeError::Nullability(target.name.clone()));
    }
    Ok(())
}

fn plan_compatible(
    provided: &[FieldDescriptor],
    requested: &[FieldDescriptor],
) -> Result<Vec<FieldMapping>> {
    plan_positional(provided, requested, |index, source, target| {
        validate_nullability(source, target, false)?;
        let cast = DEFAULT_CAST_REGISTRY.classify(&source.data_type, &target.data_type);
        if source.data_type != target.data_type && cast != Some(CastKind::Safe) {
            return Err(InterchangeError::UnsafeCast {
                field: target.name.clone(),
                provided: format!("{:?}", source.data_type),
                requested: format!("{:?}", target.data_type),
            });
        }
        Ok(FieldMapping {
            source_index: index,
            target_index: index,
            cast,
            check_nulls: false,
        })
    })
}

fn plan_coerce(
    provided: &[FieldDescriptor],
    requested: &[FieldDescriptor],
) -> Result<Vec<FieldMapping>> {
    plan_positional(provided, requested, |index, source, target| {
        let cast = DEFAULT_CAST_REGISTRY.classify(&source.data_type, &target.data_type);
        if source.data_type != target.data_type && cast.is_none() {
            return Err(InterchangeError::UnsupportedCast {
                field: target.name.clone(),
                provided: format!("{:?}", source.data_type),
                requested: format!("{:?}", target.data_type),
            });
        }
        Ok(FieldMapping {
            source_index: index,
            target_index: index,
            cast,
            check_nulls: source.nullable && !target.nullable,
        })
    })
}

fn plan_positional<F>(
    provided: &[FieldDescriptor],
    requested: &[FieldDescriptor],
    mut mapping: F,
) -> Result<Vec<FieldMapping>>
where
    F: FnMut(usize, &FieldDescriptor, &FieldDescriptor) -> Result<FieldMapping>,
{
    if provided.len() != requested.len() {
        return Err(InterchangeError::FieldCount {
            provided: provided.len(),
            requested: requested.len(),
        });
    }
    provided
        .iter()
        .zip(requested)
        .enumerate()
        .map(|(index, (source, target))| {
            if source.name != target.name {
                return Err(InterchangeError::FieldName {
                    index,
                    provided: source.name.clone(),
                    requested: target.name.clone(),
                });
            }
            mapping(index, source, target)
        })
        .collect()
}

fn validate_nullability(
    source: &FieldDescriptor,
    target: &FieldDescriptor,
    allow_runtime_check: bool,
) -> Result<()> {
    if source.nullable && !target.nullable && !allow_runtime_check {
        return Err(InterchangeError::Nullability(target.name.clone()));
    }
    Ok(())
}

fn schema_descriptors(schema: &Schema) -> Vec<FieldDescriptor> {
    schema
        .fields()
        .iter()
        .map(|field| FieldDescriptor {
            name: field.name().clone(),
            data_type: field.data_type().clone(),
            nullable: field.is_nullable(),
        })
        .collect()
}

fn signed_rank(data_type: &DataType) -> Option<u8> {
    match data_type {
        DataType::Int8 => Some(0),
        DataType::Int16 => Some(1),
        DataType::Int32 => Some(2),
        DataType::Int64 => Some(3),
        _ => None,
    }
}

fn unsigned_rank(data_type: &DataType) -> Option<u8> {
    match data_type {
        DataType::UInt8 => Some(0),
        DataType::UInt16 => Some(1),
        DataType::UInt32 => Some(2),
        DataType::UInt64 => Some(3),
        _ => None,
    }
}

fn is_numeric(data_type: &DataType) -> bool {
    signed_rank(data_type).is_some()
        || unsigned_rank(data_type).is_some()
        || matches!(
            data_type,
            DataType::Float16 | DataType::Float32 | DataType::Float64
        )
}

fn schema_fields_equal(left: &Schema, right: &Schema) -> bool {
    schema_descriptors(left) == schema_descriptors(right)
}

pub fn schema_ref(schema: Schema) -> SchemaRef {
    Arc::new(schema)
}

#[cfg(test)]
mod tests {
    use arrow::array::{Int32Array, Int64Array, RecordBatch, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};

    use super::*;

    fn provided_schema() -> SchemaRef {
        schema_ref(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
        ]))
    }

    #[test]
    fn exact_schema_maps_fields_in_place() {
        let schema = provided_schema();
        let plan = plan_schema(schema.clone(), schema, SchemaPolicy::Exact).unwrap();

        assert_eq!(
            plan.mappings,
            vec![
                FieldMapping {
                    source_index: 0,
                    target_index: 0,
                    cast: None,
                    check_nulls: false,
                },
                FieldMapping {
                    source_index: 1,
                    target_index: 1,
                    cast: None,
                    check_nulls: false,
                }
            ]
        );
    }

    #[test]
    fn projection_selects_and_reorders_fields() {
        let requested = schema_ref(Schema::new(vec![Field::new("name", DataType::Utf8, true)]));
        let plan = plan_schema(provided_schema(), requested, SchemaPolicy::Projection).unwrap();

        assert_eq!(
            plan.mappings,
            vec![FieldMapping {
                source_index: 1,
                target_index: 0,
                cast: None,
                check_nulls: false,
            }]
        );
    }

    #[test]
    fn projection_rejects_nullable_narrowing() {
        let requested = schema_ref(Schema::new(vec![Field::new("name", DataType::Utf8, false)]));
        let error = plan_schema(provided_schema(), requested, SchemaPolicy::Projection)
            .unwrap_err()
            .to_string();

        assert!(error.contains("cannot narrow nullable"));
    }

    #[test]
    fn exact_rejects_nullability_widening() {
        let requested = schema_ref(Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("name", DataType::Utf8, true),
        ]));
        let error = plan_schema(provided_schema(), requested, SchemaPolicy::Exact)
            .unwrap_err()
            .to_string();

        assert!(error.contains("nullability mismatch"));
    }

    #[test]
    fn projection_applies_to_record_batch() {
        let provided = provided_schema();
        let requested = schema_ref(Schema::new(vec![Field::new("name", DataType::Utf8, true)]));
        let plan = plan_schema(provided.clone(), requested, SchemaPolicy::Projection).unwrap();
        let batch = RecordBatch::try_new(
            provided,
            vec![
                Arc::new(Int64Array::from(vec![1, 2])),
                Arc::new(StringArray::from(vec!["ada", "grace"])),
            ],
        )
        .unwrap();

        let projected = plan.apply_batch(&batch).unwrap();

        assert_eq!(projected.num_columns(), 1);
        assert_eq!(projected.schema().field(0).name(), "name");
    }

    #[test]
    fn compatible_widens_types_and_nullability() {
        let provided = schema_ref(Schema::new(vec![Field::new("id", DataType::Int32, false)]));
        let requested = schema_ref(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let plan = plan_schema(
            provided.clone(),
            requested.clone(),
            SchemaPolicy::Compatible,
        )
        .unwrap();
        let batch =
            RecordBatch::try_new(provided, vec![Arc::new(Int32Array::from(vec![1, 2]))]).unwrap();

        let result = plan.apply_batch(&batch).unwrap();

        assert_eq!(plan.mappings[0].cast, Some(CastKind::Safe));
        assert_eq!(result.schema(), requested);
        assert_eq!(
            result
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .values(),
            &[1, 2]
        );
    }

    #[test]
    fn compatible_rejects_lossy_cast() {
        let provided = schema_ref(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let requested = schema_ref(Schema::new(vec![Field::new("id", DataType::Int32, false)]));
        let error = plan_schema(provided, requested, SchemaPolicy::Compatible)
            .unwrap_err()
            .to_string();

        assert!(error.contains("unsafe cast"));
    }

    #[test]
    fn coerce_applies_runtime_checked_cast() {
        let provided = schema_ref(Schema::new(vec![Field::new("id", DataType::Utf8, false)]));
        let requested = schema_ref(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let plan = plan_schema(provided.clone(), requested, SchemaPolicy::Coerce).unwrap();
        let batch =
            RecordBatch::try_new(provided, vec![Arc::new(StringArray::from(vec!["1", "2"]))])
                .unwrap();

        let result = plan.apply_batch(&batch).unwrap();

        assert_eq!(plan.mappings[0].cast, Some(CastKind::RuntimeChecked));
        assert_eq!(
            result
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .values(),
            &[1, 2]
        );
    }

    #[test]
    fn coerce_checks_nullable_to_required_at_runtime() {
        let provided = schema_ref(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let requested = schema_ref(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let plan = plan_schema(provided.clone(), requested, SchemaPolicy::Coerce).unwrap();
        let batch = RecordBatch::try_new(
            provided,
            vec![Arc::new(Int64Array::from(vec![Some(1), None]))],
        )
        .unwrap();
        let error = plan.apply_batch(&batch).unwrap_err().to_string();

        assert!(error.contains("contains nulls"));
    }

    #[test]
    fn registered_format_path_is_planned() {
        let request = InterchangeRequest {
            provided_format: DataFormat::Parquet,
            requested_format: DataFormat::Arrow,
            provided_schema: provided_schema(),
            requested_schema: provided_schema(),
            policy: SchemaPolicy::Exact,
        };

        let plan = plan(&request).unwrap();

        assert_eq!(plan.provided_format, DataFormat::Parquet);
        assert_eq!(plan.requested_format, DataFormat::Arrow);
    }
}
