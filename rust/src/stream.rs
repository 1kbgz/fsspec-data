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

pub struct DecodedStream {
    pub schema: SchemaRef,
    source: Box<dyn Iterator<Item = Result<RecordBatch>> + Send>,
    options: StreamOptions,
    cancellation: CancellationToken,
    pending: Option<(RecordBatch, usize)>,
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
        if options.batch_size == 0 {
            return Err(InterchangeError::InvalidBatchSize);
        }
        Ok(Self {
            schema,
            source,
            options,
            cancellation,
            pending: None,
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
            let pending = match self.pending.take() {
                Some(pending) => pending,
                None => match self.source.next()? {
                    Ok(batch) => (batch, 0),
                    Err(error) => {
                        self.finished = true;
                        return Some(Err(error));
                    }
                },
            };
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
}
