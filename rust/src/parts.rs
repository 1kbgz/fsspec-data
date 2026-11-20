//! Parquet part encoding with bounded buffers and estimated encoder accounting.
//!
//! Completed parts are handed to a synchronous consumer. This module performs no
//! storage writes or manifest publication. Dropping a writer discards its current
//! part; already handed-off parts remain the consumer's responsibility.
//!
//! ```
//! use std::sync::Arc;
//! use arrow::array::{Int64Array, RecordBatch};
//! use fsspec_data::parts::{ParquetPartWriter, PartLimits};
//!
//! let batch = RecordBatch::try_from_iter([
//!     ("value", Arc::new(Int64Array::from(vec![1, 2, 3])) as _),
//! ])?;
//! let mut rows = 0;
//! let mut writer = ParquetPartWriter::new(batch.schema(), PartLimits::default(), |part| {
//!     // A storage consumer would stage part.bytes and retain its checksum/row count.
//!     rows += part.num_rows;
//!     Ok(())
//! })?;
//! writer.write_batch(&batch)?;
//! writer.finish()?;
//! assert_eq!(rows, 3);
//! # Ok::<(), fsspec_data::InterchangeError>(())
//! ```

use std::io::{self, Write};

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use fsspec_rs::{ErrorKind, OperationError};
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;

use crate::manifest::Checksum;
use crate::{InterchangeError, Result};

// Check encoder estimates between bounded row slices, including within a large batch.
const ROWS_PER_WRITE: usize = 1024;

#[derive(Clone, Copy, Debug)]
pub struct PartLimits {
    /// Rotation goal before footer overhead; an individual write can exceed it.
    pub target_part_bytes: usize,
    /// Input/output capacity plus the encoder's reported memory estimate.
    /// This is not a hard limit on Parquet's internal allocations or process RSS.
    pub memory_limit: usize,
}

impl Default for PartLimits {
    fn default() -> Self {
        Self {
            target_part_bytes: 8 * 1024 * 1024,
            memory_limit: 64 * 1024 * 1024,
        }
    }
}

/// One complete Parquet file. The consumer takes ownership without a byte copy.
pub struct EncodedPart {
    pub bytes: Vec<u8>,
    pub num_rows: u64,
    pub sha256: Checksum,
}

/// Encodes rows in order, retaining at most one unfinished part and no input batches.
/// A consumer error or an error after encoding begins permanently fails the writer.
pub struct ParquetPartWriter<F: FnMut(EncodedPart) -> Result<()>> {
    schema: SchemaRef,
    limits: PartLimits,
    consume: F,
    writer: Option<ArrowWriter<PartBuffer>>,
    rows: u64,
    failed: bool,
}

impl<F: FnMut(EncodedPart) -> Result<()>> ParquetPartWriter<F> {
    pub fn new(schema: SchemaRef, limits: PartLimits, consume: F) -> Result<Self> {
        if limits.target_part_bytes == 0
            || limits.target_part_bytes >= limits.memory_limit
            || schema.fields().is_empty()
        {
            return Err(invalid(
                "require a nonempty schema and 0 < target < memory limit",
            ));
        }
        Ok(Self {
            schema,
            limits,
            consume,
            writer: None,
            rows: 0,
            failed: false,
        })
    }

    /// Preflight schema/input errors leave existing buffered rows intact. Encoding
    /// errors discard the current part. A batch may span several emitted parts;
    /// callers must discard all staged parts from a failed unpublished attempt.
    pub fn write_batch(&mut self, batch: &RecordBatch) -> Result<()> {
        self.ensure_open()?;
        if batch.schema() != self.schema {
            return Err(InterchangeError::InputSchema);
        }
        if batch.num_rows() == 0 {
            return Ok(());
        }
        // Array memory includes backing buffer capacities, including those retained
        // by slices. Shared buffers may be counted more than once, conservatively.
        let input_bytes = batch.columns().iter().try_fold(0usize, |total, array| {
            total
                .checked_add(array.get_array_memory_size())
                .ok_or_else(|| invalid("input memory accounting overflow"))
        })?;
        if input_bytes >= self.limits.memory_limit {
            return Err(invalid("input buffers exceed writer memory limit"));
        }
        let result = self.encode_batch(batch, input_bytes);
        self.fail_on_error(result)
    }

    /// Emit the current part and keep encoding open. Does not publish table data.
    pub fn flush(&mut self) -> Result<()> {
        self.ensure_open()?;
        let result = self.emit_part(0);
        self.fail_on_error(result)
    }

    /// Emit remaining rows and close this encoder. Does not mark a table complete.
    pub fn finish(mut self) -> Result<()> {
        self.flush()
    }

    /// Discard the current part. Already emitted parts are not recalled.
    pub fn abort(self) {}

    fn ensure_open(&self) -> Result<()> {
        if self.failed {
            Err(invalid(
                "part writer failed; discard the unpublished attempt",
            ))
        } else {
            Ok(())
        }
    }

    fn fail_on_error(&mut self, result: Result<()>) -> Result<()> {
        if result.is_err() {
            self.writer = None;
            self.rows = 0;
            self.failed = true;
        }
        result
    }

    fn encode_batch(&mut self, batch: &RecordBatch, input_bytes: usize) -> Result<()> {
        // Finish an older part before borrowing this input for encoding if the two
        // cannot coexist within the accounting budget.
        if let Some(writer) = &self.writer {
            if input_bytes
                .saturating_add(writer.memory_size())
                .saturating_add(writer.inner().bytes.capacity())
                > self.limits.memory_limit
            {
                self.emit_part(0)?;
            }
        }
        for offset in (0..batch.num_rows()).step_by(ROWS_PER_WRITE) {
            if self.writer.is_none() {
                // A single row group per part keeps metadata from accumulating
                // across arbitrarily many small calls. Rotation is managed here.
                let properties = WriterProperties::builder()
                    .set_max_row_group_row_count(None)
                    .set_max_row_group_bytes(None)
                    .build();
                self.writer = Some(
                    ArrowWriter::try_new(
                        PartBuffer {
                            bytes: Vec::new(),
                            limit: self.limits.memory_limit - input_bytes,
                        },
                        self.schema.clone(),
                        Some(properties),
                    )
                    .map_err(encoding_error)?,
                );
            }
            self.set_output_limit(input_bytes)?;
            let count = ROWS_PER_WRITE.min(batch.num_rows() - offset);
            self.rows = self
                .rows
                .checked_add(count as u64)
                .ok_or_else(|| invalid("part row count overflow"))?;
            let writer = self.writer.as_mut().unwrap();
            writer
                .write(&batch.slice(offset, count))
                .map_err(encoding_error)?;
            // ArrowWriter reports estimates after allocation; this check cannot
            // prevent transient or unreported allocations within the encoder.
            self.set_output_limit(input_bytes)?;
            let writer = self.writer.as_ref().unwrap();
            let output_bytes = writer.inner().bytes.capacity();
            let available = self.limits.memory_limit - input_bytes - output_bytes;
            if writer
                .bytes_written()
                .saturating_add(writer.in_progress_size())
                >= self.limits.target_part_bytes
                || writer.memory_size() >= available / 2
            {
                self.emit_part(input_bytes)?;
            }
        }
        Ok(())
    }

    fn set_output_limit(&mut self, input_bytes: usize) -> Result<()> {
        let writer = self.writer.as_mut().unwrap();
        let limit = self
            .limits
            .memory_limit
            .checked_sub(input_bytes)
            .and_then(|n| n.checked_sub(writer.memory_size()))
            .ok_or_else(|| invalid("input and encoder estimate exceed writer memory limit"))?;
        if writer.inner().bytes.capacity() > limit {
            return Err(invalid(
                "input, output and encoder estimate exceed writer memory limit",
            ));
        }
        writer.inner_mut().limit = limit;
        Ok(())
    }

    fn emit_part(&mut self, input_bytes: usize) -> Result<()> {
        if self.writer.is_none() {
            return Ok(());
        }
        self.set_output_limit(input_bytes)?;
        // Keep the pre-flush encoder estimate charged while pages/footer are
        // copied into output. into_inner finalizes the file and drops the encoder.
        let buffer = self
            .writer
            .take()
            .unwrap()
            .into_inner()
            .map_err(encoding_error)?;
        let part = EncodedPart {
            sha256: Checksum::of(&buffer.bytes),
            bytes: buffer.bytes,
            num_rows: std::mem::take(&mut self.rows),
        };
        (self.consume)(part)
    }
}

fn invalid(message: impl Into<String>) -> InterchangeError {
    OperationError::Detailed {
        kind: ErrorKind::Invalid,
        message: message.into(),
        retry_safe: false,
    }
    .into()
}

fn encoding_error(error: parquet::errors::ParquetError) -> InterchangeError {
    invalid(format!("Parquet part encoding failed: {error}"))
}

struct PartBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for PartBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|end| *end <= self.limit)
            .ok_or_else(|| {
                io::Error::other("encoded part exceeds remaining writer memory limit")
            })?;
        if end > self.bytes.capacity() {
            let capacity = end.max(self.bytes.capacity().saturating_mul(2).min(self.limit));
            self.bytes
                .try_reserve_exact(capacity - self.bytes.len())
                .map_err(io::Error::other)?;
            if self.bytes.capacity() > self.limit {
                return Err(io::Error::other(
                    "output allocation exceeds writer memory limit",
                ));
            }
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_limit_rejects_excess_without_a_partial_write() {
        let mut buffer = PartBuffer {
            bytes: Vec::new(),
            limit: 10,
        };
        buffer.write_all(b"12345678").unwrap();
        assert!(buffer.write_all(b"over limit").is_err());
        assert_eq!(buffer.bytes, b"12345678");
        buffer.write_all(b"90").unwrap();
        assert_eq!(buffer.bytes, b"1234567890");
        assert!(buffer.bytes.capacity() <= 10);
        assert!(buffer.write_all(b"!").is_err());
    }
}
