//! Synchronous polling of row-oriented manifest tables, with resumable positions.
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use fsspec_rs::{ErrorKind, OperationError};
use serde::{Deserialize, Serialize};

use crate::manifest::{Checksum, ManifestReader, Selection, Snapshot};
use crate::{CancellationToken, DecodedStream, InterchangeError, ScanSelection, StreamOptions};

/// Checkpoint after a returned batch. Keep the source label stable across restarts.
/// Positions describe progress, not authorization, and must not contain credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FollowPosition {
    pub source: String,
    pub namespace_id: String,
    pub table_id: String,
    pub generation: u64,
    pub manifest_sha256: Checksum,
    pub progress: u64,
    pub progress_unit: String,
}

#[derive(Clone, Debug)]
pub struct FollowOptions {
    pub start: Option<FollowPosition>,
    pub columns: Option<Vec<String>>,
    pub batch_size: usize,
    pub poll_interval: Duration,
    pub idle_timeout: Option<Duration>,
}
impl Default for FollowOptions {
    fn default() -> Self {
        Self {
            start: None,
            columns: None,
            batch_size: 1024,
            poll_interval: Duration::from_millis(100),
            idle_timeout: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowEnd {
    Complete,
    IdleTimeout,
    Cancelled,
    Replaced,
}

pub struct FollowBatch {
    pub batch: RecordBatch,
    pub position: FollowPosition,
}

/// Reads the starting snapshot, then each successor in order without directory listing.
/// Synchronous storage calls cannot be interrupted; cancellation is checked between calls
/// and wakes polling waits. Errors terminate iteration and preserve the last checkpoint.
pub struct ManifestFollower {
    reader: ManifestReader,
    snapshot: Arc<Snapshot>,
    stream: Option<DecodedStream>,
    options: FollowOptions,
    cancellation: CancellationToken,
    position: FollowPosition,
    schema: SchemaRef,
    idle_since: Instant,
    end: Option<FollowEnd>,
    failed: bool,
}
fn error(kind: ErrorKind, message: &str) -> InterchangeError {
    OperationError::Detailed {
        kind,
        message: message.into(),
        retry_safe: false,
    }
    .into()
}

impl ManifestFollower {
    /// `source` identifies the configured storage location, including endpoint and path.
    /// Use the same non-secret label only for the same logical source across restarts.
    /// A new follow starts at the latest snapshot; a resume starts at its saved generation.
    pub fn new(
        reader: ManifestReader,
        source: String,
        options: FollowOptions,
        cancellation: CancellationToken,
    ) -> crate::Result<Self> {
        if source.is_empty() || options.batch_size == 0 || options.poll_interval.is_zero() {
            return Err(error(
                ErrorKind::Invalid,
                "invalid follow options or source",
            ));
        }
        if options.start.as_ref().is_some_and(|p| p.source != source) {
            return Err(error(
                ErrorKind::Invalid,
                "position belongs to a different source",
            ));
        }
        let snapshot = Arc::new(
            reader.read(
                options
                    .start
                    .as_ref()
                    .map_or(Selection::Latest, |p| Selection::Generation(p.generation)),
            )?,
        );
        let m = snapshot.manifest();
        if m.deleted {
            return Err(error(ErrorKind::NotFound, "table is deleted"));
        }
        if m.progress_unit != "rows" {
            return Err(error(
                ErrorKind::NotSupported,
                "follow requires row progress",
            ));
        }
        let position = FollowPosition {
            source,
            namespace_id: m.namespace_id.clone(),
            table_id: m.table_id.clone(),
            generation: m.generation,
            manifest_sha256: snapshot.checksum(),
            progress: 0,
            progress_unit: m.progress_unit.clone(),
        };
        let position = if let Some(start) = &options.start {
            let mut expected = position.clone();
            expected.progress = start.progress;
            if *start != expected || start.progress > m.progress {
                return Err(error(
                    ErrorKind::Invalid,
                    "position does not match the saved table generation",
                ));
            }
            start.clone()
        } else {
            position
        };
        let stream = snapshot.scan(
            ScanSelection {
                columns: options.columns.clone(),
                row_offset: usize::try_from(position.progress).map_err(|_| {
                    error(ErrorKind::Invalid, "position exceeds platform row limit")
                })?,
            },
            StreamOptions {
                batch_size: options.batch_size,
                ..StreamOptions::default()
            },
            cancellation.clone(),
        )?;
        let schema = stream.schema.clone();
        Ok(Self {
            reader,
            snapshot,
            stream: Some(stream),
            options,
            cancellation,
            position,
            schema,
            idle_since: Instant::now(),
            end: None,
            failed: false,
        })
    }
    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
    pub fn position(&self) -> &FollowPosition {
        &self.position
    }
    pub fn end_reason(&self) -> Option<FollowEnd> {
        self.end
    }
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    fn advance(&mut self) -> crate::Result<Option<FollowBatch>> {
        loop {
            if self.cancellation.is_cancelled() {
                self.stream = None;
                self.end = Some(FollowEnd::Cancelled);
                return Ok(None);
            }
            if let Some(stream) = &mut self.stream {
                match stream.next() {
                    Some(Ok(batch)) => {
                        // Cancellation after a blocking read must not advance the checkpoint.
                        if self.cancellation.is_cancelled() {
                            continue;
                        }
                        self.position.generation = self.snapshot.manifest().generation;
                        self.position.manifest_sha256 = self.snapshot.checksum();
                        self.position.progress += batch.num_rows() as u64;
                        self.idle_since = Instant::now();
                        return Ok(Some(FollowBatch {
                            batch,
                            position: self.position.clone(),
                        }));
                    }
                    Some(Err(InterchangeError::StreamCancelled)) => continue,
                    Some(Err(e)) => return Err(e),
                    None => self.stream = None,
                }
            }
            let m = self.snapshot.manifest();
            if m.complete {
                self.end = Some(FollowEnd::Complete);
                return Ok(None);
            }
            let generation = m
                .generation
                .checked_add(1)
                .ok_or_else(|| error(ErrorKind::Invalid, "manifest generation overflow"))?;
            match self.reader.read(Selection::Generation(generation)) {
                Ok(next) => {
                    let next = Arc::new(next);
                    let n = next.manifest();
                    if n.namespace_id != m.namespace_id {
                        self.end = Some(FollowEnd::Replaced);
                        return Ok(None);
                    }
                    if n.parent.as_ref().map(|p| p.sha256) != Some(self.snapshot.checksum()) {
                        return Err(error(
                            ErrorKind::Corrupt,
                            "successor does not reference the followed manifest",
                        ));
                    }
                    if n.table_id != m.table_id || n.deleted {
                        self.end = Some(FollowEnd::Replaced);
                        return Ok(None);
                    }
                    n.validate_parent(m)?;
                    self.stream = Some(next.scan(
                        ScanSelection {
                            columns: self.options.columns.clone(),
                            row_offset: usize::try_from(self.position.progress).map_err(|_| {
                                error(ErrorKind::Invalid, "position exceeds platform row limit")
                            })?,
                        },
                        StreamOptions {
                            batch_size: self.options.batch_size,
                            ..StreamOptions::default()
                        },
                        self.cancellation.clone(),
                    )?);
                    self.snapshot = next;
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    let wait = match self.options.idle_timeout {
                        Some(timeout) => match timeout.checked_sub(self.idle_since.elapsed()) {
                            Some(remaining) if !remaining.is_zero() => {
                                remaining.min(self.options.poll_interval)
                            }
                            _ => {
                                self.end = Some(FollowEnd::IdleTimeout);
                                return Ok(None);
                            }
                        },
                        None => self.options.poll_interval,
                    };
                    futures::executor::block_on(async {
                        let cancel = self.cancellation.cancelled();
                        futures::pin_mut!(cancel);
                        futures::future::select(cancel, futures_timer::Delay::new(wait)).await;
                    });
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}
impl Iterator for ManifestFollower {
    type Item = crate::Result<FollowBatch>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.end.is_some() || self.failed {
            return None;
        }
        match self.advance() {
            Ok(batch) => batch.map(Ok),
            Err(e) => {
                self.failed = true;
                self.stream = None;
                Some(Err(e))
            }
        }
    }
}
