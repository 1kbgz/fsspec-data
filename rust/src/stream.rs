use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;

use crate::{InterchangeError, Result};

#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamOptions {
    pub batch_size: usize,
    pub row_limit: Option<usize>,
    pub byte_limit: Option<usize>,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            batch_size: 1024,
            row_limit: None,
            byte_limit: None,
        }
    }
}

/// Selection applied before output row and byte limits.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScanSelection {
    /// Top-level columns in output order, including duplicates; None selects all.
    pub columns: Option<Vec<String>>,
    /// Number of source rows to skip, starting from zero.
    pub row_offset: usize,
}

pub struct DecodedStream {
    pub schema: SchemaRef,
    source: Box<dyn Iterator<Item = Result<RecordBatch>> + Send>,
    options: StreamOptions,
    cancellation: CancellationToken,
    pending: Option<(RecordBatch, usize)>,
    rows_to_skip: usize,
    emitted_rows: usize,
    emitted_bytes: usize,
    finished: bool,
}

impl DecodedStream {
    pub fn new(
        schema: SchemaRef,
        source: Box<dyn Iterator<Item = Result<RecordBatch>> + Send>,
        options: StreamOptions,
        cancellation: CancellationToken,
    ) -> Result<Self> {
        Self::new_with_offset(schema, source, options, cancellation, 0)
    }

    /// Skip source rows before applying output limits. Skipped rows do not count
    /// toward either limit; cancellation remains observable while skipping.
    pub fn new_with_offset(
        schema: SchemaRef,
        source: Box<dyn Iterator<Item = Result<RecordBatch>> + Send>,
        options: StreamOptions,
        cancellation: CancellationToken,
        row_offset: usize,
    ) -> Result<Self> {
        if options.batch_size == 0 {
            return Err(InterchangeError::InvalidBatchSize);
        }
        Ok(Self {
            schema,
            source,
            options,
            cancellation,
            pending: None,
            rows_to_skip: row_offset,
            emitted_rows: 0,
            emitted_bytes: 0,
            finished: false,
        })
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub fn collect_batches(self) -> Result<Vec<RecordBatch>> {
        self.collect()
    }
}

impl Iterator for DecodedStream {
    type Item = Result<RecordBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if self.cancellation.is_cancelled() {
            self.finished = true;
            return Some(Err(InterchangeError::StreamCancelled));
        }
        if self
            .options
            .row_limit
            .is_some_and(|limit| self.emitted_rows >= limit)
        {
            self.finished = true;
            return None;
        }

        let (batch, offset) = loop {
            if self.cancellation.is_cancelled() {
                self.finished = true;
                return Some(Err(InterchangeError::StreamCancelled));
            }
            let mut pending = match self.pending.take() {
                Some(pending) => pending,
                None => match self.source.next()? {
                    Ok(batch) => (batch, 0),
                    Err(error) => {
                        self.finished = true;
                        return Some(Err(error));
                    }
                },
            };
            let skipped = self.rows_to_skip.min(pending.0.num_rows() - pending.1);
            pending.1 += skipped;
            self.rows_to_skip -= skipped;
            if pending.0.num_rows() > pending.1 {
                break pending;
            }
        };
        let available = batch.num_rows() - offset;
        let row_remaining = self
            .options
            .row_limit
            .map(|limit| limit - self.emitted_rows)
            .unwrap_or(usize::MAX);
        let length = available.min(self.options.batch_size).min(row_remaining);
        if length == 0 {
            self.finished = true;
            return None;
        }
        if length < available {
            self.pending = Some((batch.clone(), offset + length));
        }
        let output = if offset == 0 && length == batch.num_rows() {
            batch
        } else {
            batch.slice(offset, length)
        };
        let batch_bytes = output.get_array_memory_size();
        let attempted = self.emitted_bytes.saturating_add(batch_bytes);
        if let Some(limit) = self.options.byte_limit {
            if attempted > limit {
                self.finished = true;
                return Some(Err(InterchangeError::ByteLimitExceeded {
                    limit,
                    attempted,
                }));
            }
        }
        self.emitted_rows += output.num_rows();
        self.emitted_bytes = attempted;
        Some(Ok(output))
    }
}

/// Projects a decoded stream before enforcing the requested output limits.
pub(crate) fn project_columns(
    stream: DecodedStream,
    columns: &[String],
    options: StreamOptions,
    cancellation: CancellationToken,
) -> Result<DecodedStream> {
    let indices = columns
        .iter()
        .map(|name| stream.schema.index_of(name))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let schema = Arc::new(stream.schema.project(&indices)?);
    DecodedStream::new(
        schema,
        Box::new(stream.map(move |batch| Ok(batch?.project(&indices)?))),
        options,
        cancellation,
    )
}

#[cfg(test)]
mod tests {
    use arrow::array::Int64Array;
    use arrow::datatypes::{DataType, Field, Schema};

    use super::*;

    fn stream(options: StreamOptions) -> DecodedStream {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(Int64Array::from(vec![1, 2, 3, 4, 5]))],
        )
        .unwrap();
        DecodedStream::new(
            schema,
            Box::new(vec![Ok(batch)].into_iter()),
            options,
            CancellationToken::new(),
        )
        .unwrap()
    }

    #[test]
    fn stream_splits_batches_and_applies_row_limit() {
        let batches = stream(StreamOptions {
            batch_size: 2,
            row_limit: Some(4),
            byte_limit: None,
        })
        .collect_batches()
        .unwrap();

        assert_eq!(
            batches
                .iter()
                .map(RecordBatch::num_rows)
                .collect::<Vec<_>>(),
            vec![2, 2]
        );
    }

    #[test]
    fn stream_enforces_byte_limit() {
        let error = stream(StreamOptions {
            batch_size: 2,
            row_limit: None,
            byte_limit: Some(1),
        })
        .next()
        .unwrap()
        .unwrap_err()
        .to_string();

        assert!(error.contains("byte limit"));
    }

    #[test]
    fn stream_observes_cancellation() {
        let mut stream = stream(StreamOptions::default());
        stream.cancellation_token().cancel();

        assert!(stream
            .next()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        assert!(stream.next().is_none());
    }
    #[test]
    fn cancellation_is_observed_while_skipping_batches() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![1]))])
            .unwrap();
        let cancellation = CancellationToken::new();
        let token = cancellation.clone();
        let source = std::iter::from_fn(move || {
            token.cancel();
            Some(Ok(batch.clone()))
        });
        let mut stream = DecodedStream::new_with_offset(
            schema,
            Box::new(source),
            StreamOptions::default(),
            cancellation,
            usize::MAX,
        )
        .unwrap();
        assert!(matches!(
            stream.next(),
            Some(Err(InterchangeError::StreamCancelled))
        ));
        assert!(stream.next().is_none());
    }
}
