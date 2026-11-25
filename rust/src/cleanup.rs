//! Retention-based removal of abandoned parts in occupied manifest generations.
//!
//! Published history is never pruned. Keep the collector alive between sweeps;
//! restarting it resets retention. The table namespace must be exclusively managed
//! by protocol-compliant writers, with immutable manifests and part keys.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant, SystemTime};

use fsspec_rs::{ErrorKind, FileInfo, OperationError};
use serde::Serialize;

use crate::manifest::{
    bounded_read, identifier, join, part_generation, Checksum, Manifest, ManifestReader,
    ReadLimits, Selection, MAX_MANIFEST_BYTES,
};
use crate::SharedFileSystem;

type Result<T> = std::result::Result<T, OperationError>;

/// Bounds on history, retained keys, and entries returned across directory listings.
/// The filesystem may allocate a complete listing before these limits are checked.
#[derive(Clone, Copy, Debug)]
pub struct CleanupLimits {
    pub max_manifest_bytes: usize,
    pub max_generations: usize,
    pub max_entries: usize,
}

impl Default for CleanupLimits {
    fn default() -> Self {
        Self {
            max_manifest_bytes: MAX_MANIFEST_BYTES,
            max_generations: 256,
            max_entries: 100_000,
        }
    }
}

/// Relative keys eligible for deletion and keys successfully removed this sweep.
#[derive(Debug, Serialize)]
pub struct CleanupReport {
    pub generation: Option<u64>,
    pub pending: usize,
    pub eligible: Vec<String>,
    pub deleted: Vec<String>,
}

struct Observation {
    since: Instant,
    size: u64,
    modified: Option<SystemTime>,
}

pub struct OrphanCollector {
    fs: SharedFileSystem,
    root: String,
    retention: Duration,
    limits: CleanupLimits,
    namespace: Option<String>,
    observed: BTreeMap<String, Observation>,
}

fn error(kind: ErrorKind, message: &str) -> OperationError {
    OperationError::Detailed {
        kind,
        message: message.into(),
        retry_safe: false,
    }
}

impl OrphanCollector {
    pub fn new(
        fs: SharedFileSystem,
        root: impl Into<String>,
        retention: Duration,
        limits: CleanupLimits,
    ) -> Result<Self> {
        if retention.is_zero()
            || limits.max_manifest_bytes == 0
            || limits.max_manifest_bytes > MAX_MANIFEST_BYTES
            || limits.max_generations == 0
            || limits.max_entries == 0
        {
            return Err(error(
                ErrorKind::Invalid,
                "invalid cleanup limits or retention",
            ));
        }
        let root = root.into();
        let root = if fs.sep() == "\\" {
            root.replace('\\', "/")
        } else {
            root
        };
        Ok(Self {
            fs,
            root,
            retention,
            limits,
            namespace: None,
            observed: BTreeMap::new(),
        })
    }

    /// Validate all history through a published boundary, then observe or delete
    /// unreferenced files in occupied generations. Dry runs also track retention.
    /// No manifests or directories are created or removed. Errors before deletion
    /// leave storage untouched; a deletion error may follow successful deletions.
    /// Repeating the sweep is safe after either kind of error.
    pub fn collect(&mut self, dry_run: bool) -> Result<CleanupReport> {
        self.collect_with_clock(dry_run, Instant::now)
    }

    fn collect_with_clock(
        &mut self,
        dry_run: bool,
        clock: impl FnOnce() -> Instant,
    ) -> Result<CleanupReport> {
        let result = self.sweep(dry_run, clock);
        if result.is_err() {
            // Missing observations or failed validation must not advance retention.
            self.observed.clear();
        }
        result
    }

    fn sweep(&mut self, dry_run: bool, clock: impl FnOnce() -> Instant) -> Result<CleanupReport> {
        let reader = ManifestReader::new(
            self.fs.clone(),
            &self.root,
            ReadLimits {
                max_manifest_bytes: self.limits.max_manifest_bytes,
                ..ReadLimits::default()
            },
        )?;
        let snapshot = match reader.read(Selection::Latest) {
            Ok(snapshot) => snapshot,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                self.observed.clear();
                return Ok(CleanupReport {
                    generation: None,
                    pending: 0,
                    eligible: vec![],
                    deleted: vec![],
                });
            }
            Err(e) => return Err(e),
        };
        let boundary = snapshot.manifest().generation;
        if boundary >= self.limits.max_generations as u64 {
            return Err(error(ErrorKind::Invalid, "cleanup history limit exceeded"));
        }
        let mut protected = BTreeSet::new();
        let mut parent: Option<(Manifest, Checksum)> = None;
        for generation in 0..=boundary {
            let bytes = bounded_read(
                &self.fs,
                &join(
                    &self.root,
                    &format!("manifests/{generation:020}/manifest.json"),
                ),
                self.limits.max_manifest_bytes,
            )?;
            if bytes.len() > self.limits.max_manifest_bytes {
                return Err(error(
                    ErrorKind::Invalid,
                    "manifest exceeds cleanup byte limit",
                ));
            }
            let manifest = Manifest::decode(&bytes)?;
            let checksum = Checksum::of(&bytes);
            if manifest.generation != generation {
                return Err(error(
                    ErrorKind::Corrupt,
                    "manifest stored at wrong generation",
                ));
            }
            if let Some((previous, checksum)) = &parent {
                if manifest.parent.as_ref().map(|p| p.sha256) != Some(*checksum) {
                    return Err(error(ErrorKind::Corrupt, "parent SHA-256 mismatch"));
                }
                manifest.validate_parent(previous)?;
            }
            for key in manifest
                .parts
                .iter()
                .map(|p| p.key.clone())
                .chain(std::iter::once(format!(
                    "parts/{generation:020}/{}/candidate",
                    manifest.publication_id
                )))
            {
                protected.insert(key);
                if protected.len() > self.limits.max_entries {
                    return Err(error(
                        ErrorKind::Invalid,
                        "cleanup reference limit exceeded",
                    ));
                }
            }
            parent = Some((manifest, checksum));
        }
        if parent.as_ref().map(|p| p.1) != Some(snapshot.checksum()) {
            return Err(error(
                ErrorKind::Corrupt,
                "published manifest changed during cleanup",
            ));
        }
        let namespace = &snapshot.manifest().namespace_id;
        if self.namespace.as_ref() != Some(namespace) {
            self.observed.clear();
            self.namespace = Some(namespace.clone());
        }

        // List only the fixed protocol layout, never recursively follow arbitrary paths.
        let mut remaining = self.limits.max_entries;
        let mut files = BTreeMap::new();
        for directory in self.list("parts", &mut remaining)? {
            let Some(number) = self.child_name(&directory, "parts") else {
                continue;
            };
            if !directory.is_dir()
                || number.len() != 20
                || !number.bytes().all(|b| b.is_ascii_digit())
            {
                continue;
            }
            let Ok(generation) = number.parse::<u64>() else {
                continue;
            };
            if generation > boundary {
                continue;
            }
            let generation_key = format!("parts/{number}");
            for attempt in self.list(&generation_key, &mut remaining)? {
                let Some(id) = self.child_name(&attempt, &generation_key) else {
                    continue;
                };
                if !attempt.is_dir() || !identifier(id) {
                    continue;
                }
                let attempt_key = format!("{generation_key}/{id}");
                for file in self.list(&attempt_key, &mut remaining)? {
                    let Some(name) = self.child_name(&file, &attempt_key) else {
                        continue;
                    };
                    let key = format!("{attempt_key}/{name}");
                    if file.is_file() && part_generation(&key).is_ok() && !protected.contains(&key)
                    {
                        files.insert(key, file);
                    }
                }
            }
        }
        self.observed.retain(|key, _| files.contains_key(key));
        let mut report = CleanupReport {
            generation: Some(boundary),
            pending: 0,
            eligible: vec![],
            deleted: vec![],
        };
        let now = clock();
        for (key, file) in files {
            let observation = self.observed.entry(key.clone()).or_insert(Observation {
                since: now,
                size: file.size,
                modified: file.modified,
            });
            if observation.size != file.size || observation.modified != file.modified {
                *observation = Observation {
                    since: now,
                    size: file.size,
                    modified: file.modified,
                };
            }
            if now.duration_since(observation.since) >= self.retention {
                report.eligible.push(key);
            } else {
                report.pending += 1;
            }
        }
        if !dry_run {
            for key in &report.eligible {
                match self.fs.rm_file(&join(&self.root, key)) {
                    Ok(()) => report.deleted.push(key.clone()),
                    Err(e) => {
                        let e = OperationError::from(e);
                        if e.kind() != ErrorKind::NotFound {
                            return Err(e);
                        }
                    }
                }
                self.observed.remove(key);
            }
        }
        Ok(report)
    }

    fn normalize_listing_path(&self, path: &str) -> String {
        let path = self.fs.strip_protocol(path);
        if self.fs.sep() == "\\" {
            path.replace('\\', "/")
        } else {
            path
        }
    }

    fn child_name<'a>(&self, entry: &'a FileInfo, parent: &str) -> Option<&'a str> {
        child_name(
            entry,
            &self.normalize_listing_path(&join(&self.root, parent)),
        )
    }

    fn list(&self, key: &str, remaining: &mut usize) -> Result<Vec<FileInfo>> {
        let mut entries = match self.fs.ls(&join(&self.root, key), true) {
            Ok(entries) => entries,
            Err(e) => {
                let e = OperationError::from(e);
                return if e.kind() == ErrorKind::NotFound {
                    Ok(vec![])
                } else {
                    Err(e)
                };
            }
        };
        for entry in &mut entries {
            entry.name = self.normalize_listing_path(&entry.name);
        }
        *remaining = remaining
            .checked_sub(entries.len())
            .ok_or_else(|| error(ErrorKind::Invalid, "cleanup listing limit exceeded"))?;
        Ok(entries)
    }
}

fn child_name<'a>(entry: &'a FileInfo, parent: &str) -> Option<&'a str> {
    let name = entry.name.strip_prefix(parent)?.strip_prefix('/')?;
    // S3 directory names can include their trailing delimiter.
    let name = if entry.is_dir() {
        name.trim_end_matches('/')
    } else {
        name
    };
    (!name.is_empty() && !name.contains('/') && name != "." && name != "..").then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::{recover, FileDataSink, RecoveryAction, SinkOptions, WriteMode};
    use arrow::array::{ArrayRef, Int64Array, RecordBatch};
    use fsspec_rs::{
        FileSystem, FsError, FsFile, FsResult, IoCapabilities, MemoryFs, OpenMode, OpenOptions,
        WriteOptions, WriteResult,
    };
    use std::sync::{Arc, Mutex};

    struct Store {
        inner: MemoryFs,
        root: String,
        delete_fail_after: Mutex<Option<usize>>,
        list_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        fail_listing: Mutex<bool>,
    }
    impl Store {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                inner: MemoryFs::new(),
                root: format!("/{}", uuid::Uuid::new_v4()),
                delete_fail_after: Mutex::new(None),
                list_hook: Mutex::new(None),
                fail_listing: Mutex::new(false),
            })
        }
        fn writer(self: &Arc<Self>, mode: WriteMode) -> FileDataSink {
            let batch = RecordBatch::try_from_iter([(
                "value",
                Arc::new(Int64Array::from(vec![1])) as ArrayRef,
            )])
            .unwrap();
            let mut writer = FileDataSink::open(
                self.clone(),
                &self.root,
                mode,
                batch.schema(),
                SinkOptions::default(),
            )
            .unwrap();
            writer
                .write_batch(writer.position(), &uuid::Uuid::new_v4().to_string(), &batch)
                .unwrap();
            writer.flush().unwrap();
            writer
        }
        fn collector(self: &Arc<Self>) -> OrphanCollector {
            OrphanCollector::new(
                self.clone(),
                &self.root,
                Duration::from_secs(60),
                CleanupLimits::default(),
            )
            .unwrap()
        }
        fn put(&self, key: &str, bytes: &[u8]) {
            self.inner
                .write(&join(&self.root, key), bytes, WriteOptions::default())
                .unwrap();
        }
        fn exists(&self, key: &str) -> bool {
            self.inner.info(&join(&self.root, key)).is_ok()
        }
    }
    impl FileSystem for Store {
        fn protocol(&self) -> &[&str] {
            &["memory"]
        }
        fn io_capabilities(&self) -> IoCapabilities {
            self.inner.io_capabilities()
        }
        fn ls(&self, p: &str, detail: bool) -> FsResult<Vec<FileInfo>> {
            let hook = self.list_hook.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
            if *self.fail_listing.lock().unwrap() {
                return Err(FsError::PermissionDenied(p.into()));
            }
            self.inner.ls(p, detail)
        }
        fn info(&self, p: &str) -> FsResult<FileInfo> {
            self.inner.info(p)
        }
        fn open(&self, p: &str, m: OpenMode, o: Option<OpenOptions>) -> FsResult<Box<dyn FsFile>> {
            self.inner.open(p, m, o)
        }
        fn mkdir(&self, p: &str, parents: bool) -> FsResult<()> {
            self.inner.mkdir(p, parents)
        }
        fn rmdir(&self, p: &str) -> FsResult<()> {
            self.inner.rmdir(p)
        }
        fn rm_file(&self, p: &str) -> FsResult<()> {
            if let Some(remaining) = self.delete_fail_after.lock().unwrap().as_mut() {
                if *remaining == 0 {
                    return Err(FsError::PermissionDenied(p.into()));
                }
                *remaining -= 1;
            }
            self.inner.rm_file(p)
        }
        fn cp_file(&self, a: &str, b: &str) -> FsResult<()> {
            self.inner.cp_file(a, b)
        }
        fn write(&self, p: &str, b: &[u8], o: WriteOptions) -> Result<WriteResult> {
            self.inner.write(p, b, o)
        }
    }
    fn sweep(
        collector: &mut OrphanCollector,
        dry: bool,
        start: Instant,
        seconds: u64,
    ) -> CleanupReport {
        collector
            .collect_with_clock(dry, || start + Duration::from_secs(seconds))
            .unwrap()
    }

    #[test]
    fn retains_history_and_recovery_records_after_replacement_and_deletion() {
        let store = Store::new();
        let mut winner = store.writer(WriteMode::Create);
        let mut loser = store.writer(WriteMode::Create);
        winner.commit().unwrap();
        let token = loser.recovery_token();
        assert_eq!(loser.commit().unwrap_err().kind(), ErrorKind::Conflict);
        let candidate = bounded_read(
            &(store.clone() as SharedFileSystem),
            &join(
                &store.root,
                &format!(
                    "parts/{:020}/{}/candidate",
                    token.generation, token.publication_id
                ),
            ),
            MAX_MANIFEST_BYTES,
        )
        .unwrap();
        store.writer(WriteMode::Append).finish().unwrap();
        store.writer(WriteMode::Replace).finish().unwrap();
        let reader =
            ManifestReader::new(store.clone(), &store.root, ReadLimits::default()).unwrap();
        let snapshots: Vec<_> = (0..3)
            .map(|g| reader.read(Selection::Generation(g)).unwrap())
            .collect();
        let previous = snapshots.last().unwrap();
        let mut tombstone = previous.manifest().clone();
        tombstone.generation = 3;
        tombstone.parent = Some(crate::manifest::Parent {
            generation: 2,
            sha256: previous.checksum(),
        });
        tombstone.publication_id = "deletion".into();
        tombstone.table_id = "deleted-table".into();
        tombstone.kind = crate::manifest::PublicationKind::Delete;
        tombstone.deleted = true;
        tombstone.complete = false;
        tombstone.parts.clear();
        tombstone.receipts.clear();
        tombstone.num_rows = 0;
        tombstone.progress = 0;
        store.put(
            "manifests/00000000000000000003/manifest.json",
            &tombstone.encode().unwrap(),
        );
        let mut collector = store.collector();
        let start = Instant::now();
        let first = sweep(&mut collector, false, start, 0);
        assert_eq!(first.generation, Some(3));
        assert_eq!(first.pending, 2);
        assert!(first.deleted.is_empty());
        assert!(sweep(&mut collector, false, start, 59).eligible.is_empty());
        let dry = sweep(&mut collector, true, start, 60);
        assert_eq!(dry.eligible.len(), 2);
        assert!(dry.deleted.is_empty());
        assert!(dry.eligible.iter().all(|key| store.exists(key)));
        assert_eq!(
            sweep(&mut collector, false, start, 61).deleted,
            dry.eligible
        );
        // A request queued before cleanup still cannot replace the occupied manifest.
        assert_eq!(
            store
                .inner
                .write(
                    &join(&store.root, "manifests/00000000000000000000/manifest.json"),
                    &candidate,
                    WriteOptions {
                        condition: Some(fsspec_rs::Condition::Missing)
                    },
                )
                .unwrap_err()
                .kind(),
            ErrorKind::Conflict
        );
        for snapshot in snapshots {
            for index in 0..snapshot.manifest().parts.len() {
                snapshot.read_part(index).unwrap();
            }
            assert!(store.exists(&format!(
                "parts/{:020}/{}/candidate",
                snapshot.manifest().generation,
                snapshot.manifest().publication_id
            )));
        }
        assert_eq!(
            recover(
                store.clone(),
                &token,
                &SinkOptions::default(),
                RecoveryAction::Retry
            )
            .unwrap_err()
            .kind(),
            ErrorKind::Conflict
        );
        assert!(sweep(&mut collector, false, start, 120).eligible.is_empty());
    }

    #[test]
    fn open_generations_and_unpublished_tables_are_never_collected() {
        let store = Store::new();
        let mut winner = store.writer(WriteMode::Create);
        let mut collector = store.collector();
        let start = Instant::now();
        assert_eq!(sweep(&mut collector, false, start, 0).generation, None);
        winner.commit().unwrap();
        let _paused = store.writer(WriteMode::Append);
        assert_eq!(sweep(&mut collector, false, start, 100).pending, 0);
        assert!(sweep(&mut collector, false, start, 200).deleted.is_empty());
        store.writer(WriteMode::Append).commit().unwrap();
        assert_eq!(sweep(&mut collector, false, start, 201).pending, 1);
        assert_eq!(sweep(&mut collector, false, start, 261).deleted.len(), 1);
    }

    #[test]
    fn late_uploads_restarts_and_changed_metadata_restart_retention() {
        let store = Store::new();
        store.writer(WriteMode::Create).commit().unwrap();
        let key = "parts/00000000000000000000/loser/late.parquet";
        let mut collector = store.collector();
        let start = Instant::now();
        sweep(&mut collector, true, start, 0);
        store.put(key, b"late");
        assert_eq!(sweep(&mut collector, false, start, 60).pending, 1);
        collector = store.collector();
        assert_eq!(sweep(&mut collector, false, start, 120).pending, 1);
        store.put(key, b"recreated");
        assert_eq!(sweep(&mut collector, false, start, 180).pending, 1);
        assert_eq!(sweep(&mut collector, false, start, 240).deleted, vec![key]);
        store.put(key, b"retried upload");
        assert_eq!(sweep(&mut collector, false, start, 300).pending, 1);
    }

    #[test]
    fn publication_during_listing_keeps_new_parts_safe() {
        let store = Store::new();
        store.writer(WriteMode::Create).commit().unwrap();
        let key = "parts/00000000000000000000/loser/file.parquet";
        store.put(key, b"orphan");
        let mut collector = store.collector();
        let start = Instant::now();
        sweep(&mut collector, true, start, 0);
        let mut writer = store.writer(WriteMode::Append);
        *store.list_hook.lock().unwrap() = Some(Box::new(move || {
            writer.commit().unwrap();
        }));
        assert_eq!(sweep(&mut collector, false, start, 60).deleted, vec![key]);
        let snapshot = ManifestReader::new(store.clone(), &store.root, ReadLimits::default())
            .unwrap()
            .read(Selection::Latest)
            .unwrap();
        assert_eq!(snapshot.manifest().generation, 1);
        for index in 0..snapshot.manifest().parts.len() {
            snapshot.read_part(index).unwrap();
        }
    }

    #[test]
    fn invalid_history_listing_and_limits_prevent_deletions() {
        for failure in [
            "history",
            "listing",
            "entries",
            "list_entries",
            "generations",
            "bytes",
        ] {
            let store = Store::new();
            store.writer(WriteMode::Create).commit().unwrap();
            for _ in 0..6 {
                store.writer(WriteMode::Replace).commit().unwrap();
            }
            let key = "parts/00000000000000000000/loser/file.parquet";
            store.put(key, b"orphan");
            let mut collector = store.collector();
            let start = Instant::now();
            sweep(&mut collector, true, start, 0);
            match failure {
                "history" => store.put("manifests/00000000000000000002/manifest.json", b"corrupt"),
                "listing" => *store.fail_listing.lock().unwrap() = true,
                "entries" => collector.limits.max_entries = 1,
                "list_entries" => collector.limits.max_entries = 14,
                "generations" => collector.limits.max_generations = 1,
                "bytes" => collector.limits.max_manifest_bytes = 1,
                _ => unreachable!(),
            }
            assert!(
                collector
                    .collect_with_clock(false, || start + Duration::from_secs(60))
                    .is_err(),
                "{failure}"
            );
            assert!(store.exists(key));
            assert!(collector.observed.is_empty());
        }
    }

    #[test]
    fn relative_memory_roots_match_absolute_listing_names() {
        let store = Store::new();
        store.writer(WriteMode::Create).commit().unwrap();
        store.put("parts/00000000000000000000/loser/file", b"orphan");
        let mut collector = OrphanCollector::new(
            Arc::new(store.inner.clone()),
            store.root.trim_start_matches('/'),
            Duration::from_secs(1),
            CleanupLimits::default(),
        )
        .unwrap();
        let start = Instant::now();
        assert_eq!(sweep(&mut collector, true, start, 0).pending, 1);
        assert_eq!(sweep(&mut collector, false, start, 1).deleted.len(), 1);
    }

    #[test]
    fn interrupted_deletion_can_be_repeated_after_retention() {
        let store = Store::new();
        store.writer(WriteMode::Create).commit().unwrap();
        for name in ["a", "b"] {
            store.put(
                &format!("parts/00000000000000000000/loser/{name}"),
                b"orphan",
            );
        }
        let mut collector = store.collector();
        let start = Instant::now();
        sweep(&mut collector, true, start, 0);
        *store.delete_fail_after.lock().unwrap() = Some(1);
        assert_eq!(
            collector
                .collect_with_clock(false, || start + Duration::from_secs(60))
                .unwrap_err()
                .kind(),
            ErrorKind::PermissionDenied
        );
        *store.delete_fail_after.lock().unwrap() = None;
        assert_eq!(sweep(&mut collector, false, start, 61).pending, 1);
        assert_eq!(sweep(&mut collector, false, start, 121).deleted.len(), 1);
    }
}
