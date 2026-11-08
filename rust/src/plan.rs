use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use arrow::array::{new_null_array, ArrayRef, RecordBatch, RecordBatchOptions, UInt32Array};
use arrow::compute::{cast_with_options, take, CastOptions};
use arrow::datatypes::{DataType, Schema, SchemaRef};

use crate::error::{InterchangeError, Result};
use crate::{CancellationToken, CodecReader, DecodedStream, StreamOptions, DEFAULT_REGISTRY};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DataFormat {
    Arrow,
    Parquet,
    Csv,
    JsonLines,
    Avro,
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
            "avro" => Ok(Self::Avro),
            value => Err(InterchangeError::UnknownFormat(value.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Arrow => "arrow",
            Self::Parquet => "parquet",
            Self::Csv => "csv",
            Self::JsonLines => "jsonl",
            Self::Avro => "avro",
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
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "safe" => Ok(Self::Safe),
            "lossy" => Ok(Self::Lossy),
            "runtime_checked" => Ok(Self::RuntimeChecked),
            value => Err(InterchangeError::UnknownCastKind(value.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Lossy => "lossy",
            Self::RuntimeChecked => "runtime_checked",
        }
    }

    /// Permissiveness order: runtime-checked casts fail on bad values, lossy casts do not.
    fn rank(self) -> u8 {
        match self {
            Self::Safe => 0,
            Self::RuntimeChecked => 1,
            Self::Lossy => 2,
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

/// A requested field filled with one value instead of a provided field.
#[derive(Clone, Debug)]
pub struct FieldAddition {
    pub target_index: usize,
    /// One value of the requested field type.
    pub value: ArrayRef,
}

/// Explicit field operations applied while planning a projection.
#[derive(Clone, Debug, Default)]
pub struct ProjectionOptions {
    /// Provided field name to requested field name.
    pub renames: HashMap<String, String>,
    /// Requested field name to a one-element array. A null element adds an all-null field.
    pub defaults: HashMap<String, ArrayRef>,
    /// Provided field names to remove.
    pub drop: Vec<String>,
    /// Requested field name to the most permissive cast allowed for that field.
    pub casts: HashMap<String, CastKind>,
}

impl ProjectionOptions {
    pub fn is_empty(&self) -> bool {
        self.renames.is_empty()
            && self.defaults.is_empty()
            && self.drop.is_empty()
            && self.casts.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct InterchangeRequest {
    pub provided_format: DataFormat,
    pub requested_format: DataFormat,
    pub provided_schema: SchemaRef,
    pub requested_schema: SchemaRef,
    pub policy: SchemaPolicy,
    pub options: ProjectionOptions,
}

#[derive(Clone, Debug)]
pub struct InterchangePlan {
    pub provided_format: DataFormat,
    pub requested_format: DataFormat,
    pub provided_schema: SchemaRef,
    pub requested_schema: SchemaRef,
    pub policy: SchemaPolicy,
    pub mappings: Vec<FieldMapping>,
    pub additions: Vec<FieldAddition>,
}

impl InterchangePlan {
    pub fn apply_batch(&self, batch: &RecordBatch) -> Result<RecordBatch> {
        if !schema_fields_equal(batch.schema().as_ref(), self.provided_schema.as_ref()) {
            return Err(InterchangeError::InputSchema);
        }
        let mut columns: Vec<Option<ArrayRef>> = vec![None; self.requested_schema.fields().len()];
        for mapping in &self.mappings {
            let source = batch.column(mapping.source_index);
            if mapping.check_nulls && source.null_count() > 0 {
                return Err(InterchangeError::RuntimeNullability(
                    self.requested_schema
                        .field(mapping.target_index)
                        .name()
                        .clone(),
                ));
            }
            columns[mapping.target_index] = Some(match mapping.cast {
                Some(_) => cast_with_options(
                    source,
                    self.requested_schema
                        .field(mapping.target_index)
                        .data_type(),
                    &CastOptions {
                        safe: false,
                        ..CastOptions::default()
                    },
                )?,
                None => source.clone(),
            });
        }
        if !self.additions.is_empty() {
            let indices = UInt32Array::from(vec![0; batch.num_rows()]);
            for addition in &self.additions {
                columns[addition.target_index] = Some(take(&addition.value, &indices, None)?);
            }
        }
        Ok(RecordBatch::try_new_with_options(
            self.requested_schema.clone(),
            columns.into_iter().map(Option::unwrap).collect(),
            &RecordBatchOptions::new().with_row_count(Some(batch.num_rows())),
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

    /// Decodes `reader` in the provided format and applies this plan to each bounded batch.
    /// CSV and JSONL are decoded with the provided schema.
    pub fn iter_batches(
        &self,
        reader: Box<dyn CodecReader>,
        options: StreamOptions,
        cancellation: CancellationToken,
    ) -> Result<PlannedStream> {
        let decode_schema = matches!(
            self.provided_format,
            DataFormat::Csv | DataFormat::JsonLines
        )
        .then(|| self.provided_schema.clone());
        let source = DEFAULT_REGISTRY.get(self.provided_format)?.decode_reader(
            reader,
            decode_schema,
            options,
            cancellation,
        )?;
        Ok(PlannedStream {
            plan: self.clone(),
            source,
        })
    }

    /// Converts `reader` from the provided format to the requested format one bounded batch
    /// at a time.
    pub fn convert(
        &self,
        reader: Box<dyn CodecReader>,
        output: &mut (dyn Write + Send),
        options: StreamOptions,
    ) -> Result<()> {
        let mut batches = self.iter_batches(reader, options, CancellationToken::new())?;
        DEFAULT_REGISTRY.get(self.requested_format)?.encode_stream(
            self.requested_schema.clone(),
            &mut batches,
            output,
        )
    }
}

/// Decoded batches with an [`InterchangePlan`] applied, in the requested schema.
pub struct PlannedStream {
    plan: InterchangePlan,
    source: DecodedStream,
}

impl PlannedStream {
    pub fn schema(&self) -> SchemaRef {
        self.plan.requested_schema.clone()
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.source.cancellation_token()
    }
}

impl Iterator for PlannedStream {
    type Item = Result<RecordBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        let batch = self.source.next()?;
        Some(batch.and_then(|batch| self.plan.apply_batch(&batch)))
    }
}

pub fn plan(request: &InterchangeRequest) -> Result<InterchangePlan> {
    DEFAULT_REGISTRY.get(request.provided_format)?;
    DEFAULT_REGISTRY.get(request.requested_format)?;
    let mut plan = plan_schema_with(
        request.provided_schema.clone(),
        request.requested_schema.clone(),
        request.policy,
        &request.options,
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
    plan_schema_with(
        provided_schema,
        requested_schema,
        policy,
        &ProjectionOptions::default(),
    )
}

/// Plans a projection with explicit renames, defaults, drops, and per-field casts.
///
/// Without options this matches [`plan_schema`]. With options, fields are matched by
/// name after renames. Every provided field must be used or dropped, except under the
/// projection policy, which drops unused fields implicitly.
pub fn plan_schema_with(
    provided_schema: SchemaRef,
    requested_schema: SchemaRef,
    policy: SchemaPolicy,
    options: &ProjectionOptions,
) -> Result<InterchangePlan> {
    let provided = schema_descriptors(provided_schema.as_ref());
    let requested = schema_descriptors(requested_schema.as_ref());
    let (mappings, additions) = if options.is_empty() {
        (
            plan_field_descriptors(&provided, &requested, policy)?,
            Vec::new(),
        )
    } else {
        plan_operations(&provided, &requested, policy, options)?
    };
    Ok(InterchangePlan {
        provided_format: DataFormat::Arrow,
        requested_format: DataFormat::Arrow,
        provided_schema,
        requested_schema,
        policy,
        mappings,
        additions,
    })
}

fn plan_operations(
    provided: &[FieldDescriptor],
    requested: &[FieldDescriptor],
    policy: SchemaPolicy,
    options: &ProjectionOptions,
) -> Result<(Vec<FieldMapping>, Vec<FieldAddition>)> {
    if policy == SchemaPolicy::Exact {
        return Err(InterchangeError::ExactOperations);
    }
    for name in options.renames.keys().chain(&options.drop) {
        match provided.iter().filter(|field| &field.name == name).count() {
            0 => return Err(InterchangeError::MissingProvidedField(name.clone())),
            1 => {}
            _ => return Err(InterchangeError::AmbiguousField(name.clone())),
        }
    }
    if let Some(name) = options
        .drop
        .iter()
        .find(|name| options.renames.contains_key(*name))
    {
        return Err(InterchangeError::RenameDropConflict(name.clone()));
    }
    let requested_names = [
        ("rename", options.renames.values().collect::<Vec<_>>()),
        ("default", options.defaults.keys().collect()),
        ("cast", options.casts.keys().collect()),
    ];
    for (operation, names) in requested_names {
        if let Some(name) = names
            .into_iter()
            .find(|name| !requested.iter().any(|field| &field.name == *name))
        {
            return Err(InterchangeError::UnknownRequestedField {
                operation,
                field: name.clone(),
            });
        }
    }

    let sources = provided
        .iter()
        .enumerate()
        .filter(|(_, field)| !options.drop.contains(&field.name))
        .map(|(index, field)| {
            let name = options.renames.get(&field.name).unwrap_or(&field.name);
            (index, name)
        })
        .collect::<Vec<_>>();
    let mut used = vec![false; sources.len()];
    let mut mappings = Vec::new();
    let mut additions = Vec::new();
    for (target_index, target) in requested.iter().enumerate() {
        let matches = sources
            .iter()
            .enumerate()
            .filter(|(_, (_, name))| *name == &target.name)
            .map(|(position, _)| position)
            .collect::<Vec<_>>();
        match (matches.as_slice(), options.defaults.get(&target.name)) {
            ([], Some(value)) => additions.push(plan_addition(target_index, target, value)?),
            ([], None) => return Err(InterchangeError::MissingField(target.name.clone())),
            ([position], None) => {
                used[*position] = true;
                let source_index = sources[*position].0;
                mappings.push(plan_mapping(
                    source_index,
                    target_index,
                    &provided[source_index],
                    target,
                    policy,
                    options.casts.get(&target.name).copied(),
                )?);
            }
            ([_], Some(_)) => return Err(InterchangeError::DefaultConflict(target.name.clone())),
            _ => return Err(InterchangeError::AmbiguousField(target.name.clone())),
        }
    }
    if policy != SchemaPolicy::Projection {
        if let Some(position) = used.iter().position(|used| !used) {
            return Err(InterchangeError::UnmappedField(
                provided[sources[position].0].name.clone(),
            ));
        }
    }
    Ok((mappings, additions))
}

fn plan_mapping(
    source_index: usize,
    target_index: usize,
    source: &FieldDescriptor,
    target: &FieldDescriptor,
    policy: SchemaPolicy,
    cast_override: Option<CastKind>,
) -> Result<FieldMapping> {
    let cast = DEFAULT_CAST_REGISTRY.classify(&source.data_type, &target.data_type);
    let allowed = cast_override.or(match policy {
        SchemaPolicy::Compatible => Some(CastKind::Safe),
        SchemaPolicy::Coerce => Some(CastKind::Lossy),
        SchemaPolicy::Exact | SchemaPolicy::Projection => None,
    });
    if source.data_type != target.data_type {
        match (cast, allowed) {
            (_, None) => {
                return Err(InterchangeError::TypeMismatch {
                    field: target.name.clone(),
                    provided: format!("{:?}", source.data_type),
                    requested: format!("{:?}", target.data_type),
                })
            }
            (None, Some(_)) => {
                return Err(InterchangeError::UnsupportedCast {
                    field: target.name.clone(),
                    provided: format!("{:?}", source.data_type),
                    requested: format!("{:?}", target.data_type),
                })
            }
            (Some(kind), Some(max)) if kind.rank() > max.rank() => {
                return Err(InterchangeError::CastNotAllowed {
                    field: target.name.clone(),
                    cast: kind.as_str(),
                    allowed: max.as_str(),
                })
            }
            _ => {}
        }
    }
    let check_nulls = source.nullable && !target.nullable;
    if check_nulls && policy != SchemaPolicy::Coerce {
        return Err(InterchangeError::Nullability(target.name.clone()));
    }
    Ok(FieldMapping {
        source_index,
        target_index,
        cast,
        check_nulls,
    })
}

fn plan_addition(
    target_index: usize,
    target: &FieldDescriptor,
    value: &ArrayRef,
) -> Result<FieldAddition> {
    if value.len() != 1 {
        return Err(InterchangeError::DefaultLength(target.name.clone()));
    }
    let value = if value.is_null(0) {
        if !target.nullable {
            return Err(InterchangeError::NullDefault(target.name.clone()));
        }
        new_null_array(&target.data_type, 1)
    } else {
        cast_with_options(
            value,
            &target.data_type,
            &CastOptions {
                safe: false,
                ..CastOptions::default()
            },
        )
        .map_err(|source| InterchangeError::DefaultCast {
            field: target.name.clone(),
            requested: format!("{:?}", target.data_type),
            source,
        })?
    };
    Ok(FieldAddition {
        target_index,
        value,
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
    use arrow::array::{Array, Float64Array, Int32Array, Int64Array, RecordBatch, StringArray};
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
            options: ProjectionOptions::default(),
        };

        let plan = plan(&request).unwrap();

        assert_eq!(plan.provided_format, DataFormat::Parquet);
        assert_eq!(plan.requested_format, DataFormat::Arrow);
    }

    fn options() -> ProjectionOptions {
        ProjectionOptions::default()
    }

    fn batch(schema: SchemaRef) -> RecordBatch {
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![1, 2])),
                Arc::new(StringArray::from(vec![Some("ada"), None])),
            ],
        )
        .unwrap()
    }

    #[test]
    fn rename_maps_provided_field_to_requested_name() {
        let requested = schema_ref(Schema::new(vec![
            Field::new("customer_id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let mut options = options();
        options.renames.insert("id".into(), "customer_id".into());
        let plan = plan_schema_with(
            provided_schema(),
            requested.clone(),
            SchemaPolicy::Compatible,
            &options,
        )
        .unwrap();

        let result = plan.apply_batch(&batch(provided_schema())).unwrap();

        assert_eq!(result.schema(), requested);
        assert_eq!(plan.mappings[0].source_index, 0);
    }

    #[test]
    fn defaults_add_literal_and_null_fields() {
        let requested = schema_ref(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("region", DataType::Utf8, false),
            Field::new("score", DataType::Float64, true),
        ]));
        let mut options = options();
        options.defaults.insert(
            "region".into(),
            Arc::new(StringArray::from(vec!["us"])) as ArrayRef,
        );
        options
            .defaults
            .insert("score".into(), new_null_array(&DataType::Null, 1));
        let plan = plan_schema_with(
            provided_schema(),
            requested.clone(),
            SchemaPolicy::Coerce,
            &options,
        )
        .unwrap();

        let result = plan.apply_batch(&batch(provided_schema())).unwrap();

        assert_eq!(result.schema(), requested);
        let region = result
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(
            region.iter().collect::<Vec<_>>(),
            vec![Some("us"), Some("us")]
        );
        let score = result
            .column(3)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert_eq!(score.null_count(), 2);
    }

    #[test]
    fn defaults_cast_to_requested_type_at_plan_time() {
        let requested = schema_ref(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("version", DataType::Int32, false),
        ]));
        let mut options = options();
        options.defaults.insert(
            "version".into(),
            Arc::new(Int64Array::from(vec![3])) as ArrayRef,
        );
        let plan = plan_schema_with(
            provided_schema(),
            requested,
            SchemaPolicy::Compatible,
            &options,
        )
        .unwrap();

        assert_eq!(plan.additions[0].target_index, 2);
        assert_eq!(plan.additions[0].value.data_type(), &DataType::Int32);
    }

    #[test]
    fn additions_keep_row_count_without_source_columns() {
        let requested = schema_ref(Schema::new(vec![Field::new(
            "region",
            DataType::Utf8,
            false,
        )]));
        let mut options = options();
        options.defaults.insert(
            "region".into(),
            Arc::new(StringArray::from(vec!["us"])) as ArrayRef,
        );
        let plan = plan_schema_with(
            provided_schema(),
            requested,
            SchemaPolicy::Projection,
            &options,
        )
        .unwrap();

        let result = plan.apply_batch(&batch(provided_schema())).unwrap();

        assert_eq!(result.num_rows(), 2);
    }

    #[test]
    fn non_projection_policies_require_explicit_drops() {
        let requested = schema_ref(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let mut options = options();
        options.casts.insert("id".into(), CastKind::Safe);
        let error = plan_schema_with(
            provided_schema(),
            requested.clone(),
            SchemaPolicy::Compatible,
            &options,
        )
        .unwrap_err();
        assert!(matches!(error, InterchangeError::UnmappedField(name) if name == "name"));

        options.drop.push("name".into());
        let plan = plan_schema_with(
            provided_schema(),
            requested.clone(),
            SchemaPolicy::Compatible,
            &options,
        )
        .unwrap();
        let result = plan.apply_batch(&batch(provided_schema())).unwrap();
        assert_eq!(result.schema(), requested);
    }

    #[test]
    fn cast_override_allows_lossy_cast_under_compatible() {
        let requested = schema_ref(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let mut options = options();
        options.casts.insert("id".into(), CastKind::Lossy);
        let plan = plan_schema_with(
            provided_schema(),
            requested,
            SchemaPolicy::Compatible,
            &options,
        )
        .unwrap();

        assert_eq!(plan.mappings[0].cast, Some(CastKind::Lossy));
    }

    #[test]
    fn cast_override_restricts_coerce() {
        let provided = schema_ref(Schema::new(vec![Field::new("id", DataType::Utf8, false)]));
        let requested = schema_ref(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let mut options = options();
        options.casts.insert("id".into(), CastKind::Safe);
        let error =
            plan_schema_with(provided, requested, SchemaPolicy::Coerce, &options).unwrap_err();

        assert!(error.to_string().contains("runtime_checked cast"));
    }

    #[test]
    fn operations_reject_conflicts_and_unknown_fields() {
        let requested = schema_ref(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let plan_with = |options: &ProjectionOptions, policy| {
            plan_schema_with(provided_schema(), requested.clone(), policy, options).unwrap_err()
        };

        let mut options = options();
        options.renames.insert("id".into(), "id".into());
        assert!(matches!(
            plan_with(&options, SchemaPolicy::Exact),
            InterchangeError::ExactOperations
        ));

        let mut options = self::options();
        options.renames.insert("missing".into(), "id".into());
        assert!(matches!(
            plan_with(&options, SchemaPolicy::Coerce),
            InterchangeError::MissingProvidedField(_)
        ));

        let mut options = self::options();
        options.renames.insert("name".into(), "label".into());
        options.drop.push("name".into());
        assert!(matches!(
            plan_with(&options, SchemaPolicy::Coerce),
            InterchangeError::RenameDropConflict(_)
        ));

        let mut options = self::options();
        options.casts.insert("label".into(), CastKind::Safe);
        assert!(matches!(
            plan_with(&options, SchemaPolicy::Coerce),
            InterchangeError::UnknownRequestedField {
                operation: "cast",
                ..
            }
        ));

        let mut options = self::options();
        options.defaults.insert(
            "name".into(),
            Arc::new(StringArray::from(vec!["x"])) as ArrayRef,
        );
        assert!(matches!(
            plan_with(&options, SchemaPolicy::Coerce),
            InterchangeError::DefaultConflict(_)
        ));
    }

    #[test]
    fn null_default_requires_nullable_field() {
        let requested = schema_ref(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("region", DataType::Utf8, false),
        ]));
        let mut options = options();
        options
            .defaults
            .insert("region".into(), new_null_array(&DataType::Utf8, 1));
        let error = plan_schema_with(provided_schema(), requested, SchemaPolicy::Coerce, &options)
            .unwrap_err();

        assert!(matches!(error, InterchangeError::NullDefault(_)));
    }

    #[test]
    fn default_cast_failure_is_reported_at_plan_time() {
        let requested = schema_ref(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("version", DataType::Int32, false),
        ]));
        let mut options = options();
        options.defaults.insert(
            "version".into(),
            Arc::new(StringArray::from(vec!["three"])) as ArrayRef,
        );
        let error = plan_schema_with(provided_schema(), requested, SchemaPolicy::Coerce, &options)
            .unwrap_err();

        assert!(matches!(error, InterchangeError::DefaultCast { .. }));
    }

    #[test]
    fn plans_convert_referenced_csv_schemas_to_parquet() {
        use std::io::Cursor;

        use crate::{SchemaFormat, SchemaReference, DEFAULT_SCHEMA_RESOLVERS};

        let directory =
            std::env::temp_dir().join(format!("fsspec-data-plan-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let schema_path = directory.join("provided.json");
        std::fs::write(
            &schema_path,
            r#"{"fields": [{"name": "id", "type": "int64", "nullable": false}, {"name": "label", "type": "string"}]}"#,
        )
        .unwrap();
        let provided = DEFAULT_SCHEMA_RESOLVERS
            .resolve(
                &SchemaReference::new(schema_path.display().to_string())
                    .with_format(SchemaFormat::ArrowJson),
            )
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        let requested = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("source", DataType::Utf8, false),
        ]));
        let options = ProjectionOptions {
            renames: HashMap::from([("label".to_string(), "name".to_string())]),
            defaults: HashMap::from([(
                "source".to_string(),
                Arc::new(StringArray::from(vec!["csv"])) as ArrayRef,
            )]),
            ..ProjectionOptions::default()
        };
        let plan = plan(&InterchangeRequest {
            provided_format: DataFormat::Csv,
            requested_format: DataFormat::Parquet,
            provided_schema: provided.schema,
            requested_schema: requested.clone(),
            policy: SchemaPolicy::Compatible,
            options,
        })
        .unwrap();
        let csv = || Box::new(Cursor::new(b"id,label\n1,a\n2,b\n3,c\n".to_vec()));
        let stream_options = StreamOptions {
            batch_size: 2,
            ..StreamOptions::default()
        };

        let stream = plan
            .iter_batches(csv(), stream_options, CancellationToken::new())
            .unwrap();
        assert_eq!(stream.schema(), requested);
        stream.cancellation_token();
        let rows: Vec<usize> = stream.map(|batch| batch.unwrap().num_rows()).collect();
        assert_eq!(rows, [2, 1]);

        let mut parquet = Vec::new();
        plan.convert(csv(), &mut parquet, stream_options).unwrap();
        let decoded = DEFAULT_REGISTRY
            .get(DataFormat::Parquet)
            .unwrap()
            .decode(&parquet, None)
            .unwrap();
        assert_eq!(decoded.schema, requested);
        let batch = &decoded.batches[0];
        assert_eq!(
            batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap(),
            &StringArray::from(vec!["a", "b", "c"])
        );
        assert_eq!(
            batch
                .column(2)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap(),
            &StringArray::from(vec!["csv"; 3])
        );
    }
}
