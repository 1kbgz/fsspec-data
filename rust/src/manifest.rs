//! Versioned manifests and synchronous, listing-free snapshot reads.
//!
//! Publication requires immutable parts, consecutive immutable manifest generations,
//! and consistent reads of missing keys. This module reads that protocol; it does not
//! publish tables or certify a filesystem's guarantees. Part verification is lazy.
//!
//! ```no_run
//! use std::sync::Arc;
//! use fsspec_data::manifest::{ManifestReader, ReadLimits, Selection};
//! use fsspec_rs::LocalFs;
//!
//! let reader = ManifestReader::new(
//!     Arc::new(LocalFs::new()), "/data/events", ReadLimits::default(),
//! )?;
//! let snapshot = reader.read(Selection::Latest)?;
//! for index in 0..snapshot.manifest().parts.len() {
//!     let encoded = snapshot.read_part(index)?;
//!     // Pass verified bytes to the codec recorded in the part metadata.
//! }
//! # Ok::<(), fsspec_rs::OperationError>(())
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Read;

use arrow_schema::Schema;
use fsspec_rs::{ErrorKind, FsError, OpenMode, OperationError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::SharedFileSystem;

type Result<T> = std::result::Result<T, OperationError>;

/// Maximum encoded size of a version-one manifest (8 MiB).
pub const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;

/// SHA-256 of exact stored bytes, serialized as 64 lowercase hexadecimal digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Checksum(pub(crate) [u8; 32]);

impl Checksum {
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

impl fmt::Display for Checksum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl From<Checksum> for String {
    fn from(value: Checksum) -> Self {
        value.to_string()
    }
}

impl TryFrom<String> for Checksum {
    type Error = &'static str;
    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("SHA-256 must contain 64 lowercase hexadecimal digits");
        }
        let mut bytes = [0; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&value[2 * index..2 * index + 2], 16)
                .map_err(|_| "invalid SHA-256")?;
        }
        Ok(Self(bytes))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parent {
    pub generation: u64,
    pub sha256: Checksum,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationKind {
    Create,
    Append,
    Replace,
    Finish,
    Delete,
    Cleanup,
}

/// Ordered logical progress interval. Units are declared once by the manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Span {
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Part {
    pub key: String,
    pub size: u64,
    pub sha256: Checksum,
    pub num_rows: u64,
    pub progress: Span,
    pub encoding: String,
    pub options: BTreeMap<String, Value>,
}

/// A batch fingerprint is distinct from a checksum of an encoded part.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchReceipt {
    pub key: String,
    pub progress: Span,
    pub fingerprint: Checksum,
}

/// Version-one JSON document. Schema uses Arrow 60's serde schema representation.
/// Receipts describe only the batches introduced by this publication.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format_version: u32,
    pub namespace_id: String,
    pub table_id: String,
    pub generation: u64,
    pub parent: Option<Parent>,
    pub publication_id: String,
    pub kind: PublicationKind,
    pub schema: Schema,
    pub partition_values: BTreeMap<String, Value>,
    pub complete: bool,
    pub deleted: bool,
    pub num_rows: u64,
    pub progress_unit: String,
    pub progress: u64,
    pub parts: Vec<Part>,
    pub receipts: Vec<BatchReceipt>,
}

fn error(kind: ErrorKind, message: impl Into<String>) -> OperationError {
    OperationError::Detailed {
        kind,
        message: message.into(),
        retry_safe: false,
    }
}

fn check(valid: bool, message: &str) -> Result<()> {
    if valid {
        Ok(())
    } else {
        Err(error(ErrorKind::Corrupt, message))
    }
}

pub(crate) fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub(crate) fn part_generation(key: &str) -> Result<(u64, &str)> {
    let components: Vec<_> = key.split('/').collect();
    check(
        components.len() == 4 && components[0] == "parts",
        "invalid part key",
    )?;
    let (number, attempt, name) = (components[1], components[2], components[3]);
    check(
        number.len() == 20 && number.bytes().all(|b| b.is_ascii_digit()),
        "invalid part generation",
    )?;
    let generation = number
        .parse()
        .map_err(|_| error(ErrorKind::Corrupt, "part generation overflow"))?;
    check(identifier(attempt), "invalid part publication ID")?;
    check(
        !name.is_empty()
            && name.len() <= 128
            && name != "."
            && name != ".."
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')),
        "invalid part filename",
    )?;
    Ok((generation, attempt))
}

impl Manifest {
    /// Validate and serialize. Checksums of parents must use the returned bytes,
    /// not a later reserialization of the document.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes =
            serde_json::to_vec(self).map_err(|e| error(ErrorKind::Corrupt, e.to_string()))?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(error(
                ErrorKind::Invalid,
                "manifest exceeds version-one byte limit",
            ));
        }
        Ok(bytes)
    }

    /// Parse a bounded version-one document and validate its local invariants.
    /// Parent/namespace checks require a reader and are not performed here.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(error(
                ErrorKind::Invalid,
                "manifest exceeds version-one byte limit",
            ));
        }
        #[derive(Deserialize)]
        struct Version {
            format_version: u32,
        }
        let version: Version =
            serde_json::from_slice(bytes).map_err(|e| error(ErrorKind::Corrupt, e.to_string()))?;
        if version.format_version != 1 {
            return Err(error(
                ErrorKind::NotSupported,
                "unsupported manifest format version",
            ));
        }
        let manifest: Self =
            serde_json::from_slice(bytes).map_err(|e| error(ErrorKind::Corrupt, e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<()> {
        if self.format_version != 1 {
            return Err(error(
                ErrorKind::NotSupported,
                "unsupported manifest format version",
            ));
        }
        check(
            identifier(&self.namespace_id)
                && identifier(&self.table_id)
                && identifier(&self.publication_id)
                && identifier(&self.progress_unit),
            "invalid manifest identity or progress unit",
        )?;
        match (&self.parent, self.generation) {
            (None, 0) => check(
                self.kind == PublicationKind::Create,
                "generation zero must create a table",
            )?,
            (Some(parent), g) if g > 0 => check(
                parent.generation == g - 1,
                "nonconsecutive parent generation",
            )?,
            _ => return Err(error(ErrorKind::Corrupt, "invalid parent reference")),
        }
        check(
            !self.deleted
                || (!self.complete
                    && self.parts.is_empty()
                    && self.receipts.is_empty()
                    && self.num_rows == 0
                    && self.progress == 0),
            "invalid tombstone state",
        )?;
        match self.kind {
            PublicationKind::Delete => check(self.deleted, "delete must publish a tombstone")?,
            PublicationKind::Cleanup => {}
            _ => check(
                !self.deleted,
                "publication kind disagrees with deletion state",
            )?,
        }
        check(
            self.kind != PublicationKind::Finish || self.complete,
            "finish must mark complete",
        )?;
        check(
            self.kind != PublicationKind::Append || !self.complete,
            "append cannot mark complete",
        )?;
        let (mut position, mut rows) = (0u64, 0u64);
        let mut keys = BTreeSet::new();
        for part in &self.parts {
            let (generation, publication) = part_generation(&part.key)?;
            check(
                generation <= self.generation
                    && (generation < self.generation || publication == self.publication_id),
                "part belongs to a different publication",
            )?;
            check(keys.insert(&part.key), "duplicate part key")?;
            check(
                part.size > 0 && part.progress.start == position && part.progress.end > position,
                "invalid part size or progress interval",
            )?;
            if self.progress_unit == "rows" {
                check(
                    part.num_rows == part.progress.end - position,
                    "part row progress mismatch",
                )?;
            }
            if !matches!(
                part.encoding.as_str(),
                "parquet" | "arrow" | "csv" | "jsonl" | "avro"
            ) {
                return Err(error(ErrorKind::NotSupported, "unsupported part encoding"));
            }
            position = part.progress.end;
            rows = rows
                .checked_add(part.num_rows)
                .ok_or_else(|| error(ErrorKind::Corrupt, "row count overflow"))?;
        }
        check(
            position == self.progress && rows == self.num_rows,
            "manifest totals disagree with parts",
        )?;
        let mut receipt_keys = BTreeSet::new();
        let mut end = None;
        for receipt in &self.receipts {
            check(
                !receipt.key.is_empty()
                    && receipt.key.len() <= 256
                    && receipt_keys.insert(&receipt.key),
                "invalid or duplicate receipt key",
            )?;
            check(
                receipt.progress.end > receipt.progress.start
                    && end.is_none_or(|p| receipt.progress.start == p),
                "invalid receipt progress interval",
            )?;
            end = Some(receipt.progress.end);
        }
        check(
            end.is_none_or(|end| end == self.progress),
            "receipts do not end at manifest progress",
        )?;
        if matches!(
            self.kind,
            PublicationKind::Create | PublicationKind::Replace
        ) {
            self.validate_receipt_start(0)?;
        }
        if self.kind == PublicationKind::Cleanup {
            check(
                self.receipts.is_empty(),
                "cleanup cannot introduce batch receipts",
            )?;
        }
        Ok(())
    }

    fn validate_receipt_start(&self, start: u64) -> Result<()> {
        check(
            self.receipts
                .first()
                .map_or(self.progress == start, |r| r.progress.start == start),
            "receipts do not cover new progress",
        )
    }

    pub(crate) fn validate_parent(&self, parent: &Manifest) -> Result<()> {
        check(
            self.namespace_id == parent.namespace_id
                && self.partition_values == parent.partition_values,
            "namespace or partition values changed",
        )?;
        check(
            self.publication_id != parent.publication_id,
            "publication identity reused",
        )?;
        match self.kind {
            PublicationKind::Create | PublicationKind::Replace => {
                check(
                    self.table_id != parent.table_id,
                    "replacement reused table identity",
                )?;
                check(
                    parent.deleted == (self.kind == PublicationKind::Create),
                    "invalid create/replace transition",
                )?;
            }
            PublicationKind::Append | PublicationKind::Finish => {
                check(
                    !parent.deleted
                        && !parent.complete
                        && self.table_id == parent.table_id
                        && self.schema == parent.schema
                        && self.progress_unit == parent.progress_unit,
                    "append/finish changed or reopened a table",
                )?;
                check(
                    self.parts.starts_with(&parent.parts),
                    "append/finish changed existing parts",
                )?;
                self.validate_receipt_start(parent.progress)?;
            }
            PublicationKind::Cleanup => {
                check(
                    self.table_id == parent.table_id
                        && self.schema == parent.schema
                        && self.progress_unit == parent.progress_unit
                        && self.parts == parent.parts
                        && self.complete == parent.complete
                        && self.deleted == parent.deleted,
                    "cleanup changed table state",
                )?;
            }
            PublicationKind::Delete => {
                check(
                    !parent.deleted && self.table_id != parent.table_id,
                    "invalid deletion transition",
                )?;
            }
        }
        let inherited: BTreeMap<_, _> = parent.parts.iter().map(|p| (&p.key, p)).collect();
        for part in &self.parts {
            if part_generation(&part.key)?.0 < self.generation {
                check(
                    inherited.get(&part.key) == Some(&part),
                    "part was not inherited from the parent",
                )?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Selection {
    Latest,
    Generation(u64),
}

/// Encoded byte and request limits, not process-wide memory or transport deadlines.
#[derive(Clone, Copy, Debug)]
pub struct ReadLimits {
    pub max_manifest_bytes: usize,
    pub max_part_bytes: usize,
    pub max_manifest_reads: usize,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            max_manifest_bytes: MAX_MANIFEST_BYTES,
            max_part_bytes: 64 * 1024 * 1024,
            max_manifest_reads: 256,
        }
    }
}

impl ReadLimits {
    fn validate(&self) -> Result<()> {
        if self.max_manifest_bytes == 0
            || self.max_manifest_bytes > MAX_MANIFEST_BYTES
            || self.max_part_bytes == 0
            || self.max_part_bytes == usize::MAX
            || self.max_manifest_reads == 0
        {
            return Err(error(ErrorKind::Invalid, "invalid manifest read limits"));
        }
        Ok(())
    }
}

/// Uses a filesystem instance directly; no process-global snapshot/credential cache.
#[derive(Clone)]
pub struct ManifestReader {
    pub(crate) fs: crate::async_tables::ReadSource,
    root: String,
    limits: ReadLimits,
}

impl ManifestReader {
    pub fn new(fs: SharedFileSystem, root: impl Into<String>, limits: ReadLimits) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            fs: crate::async_tables::ReadSource::sync(fs),
            root: root.into(),
            limits,
        })
    }

    /// Borrow native async handles for reads executed by `read_async`/`scan_async`.
    /// The caller owns filesystem/context shutdown. Synchronous methods block their caller.
    pub fn new_async(
        fs: fsspec_rs::async_files::AsyncFilesystem,
        root: impl Into<String>,
        limits: ReadLimits,
    ) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            fs: crate::async_tables::ReadSource::asynchronous(fs),
            root: root.into(),
            limits,
        })
    }

    /// Read a validated metadata snapshot. Parts are verified by `Snapshot::read_part`.
    /// Validates generation zero and the selected/probed generations' immediate parents,
    /// not every historical manifest or receipt in the partition.
    pub fn read(&self, selection: Selection) -> Result<Snapshot> {
        let mut session = Session {
            reader: self,
            remaining: self.limits.max_manifest_reads,
        };
        let loaded = match selection {
            Selection::Generation(g) => {
                let loaded = session
                    .raw(g)?
                    .ok_or_else(|| error(ErrorKind::NotFound, "manifest generation not found"))?;
                let root = if g == 0 {
                    loaded.clone()
                } else {
                    session
                        .raw(0)?
                        .ok_or_else(|| error(ErrorKind::Corrupt, "missing generation zero"))?
                };
                session.validate(loaded, &root)?
            }
            Selection::Latest => {
                let root = session
                    .raw(0)?
                    .ok_or_else(|| error(ErrorKind::NotFound, "no published table"))?;
                let mut low = root.clone();
                loop {
                    if low.manifest.generation == u64::MAX {
                        break low;
                    }
                    let mut distance = 1u64;
                    let mut high;
                    loop {
                        high = low.manifest.generation.saturating_add(distance);
                        match session.candidate(high, &root)? {
                            Some(found) => {
                                low = found;
                                if high == u64::MAX {
                                    break;
                                }
                                distance = distance.saturating_mul(2);
                            }
                            None => break,
                        }
                    }
                    if low.manifest.generation == u64::MAX {
                        break low;
                    }
                    while high - low.manifest.generation > 1 {
                        let mid = low.manifest.generation + (high - low.manifest.generation) / 2;
                        match session.candidate(mid, &root)? {
                            Some(found) => low = found,
                            None => high = mid,
                        }
                    }
                    // Publication may have filled the previously missing successor.
                    match session.candidate(high, &root)? {
                        Some(found) => low = found,
                        None => break low,
                    }
                }
            }
        };
        Ok(Snapshot {
            fs: self.fs.clone(),
            root: self.root.clone(),
            limits: self.limits,
            manifest: loaded.manifest,
            checksum: loaded.checksum,
        })
    }
}

/// Frozen metadata, bound to the filesystem and partition it was read from.
#[derive(Clone)]
pub struct Snapshot {
    pub(crate) fs: crate::async_tables::ReadSource,
    root: String,
    limits: ReadLimits,
    manifest: Manifest,
    checksum: Checksum,
}

impl Snapshot {
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    /// Authoritative visible state; unavailable timestamps/distinct values are omitted.
    pub fn state(&self) -> crate::Metadata {
        let m = &self.manifest;
        let mut state = serde_json::json!({
            "generation": m.generation, "num_rows": m.num_rows, "num_rows_exact": true,
            "complete": m.complete, "progress": m.progress, "progress_unit": m.progress_unit,
        })
        .as_object()
        .unwrap()
        .clone();
        for (column, value) in &m.partition_values {
            state.insert(format!("distinct.{column}"), serde_json::json!([value]));
        }
        state
    }

    pub fn checksum(&self) -> Checksum {
        self.checksum
    }

    /// Read and verify one full encoded part; does not decode rows or fetch other parts.
    pub fn read_part(&self, index: usize) -> Result<Vec<u8>> {
        let part = self
            .manifest
            .parts
            .get(index)
            .ok_or_else(|| error(ErrorKind::Invalid, "part index out of range"))?;
        if part.size > self.limits.max_part_bytes as u64 {
            return Err(error(
                ErrorKind::Invalid,
                "part exceeds configured read limit",
            ));
        }
        let bytes = self
            .fs
            .read(&join(&self.root, &part.key), part.size as usize)
            .map_err(|e| {
                if e.kind() == ErrorKind::NotFound {
                    error(ErrorKind::Corrupt, "manifest references a missing part")
                } else {
                    e
                }
            })?;
        check(
            bytes.len() as u64 == part.size && Checksum::of(&bytes) == part.sha256,
            "part size or SHA-256 mismatch",
        )?;
        Ok(bytes)
    }
}

#[derive(Clone)]
struct Loaded {
    manifest: Manifest,
    checksum: Checksum,
}

struct Session<'a> {
    reader: &'a ManifestReader,
    remaining: usize,
}

impl Session<'_> {
    fn raw(&mut self, generation: u64) -> Result<Option<Loaded>> {
        self.remaining = self.remaining.checked_sub(1).ok_or_else(|| {
            error(
                ErrorKind::Timeout,
                "manifest discovery read budget exhausted",
            )
        })?;
        let path = join(
            &self.reader.root,
            &format!("manifests/{generation:020}/manifest.json"),
        );
        let bytes = match self
            .reader
            .fs
            .read(&path, self.reader.limits.max_manifest_bytes)
        {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if bytes.len() > self.reader.limits.max_manifest_bytes {
            return Err(error(
                ErrorKind::Invalid,
                "manifest exceeds configured read limit",
            ));
        }
        let manifest = Manifest::decode(&bytes)?;
        check(
            manifest.generation == generation,
            "manifest stored at the wrong generation",
        )?;
        Ok(Some(Loaded {
            manifest,
            checksum: Checksum::of(&bytes),
        }))
    }

    fn candidate(&mut self, generation: u64, root: &Loaded) -> Result<Option<Loaded>> {
        self.raw(generation)?
            .map(|loaded| self.validate(loaded, root))
            .transpose()
    }

    fn validate(&mut self, loaded: Loaded, root: &Loaded) -> Result<Loaded> {
        check(
            loaded.manifest.namespace_id == root.manifest.namespace_id
                && loaded.manifest.partition_values == root.manifest.partition_values,
            "manifest disagrees with generation zero",
        )?;
        if let Some(reference) = &loaded.manifest.parent {
            let parent = if reference.generation == 0 {
                root.clone()
            } else {
                self.raw(reference.generation)?
                    .ok_or_else(|| error(ErrorKind::Corrupt, "missing parent manifest"))?
            };
            check(
                parent.checksum == reference.sha256,
                "parent SHA-256 mismatch",
            )?;
            loaded.manifest.validate_parent(&parent.manifest)?;
        }
        Ok(loaded)
    }
}

pub(crate) fn join(root: &str, key: &str) -> String {
    if root.is_empty() {
        key.to_owned()
    } else {
        format!("{}/{key}", root.trim_end_matches('/'))
    }
}

// Read through a handle: the generic filesystem cat_file implementation can read an
// unknown-size input completely before applying a requested range.
pub(crate) fn bounded_read(fs: &SharedFileSystem, path: &str, limit: usize) -> Result<Vec<u8>> {
    let file = fs.open(path, OpenMode::Read, None)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(FsError::from)?;
    Ok(bytes)
}
