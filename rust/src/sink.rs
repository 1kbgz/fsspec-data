//! Positioned, idempotent writes to immutable manifest tables.
//!
//! Storage must provide atomic, durable create-if-absent and consistent reads.
//! Advertising `write_missing` is required but does not certify those guarantees.
//! Parts and attempt descriptors are retained on failure; reclamation requires the
//! closed-generation retention protocol. This module never deletes objects.
//!
//! ```no_run
//! use std::sync::Arc;
//! use arrow::array::{ArrayRef, Int64Array, RecordBatch};
//! use fsspec_data::sink::{FileDataSink, SinkOptions, WriteMode};
//! use fsspec_rs::LocalFs;
//!
//! let batch = RecordBatch::try_from_iter([
//!     ("value", Arc::new(Int64Array::from(vec![1, 2, 3])) as ArrayRef),
//! ])?;
//! let mut sink = FileDataSink::open(Arc::new(LocalFs::new()), "/data/events",
//!     WriteMode::Create, batch.schema(), SinkOptions::default())?;
//! let recovery = sink.recovery_token(); // Persist with the source configuration.
//! sink.write_batch(0, "request-1", &batch)?;
//! let committed = sink.commit()?; // Checkpoint committed.progress, not position().
//! assert_eq!(committed.progress, 3);
//! sink.close()?; // The table remains incomplete.
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use fsspec_rs::{Condition, ErrorKind, OperationError, SharedFileSystem, WriteOptions};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::fingerprint::{fingerprint, validate_schema};
use crate::manifest::{
    bounded_read, identifier, join, BatchReceipt, Checksum, Manifest, ManifestReader, Parent, Part,
    PublicationKind, ReadLimits, Selection, Span, MAX_MANIFEST_BYTES,
};
use crate::parts::{EncodedPart, ParquetPartWriter, PartLimits, ROWS_PER_WRITE};
use crate::InterchangeError;

type Result<T> = std::result::Result<T, OperationError>;
type Encoder = ParquetPartWriter<Box<dyn FnMut(EncodedPart) -> crate::Result<()> + Send>>;
const DESCRIPTOR_HEADER: &[u8] = b"fsspec-data/candidate/1\n";

#[derive(Clone, Copy, Debug)]
pub enum WriteMode {
    Create,
    Append,
    Replace,
}

#[derive(Clone, Debug)]
pub struct SinkOptions {
    pub parts: PartLimits,
    /// Encoded manifest cap, including inherited parts and new receipts.
    pub max_manifest_bytes: usize,
    /// Maximum historical generations examined for one retry key.
    pub max_history_reads: usize,
    /// Used for Create/Replace; must agree with an existing partition's values.
    pub partition_values: BTreeMap<String, Value>,
}

impl Default for SinkOptions {
    fn default() -> Self {
        Self {
            parts: PartLimits::default(),
            max_manifest_bytes: MAX_MANIFEST_BYTES,
            max_history_reads: 256,
            partition_values: BTreeMap::new(),
        }
    }
}

/// Persist this token before commit/finish. Recover using the same source and
/// credential context; credentials are deliberately not serialized here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryToken {
    pub root: String,
    pub namespace_id: String,
    pub table_id: String,
    pub generation: u64,
    pub publication_id: String,
}

#[derive(Clone, Copy, Debug)]
pub enum RecoveryAction {
    /// Observe the target without resubmitting publication.
    Observe,
    /// May resubmit the identical manifest to the original generation.
    Retry,
}

#[derive(Clone, Debug)]
pub struct MarkerError {
    pub kind: ErrorKind,
    pub message: String,
}

/// A successful receipt authorizes checkpointing this committed progress.
#[derive(Clone, Debug)]
pub struct PublicationReceipt {
    pub recovery: RecoveryToken,
    pub sha256: Checksum,
    pub progress: u64,
    pub num_rows: u64,
    pub complete: bool,
    pub marker_error: Option<MarkerError>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Open,
    Frozen,
    Failed,
    Closed,
}

struct Candidate {
    manifest: Manifest,
    bytes: Vec<u8>,
    sha256: Checksum,
}
struct Staging {
    fs: SharedFileSystem,
    token: RecoveryToken,
    manifest: Manifest,
    limit: usize,
}

pub struct FileDataSink {
    fs: SharedFileSystem,
    options: SinkOptions,
    schema: SchemaRef,
    stage: Arc<Mutex<Staging>>,
    encoder: Option<Encoder>,
    base: Option<(Manifest, Checksum)>,
    accepted: u64,
    state: State,
    candidate: Option<Candidate>,
    last: Option<PublicationReceipt>,
}

impl FileDataSink {
    pub fn open(
        fs: SharedFileSystem,
        root: impl Into<String>,
        mode: WriteMode,
        schema: SchemaRef,
        options: SinkOptions,
    ) -> Result<Self> {
        check_options(&fs, &options)?;
        validate_schema(&schema)?;
        let root = root.into();
        let base = latest(&fs, &root, &options)?;
        match (mode, &base) {
            (WriteMode::Create, Some((m, _))) if !m.deleted => {
                return Err(error(ErrorKind::AlreadyExists, "table already exists"))
            }
            (WriteMode::Append | WriteMode::Replace, None) => {
                return Err(error(ErrorKind::NotFound, "table not found"))
            }
            (WriteMode::Append | WriteMode::Replace, Some((m, _))) if m.deleted => {
                return Err(error(ErrorKind::NotFound, "table is deleted"))
            }
            (WriteMode::Append, Some((m, _)))
                if m.complete || m.schema != *schema || m.progress_unit != "rows" =>
            {
                return Err(error(
                    ErrorKind::Conflict,
                    "append requires an incomplete table with the same schema and row progress",
                ))
            }
            _ => {}
        }
        if let Some((m, _)) = &base {
            if m.partition_values != options.partition_values {
                return Err(error(ErrorKind::Conflict, "partition values changed"));
            }
        }
        let generation = next_generation(base.as_ref().map(|(m, _)| m.generation))?;
        let publication_id = new_id();
        let table_id = match (&mode, &base) {
            (WriteMode::Append, Some((m, _))) => m.table_id.clone(),
            _ => new_id(),
        };
        let namespace_id = base
            .as_ref()
            .map(|(m, _)| m.namespace_id.clone())
            .unwrap_or_else(new_id);
        let inherited = matches!(mode, WriteMode::Append);
        let manifest = Manifest {
            format_version: 1,
            namespace_id: namespace_id.clone(),
            table_id: table_id.clone(),
            generation,
            parent: base.as_ref().map(|(m, hash)| Parent {
                generation: m.generation,
                sha256: *hash,
            }),
            publication_id: publication_id.clone(),
            kind: match mode {
                WriteMode::Create => PublicationKind::Create,
                WriteMode::Append => PublicationKind::Append,
                WriteMode::Replace => PublicationKind::Replace,
            },
            schema: (*schema).clone(),
            partition_values: options.partition_values.clone(),
            complete: false,
            deleted: false,
            num_rows: if inherited {
                base.as_ref().unwrap().0.num_rows
            } else {
                0
            },
            progress_unit: "rows".into(),
            progress: if inherited {
                base.as_ref().unwrap().0.progress
            } else {
                0
            },
            parts: if inherited {
                base.as_ref().unwrap().0.parts.clone()
            } else {
                Vec::new()
            },
            receipts: Vec::new(),
        };
        metadata_fits(&manifest, options.max_manifest_bytes)?;
        let accepted = manifest.progress;
        let token = RecoveryToken {
            root,
            namespace_id,
            table_id,
            generation,
            publication_id,
        };
        let stage = Arc::new(Mutex::new(Staging {
            fs: fs.clone(),
            token,
            manifest,
            limit: options.max_manifest_bytes,
        }));
        let encoder = Some(encoder(schema.clone(), options.parts, stage.clone())?);
        let last = base
            .as_ref()
            .filter(|_| inherited)
            .map(|(m, hash)| receipt(&stage.lock().unwrap().token.root, m, *hash));
        Ok(Self {
            fs,
            options,
            schema,
            stage,
            encoder,
            base,
            accepted,
            state: State::Open,
            candidate: None,
            last,
        })
    }

    pub fn recovery_token(&self) -> RecoveryToken {
        self.stage.lock().unwrap().token.clone()
    }
    /// Accepted position; only a publication receipt proves durable progress.
    pub fn position(&self) -> u64 {
        self.accepted
    }

    pub fn write_batch(
        &mut self,
        expected_position: u64,
        key: &str,
        batch: &RecordBatch,
    ) -> Result<BatchReceipt> {
        self.require_open()?;
        if key.is_empty()
            || key.len() > 256
            || batch.num_rows() == 0
            || batch.schema() != self.schema
        {
            return Err(error(
                ErrorKind::Invalid,
                "require a nonempty batch, matching schema, and a 1–256 byte key",
            ));
        }
        let input = batch
            .columns()
            .iter()
            .try_fold(0usize, |sum, a| sum.checked_add(a.get_array_memory_size()))
            .ok_or_else(|| error(ErrorKind::Invalid, "input size overflow"))?;
        if input >= self.options.parts.memory_limit {
            return Err(error(ErrorKind::Invalid, "input exceeds part writer limit"));
        }
        let end = expected_position
            .checked_add(batch.num_rows() as u64)
            .ok_or_else(|| error(ErrorKind::Invalid, "progress overflow"))?;
        let requested = BatchReceipt {
            key: key.into(),
            progress: Span {
                start: expected_position,
                end,
            },
            fingerprint: fingerprint(batch, expected_position)?,
        };
        if let Some(old) = self.lookup(key)? {
            return if old == requested {
                Ok(old)
            } else {
                Err(error(
                    ErrorKind::Conflict,
                    "retry key reused with different position or content",
                ))
            };
        }
        if expected_position != self.accepted {
            return Err(error(
                if expected_position > self.accepted {
                    ErrorKind::Gap
                } else {
                    ErrorKind::Conflict
                },
                "unexpected write position",
            ));
        }
        {
            let mut stage = self.stage.lock().unwrap();
            // Each row slice emits at most one part, plus an older buffered part.
            // 1 KiB covers our fixed part fields/keys with maximal numeric widths,
            // including growth of the manifest's row/progress totals.
            let remaining = batch
                .num_rows()
                .div_ceil(ROWS_PER_WRITE)
                .checked_add(1)
                .and_then(|n| n.checked_mul(1024))
                .and_then(|reserve| stage.limit.checked_sub(reserve))
                .ok_or_else(|| {
                    error(
                        ErrorKind::Invalid,
                        "insufficient manifest space for staged parts",
                    )
                })?;
            stage.manifest.receipts.push(requested.clone());
            if let Err(e) = metadata_fits(&stage.manifest, remaining) {
                stage.manifest.receipts.pop();
                return Err(e);
            }
        }
        if let Err(e) = self.encoder.as_mut().unwrap().write_batch(batch) {
            self.fail();
            return Err(operation(e));
        }
        self.accepted = end;
        Ok(requested)
    }

    pub fn flush(&mut self) -> Result<()> {
        self.require_open()?;
        if let Err(e) = self.encoder.as_mut().unwrap().flush() {
            self.fail();
            return Err(operation(e));
        }
        Ok(())
    }

    pub fn commit(&mut self) -> Result<PublicationReceipt> {
        self.publish(false)
    }
    pub fn finish(&mut self) -> Result<PublicationReceipt> {
        if self.state == State::Closed {
            if let Some(last) = &self.last {
                if last.complete {
                    return Ok(last.clone());
                }
            }
        }
        self.publish(true)
    }

    /// No objects are removed: aborted uploads remain invisible until safe collection.
    pub fn abort(&mut self) -> Result<()> {
        if self.state == State::Frozen {
            return Err(error(
                ErrorKind::UnknownOutcome,
                "resolve publication before aborting",
            ));
        }
        self.encoder = None;
        self.state = State::Closed;
        Ok(())
    }
    pub fn close(&mut self) -> Result<()> {
        self.abort()
    }

    pub fn resolve(&mut self, action: RecoveryAction) -> Result<PublicationReceipt> {
        if self.state != State::Frozen {
            return Err(error(ErrorKind::Invalid, "no unresolved publication"));
        }
        let token = self.recovery_token();
        let result = resolve_candidate(
            &self.fs,
            &token,
            self.candidate.as_ref().unwrap(),
            &self.options,
            action,
        );
        match result {
            Ok(receipt) => {
                self.accept_publication(&receipt)?;
                Ok(receipt)
            }
            Err(e) => {
                if e.kind() == ErrorKind::Conflict {
                    self.fail();
                }
                Err(e)
            }
        }
    }

    fn publish(&mut self, complete: bool) -> Result<PublicationReceipt> {
        self.require_open()?;
        if !complete && self.stage.lock().unwrap().manifest.receipts.is_empty() {
            if let Some(last) = &self.last {
                return Ok(last.clone());
            }
        }
        self.flush()?;
        let mut manifest = self.stage.lock().unwrap().manifest.clone();
        manifest.complete = complete;
        if complete && manifest.kind == PublicationKind::Append {
            manifest.kind = PublicationKind::Finish;
        }
        let bytes = match manifest.encode() {
            Ok(bytes) => bytes,
            Err(e) => {
                self.fail();
                return Err(e);
            }
        };
        if bytes.len() > self.options.max_manifest_bytes {
            self.fail();
            return Err(error(
                ErrorKind::Invalid,
                "manifest exceeds configured limit",
            ));
        }
        let sha256 = Checksum::of(&bytes);
        let candidate = Candidate {
            manifest,
            bytes,
            sha256,
        };
        let token = self.recovery_token();
        let descriptor = descriptor(&candidate);
        if let Err(e) = put_exact(&self.fs, &descriptor_path(&token), &descriptor) {
            self.fail();
            return Err(e);
        }
        self.encoder = None;
        self.candidate = Some(candidate);
        self.state = State::Frozen;
        let result = self.resolve(RecoveryAction::Retry);
        if let Err(e) = &result {
            if !matches!(
                e.kind(),
                ErrorKind::UnknownOutcome | ErrorKind::Timeout | ErrorKind::Unavailable
            ) {
                self.fail();
            }
        }
        result
    }

    fn accept_publication(&mut self, receipt: &PublicationReceipt) -> Result<()> {
        let candidate = self.candidate.take().unwrap();
        self.base = Some((candidate.manifest.clone(), candidate.sha256));
        self.last = Some(receipt.clone());
        self.accepted = candidate.manifest.progress;
        if candidate.manifest.complete || candidate.manifest.generation == u64::MAX {
            self.state = State::Closed;
            return Ok(());
        }
        let mut manifest = candidate.manifest;
        manifest.parent = Some(Parent {
            generation: manifest.generation,
            sha256: candidate.sha256,
        });
        manifest.generation += 1;
        manifest.publication_id = new_id();
        manifest.kind = PublicationKind::Append;
        manifest.receipts.clear();
        {
            let mut stage = self.stage.lock().unwrap();
            stage.token.generation = manifest.generation;
            stage.token.publication_id = manifest.publication_id.clone();
            stage.manifest = manifest;
        }
        // Configuration/schema were already validated. Allocation errors in the
        // next part are reported on its first write, after this success receipt.
        self.encoder = Some(encoder(
            self.schema.clone(),
            self.options.parts,
            self.stage.clone(),
        )?);
        self.state = State::Open;
        Ok(())
    }

    fn lookup(&self, key: &str) -> Result<Option<BatchReceipt>> {
        let stage = self.stage.lock().unwrap();
        if let Some(receipt) = stage.manifest.receipts.iter().find(|r| r.key == key) {
            return Ok(Some(receipt.clone()));
        }
        let instance = stage.manifest.table_id.clone();
        let root = stage.token.root.clone();
        drop(stage);
        let Some((base, _)) = &self.base else {
            return Ok(None);
        };
        let mut current = base.clone();
        for _ in 0..self.options.max_history_reads {
            if current.table_id != instance {
                return Ok(None);
            }
            if let Some(receipt) = current.receipts.iter().find(|r| r.key == key) {
                return Ok(Some(receipt.clone()));
            }
            let Some(parent) = current.parent else {
                return Ok(None);
            };
            let loaded = read_generation(&self.fs, &root, parent.generation, &self.options)?
                .ok_or_else(|| error(ErrorKind::Corrupt, "missing receipt history"))?;
            if loaded.1 != parent.sha256 {
                return Err(error(
                    ErrorKind::Corrupt,
                    "receipt history checksum mismatch",
                ));
            }
            current = loaded.0;
        }
        Err(error(
            ErrorKind::Timeout,
            "receipt history read budget exhausted",
        ))
    }

    fn require_open(&self) -> Result<()> {
        match self.state {
            State::Open => Ok(()),
            State::Frozen => Err(error(
                ErrorKind::UnknownOutcome,
                "resolve publication before writing",
            )),
            _ => Err(error(ErrorKind::Invalid, "writer is closed or failed")),
        }
    }
    fn fail(&mut self) {
        self.encoder = None;
        self.state = State::Failed;
    }
}

fn encoder(schema: SchemaRef, limits: PartLimits, stage: Arc<Mutex<Staging>>) -> Result<Encoder> {
    ParquetPartWriter::new(
        schema,
        limits,
        Box::new(move |encoded: EncodedPart| {
            let mut stage = stage.lock().unwrap();
            let end = stage
                .manifest
                .progress
                .checked_add(encoded.num_rows)
                .ok_or_else(|| error(ErrorKind::Invalid, "staged row count overflow"))?;
            let part = Part {
                key: format!(
                    "parts/{:020}/{}/{:020}.parquet",
                    stage.token.generation,
                    stage.token.publication_id,
                    stage.manifest.parts.len()
                ),
                size: encoded.bytes.len() as u64,
                sha256: encoded.sha256,
                num_rows: encoded.num_rows,
                progress: Span {
                    start: stage.manifest.progress,
                    end,
                },
                encoding: "parquet".into(),
                options: BTreeMap::new(),
            };
            stage.manifest.parts.push(part.clone());
            stage.manifest.progress = end;
            stage.manifest.num_rows = end;
            metadata_fits(&stage.manifest, stage.limit)?;
            put_exact(
                &stage.fs,
                &join(&stage.token.root, &part.key),
                &encoded.bytes,
            )?;
            Ok(())
        }) as Box<dyn FnMut(EncodedPart) -> crate::Result<()> + Send>,
    )
    .map_err(operation)
}

/// Recover after a restart. A different winner is detected before opening the
/// descriptor, which may have been collected for a losing attempt. On successful
/// complete publication this also attempts repair of the generation's _SUCCESS.
pub fn recover(
    fs: SharedFileSystem,
    token: &RecoveryToken,
    options: &SinkOptions,
    action: RecoveryAction,
) -> Result<PublicationReceipt> {
    check_options(&fs, options)?;
    if !identifier(&token.publication_id)
        || !identifier(&token.namespace_id)
        || !identifier(&token.table_id)
    {
        return Err(error(ErrorKind::Invalid, "invalid recovery identity"));
    }
    if let Some((actual, _)) = read_generation(&fs, &token.root, token.generation, options)? {
        check_winner(&actual, token)?;
    }
    let raw = bounded_read(
        &fs,
        &descriptor_path(token),
        options.max_manifest_bytes + DESCRIPTOR_HEADER.len() + 65,
    )?;
    let candidate = parse_descriptor(&raw, token, options)?;
    resolve_candidate(&fs, token, &candidate, options, action)
}

fn resolve_candidate(
    fs: &SharedFileSystem,
    token: &RecoveryToken,
    candidate: &Candidate,
    options: &SinkOptions,
    action: RecoveryAction,
) -> Result<PublicationReceipt> {
    if let Some((actual, hash)) = read_generation(fs, &token.root, token.generation, options)? {
        check_winner(&actual, token)?;
        if hash != candidate.sha256 {
            return Err(error(
                ErrorKind::Conflict,
                "published bytes differ from candidate",
            ));
        }
        return Ok(with_marker(fs, receipt(&token.root, &actual, hash)));
    }
    if matches!(action, RecoveryAction::Observe) {
        return Err(error(
            ErrorKind::UnknownOutcome,
            "publication absent; a delayed request may still succeed",
        ));
    }
    validate_candidate(fs, token, candidate, options)?;
    // Recovery verifies the new parts before resubmission. Inherited immutable
    // parts were already published by the validated parent.
    for part in &candidate.manifest.parts {
        if part
            .key
            .starts_with(&format!("parts/{:020}/", token.generation))
        {
            if part.size > options.parts.memory_limit as u64 {
                return Err(error(
                    ErrorKind::Invalid,
                    "recovery part exceeds read limit",
                ));
            }
            let bytes = bounded_read(fs, &join(&token.root, &part.key), part.size as usize)
                .map_err(|e| {
                    if e.kind() == ErrorKind::NotFound {
                        error(ErrorKind::Corrupt, "candidate part missing")
                    } else {
                        e
                    }
                })?;
            if bytes.len() as u64 != part.size || Checksum::of(&bytes) != part.sha256 {
                return Err(error(ErrorKind::Corrupt, "candidate part corrupt"));
            }
        }
    }
    let path = manifest_path(token);
    match put_exact(fs, &path, &candidate.bytes) {
        Ok(()) => Ok(with_marker(
            fs,
            receipt(&token.root, &candidate.manifest, candidate.sha256),
        )),
        Err(e) if e.kind() == ErrorKind::Conflict => {
            // A competing object must be a valid manifest before reporting conflict.
            read_generation(fs, &token.root, token.generation, options)?.ok_or_else(|| {
                error(
                    ErrorKind::UnknownOutcome,
                    "conflicting manifest is not readable",
                )
            })?;
            Err(e)
        }
        Err(e) => Err(e),
    }
}

fn validate_candidate(
    fs: &SharedFileSystem,
    token: &RecoveryToken,
    candidate: &Candidate,
    options: &SinkOptions,
) -> Result<()> {
    if let Some(parent) = &candidate.manifest.parent {
        let (m, hash) = read_generation(fs, &token.root, parent.generation, options)?
            .ok_or_else(|| error(ErrorKind::Corrupt, "candidate parent missing"))?;
        if hash != parent.sha256 {
            return Err(error(ErrorKind::Corrupt, "candidate parent changed"));
        }
        candidate.manifest.validate_parent(&m)?;
    }
    Ok(())
}

fn parse_descriptor(raw: &[u8], token: &RecoveryToken, options: &SinkOptions) -> Result<Candidate> {
    let header = DESCRIPTOR_HEADER.len();
    if raw.len() < header + 65 || !raw.starts_with(DESCRIPTOR_HEADER) || raw[header + 64] != b'\n' {
        return Err(error(ErrorKind::Corrupt, "invalid candidate descriptor"));
    }
    let bytes = &raw[header + 65..];
    if bytes.len() > options.max_manifest_bytes {
        return Err(error(
            ErrorKind::Invalid,
            "candidate exceeds manifest limit",
        ));
    }
    let sha256 =
        Checksum::try_from(String::from_utf8_lossy(&raw[header..header + 64]).into_owned())
            .map_err(|e| error(ErrorKind::Corrupt, e))?;
    if Checksum::of(bytes) != sha256 {
        return Err(error(ErrorKind::Corrupt, "candidate checksum mismatch"));
    }
    let manifest = Manifest::decode(bytes)?;
    check_winner(&manifest, token)?;
    if manifest.generation != token.generation {
        return Err(error(ErrorKind::Corrupt, "candidate generation mismatch"));
    }
    Ok(Candidate {
        manifest,
        bytes: bytes.to_vec(),
        sha256,
    })
}

fn check_winner(manifest: &Manifest, token: &RecoveryToken) -> Result<()> {
    if manifest.publication_id != token.publication_id
        || manifest.namespace_id != token.namespace_id
        || manifest.table_id != token.table_id
    {
        Err(error(
            ErrorKind::Conflict,
            "another publication won this generation",
        ))
    } else {
        Ok(())
    }
}

fn descriptor(candidate: &Candidate) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(DESCRIPTOR_HEADER.len() + 65 + candidate.bytes.len());
    bytes.extend_from_slice(DESCRIPTOR_HEADER);
    bytes.extend_from_slice(candidate.sha256.to_string().as_bytes());
    bytes.push(b'\n');
    bytes.extend_from_slice(&candidate.bytes);
    bytes
}
fn descriptor_path(token: &RecoveryToken) -> String {
    join(
        &token.root,
        &format!(
            "parts/{:020}/{}/candidate",
            token.generation, token.publication_id
        ),
    )
}
fn manifest_path(token: &RecoveryToken) -> String {
    join(
        &token.root,
        &format!("manifests/{:020}/manifest.json", token.generation),
    )
}
fn new_id() -> String {
    Uuid::new_v4().simple().to_string()
}
fn next_generation(base: Option<u64>) -> Result<u64> {
    match base {
        None => Ok(0),
        Some(g) => g
            .checked_add(1)
            .ok_or_else(|| error(ErrorKind::Invalid, "generation exhausted")),
    }
}
fn receipt(root: &str, m: &Manifest, sha256: Checksum) -> PublicationReceipt {
    PublicationReceipt {
        recovery: RecoveryToken {
            root: root.into(),
            namespace_id: m.namespace_id.clone(),
            table_id: m.table_id.clone(),
            generation: m.generation,
            publication_id: m.publication_id.clone(),
        },
        sha256,
        progress: m.progress,
        num_rows: m.num_rows,
        complete: m.complete,
        marker_error: None,
    }
}
fn with_marker(fs: &SharedFileSystem, mut receipt: PublicationReceipt) -> PublicationReceipt {
    if receipt.complete {
        let path = join(
            &receipt.recovery.root,
            &format!("manifests/{:020}/_SUCCESS", receipt.recovery.generation),
        );
        if let Err(e) = put_exact(fs, &path, b"") {
            receipt.marker_error = Some(MarkerError {
                kind: e.kind(),
                message: e.to_string(),
            });
        }
    }
    receipt
}
fn read_generation(
    fs: &SharedFileSystem,
    root: &str,
    g: u64,
    options: &SinkOptions,
) -> Result<Option<(Manifest, Checksum)>> {
    read_snapshot(fs, root, Selection::Generation(g), options)
}
fn latest(
    fs: &SharedFileSystem,
    root: &str,
    options: &SinkOptions,
) -> Result<Option<(Manifest, Checksum)>> {
    read_snapshot(fs, root, Selection::Latest, options)
}
fn read_snapshot(
    fs: &SharedFileSystem,
    root: &str,
    selection: Selection,
    options: &SinkOptions,
) -> Result<Option<(Manifest, Checksum)>> {
    let reader = ManifestReader::new(
        fs.clone(),
        root,
        ReadLimits {
            max_manifest_bytes: options.max_manifest_bytes,
            ..ReadLimits::default()
        },
    )?;
    match reader.read(selection) {
        Ok(s) => Ok(Some((s.manifest().clone(), s.checksum()))),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
fn check_options(fs: &SharedFileSystem, options: &SinkOptions) -> Result<()> {
    if !fs.io_capabilities().write_missing {
        return Err(error(
            ErrorKind::NotSupported,
            "store must support atomic create-if-absent",
        ));
    }
    if options.max_manifest_bytes == 0
        || options.max_manifest_bytes > MAX_MANIFEST_BYTES
        || options.max_history_reads == 0
        || options.parts.target_part_bytes == 0
        || options.parts.target_part_bytes >= options.parts.memory_limit
        || options.parts.memory_limit == usize::MAX
    {
        return Err(error(ErrorKind::Invalid, "invalid sink metadata limits"));
    }
    Ok(())
}

// The visibility decision always uses exact stored bytes. Absence after an
// ambiguous result cannot prove that the submitted request will never complete.
fn put_exact(fs: &SharedFileSystem, path: &str, bytes: &[u8]) -> Result<()> {
    if let Some((parent, _)) = path.rsplit_once('/') {
        if let Err(e) = fs.mkdir(parent, true) {
            if e.kind() != ErrorKind::AlreadyExists {
                return Err(e.into());
            }
        }
    }
    let result = fs.write(
        path,
        bytes,
        WriteOptions {
            condition: Some(Condition::Missing),
        },
    );
    if let Err(e) = &result {
        if !matches!(
            e.kind(),
            ErrorKind::Conflict
                | ErrorKind::AlreadyExists
                | ErrorKind::UnknownOutcome
                | ErrorKind::Timeout
                | ErrorKind::Unavailable
                | ErrorKind::Io
        ) {
            return result.map(|_| ());
        }
    }
    match bounded_read(fs, path, bytes.len()) {
        Ok(actual) if actual == bytes => Ok(()),
        Ok(_) => Err(error(
            ErrorKind::Conflict,
            "immutable key contains different bytes",
        )),
        Err(e) => Err(error(
            ErrorKind::UnknownOutcome,
            format!("cannot resolve create-if-absent: {e}"),
        )),
    }
}

fn metadata_fits(manifest: &Manifest, limit: usize) -> Result<()> {
    struct Counter {
        remaining: usize,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.remaining = self
                .remaining
                .checked_sub(bytes.len())
                .ok_or_else(|| io::Error::other("manifest limit exceeded"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter { remaining: limit }, manifest)
        .map_err(|e| error(ErrorKind::Invalid, e.to_string()))
}
pub(crate) fn error(kind: ErrorKind, message: impl Into<String>) -> OperationError {
    OperationError::Detailed {
        kind,
        message: message.into(),
        retry_safe: false,
    }
}
fn operation(error: InterchangeError) -> OperationError {
    match error {
        InterchangeError::Operation(e) => e,
        InterchangeError::FileSystem(e) => e.into(),
        other => self::error(ErrorKind::Invalid, other.to_string()),
    }
}
