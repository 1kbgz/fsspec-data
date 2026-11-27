//! Checksum-verified Parquet scans of immutable table snapshots.
use crate::manifest::Snapshot;
use crate::{CancellationToken, DecodedStream, InterchangeError, ScanSelection, StreamOptions};
use arrow::array::RecordBatch;
use arrow::datatypes::Schema;
use bytes::Bytes;
use fsspec_rs::{ErrorKind, OperationError};
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use std::sync::Arc;

fn corrupt(error: impl std::fmt::Display) -> InterchangeError {
    OperationError::Detailed {
        kind: ErrorKind::Corrupt,
        message: error.to_string(),
        retry_safe: false,
    }
    .into()
}

impl Snapshot {
    /// Read listed parts on demand. Entire encoded parts are verified before decoding.
    /// Parts wholly before the row offset are skipped without fetching them.
    pub fn scan(
        self: &Arc<Self>,
        selection: ScanSelection,
        options: StreamOptions,
        cancellation: CancellationToken,
    ) -> crate::Result<DecodedStream> {
        let schema = &self.manifest().schema;
        let indices = match selection.columns {
            Some(columns) => columns
                .iter()
                .map(|name| schema.index_of(name))
                .collect::<Result<Vec<_>, _>>()?,
            None => (0..schema.fields().len()).collect(),
        };
        let schema = Arc::new(schema.project(&indices)?);
        let mut offset = selection.row_offset;
        let mut index = 0;
        for part in &self.manifest().parts {
            if part.num_rows > offset as u64 {
                break;
            }
            offset -= part.num_rows as usize;
            index += 1;
        }
        let source = PartBatches {
            snapshot: self.clone(),
            index,
            reader: None,
            batch_size: options.batch_size,
            indices,
            schema: schema.clone(),
        };
        DecodedStream::new_with_offset(schema, Box::new(source), options, cancellation, offset)
    }
}

struct PartBatches {
    snapshot: Arc<Snapshot>,
    index: usize,
    reader: Option<ParquetRecordBatchReader>,
    batch_size: usize,
    indices: Vec<usize>,
    schema: Arc<Schema>,
}
impl Iterator for PartBatches {
    type Item = crate::Result<RecordBatch>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(reader) = &mut self.reader {
                if let Some(batch) = reader.next() {
                    return Some(
                        batch
                            .and_then(|b| b.project(&self.indices))
                            .and_then(|b| b.with_schema(self.schema.clone()))
                            .map_err(corrupt),
                    );
                }
                self.reader = None;
            }
            let part = self.snapshot.manifest().parts.get(self.index)?;
            let result = (|| {
                if part.encoding != "parquet" || !part.options.is_empty() {
                    return Err(InterchangeError::Operation(OperationError::Detailed {
                        kind: ErrorKind::NotSupported,
                        message: "table scans require Parquet parts without encoding options"
                            .into(),
                        retry_safe: false,
                    }));
                }
                let bytes = self.snapshot.read_part(self.index)?;
                let builder = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes))
                    .map_err(corrupt)?;
                if builder.schema().as_ref() != &self.snapshot.manifest().schema
                    || builder.metadata().file_metadata().num_rows() as u64 != part.num_rows
                {
                    return Err(InterchangeError::Operation(OperationError::Detailed {
                        kind: ErrorKind::Corrupt,
                        message: "Parquet schema or row count differs from manifest".into(),
                        retry_safe: false,
                    }));
                }
                builder
                    .with_batch_size(self.batch_size)
                    .build()
                    .map_err(corrupt)
            })();
            self.index += 1;
            match result {
                Ok(reader) => self.reader = Some(reader),
                Err(error) => return Some(Err(error)),
            }
        }
    }
}
