//! Source-owned raw range sharing. Each decoder retains its own cursor and output buffers.
use std::io::SeekFrom;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fsspec_data::CancellationToken;
use fsspec_rs::async_files::AsyncFile;
use fsspec_rs::async_io::{Operation, Value};
use fsspec_rs::{AsyncFsFile, ErrorKind, FileInfo, IoFuture, OperationError};
use tokio::runtime::Handle;
use tokio::sync::{Mutex as AsyncMutex, Notify, Semaphore};

const MAX_READ: usize = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 1024;

fn error(kind: ErrorKind, message: &str) -> OperationError {
    OperationError::Detailed {
        kind,
        message: message.into(),
        retry_safe: false,
    }
}

#[derive(Clone)]
struct Failure {
    kind: ErrorKind,
    message: String,
    retry_safe: bool,
}
impl From<OperationError> for Failure {
    fn from(value: OperationError) -> Self {
        Self {
            kind: value.kind(),
            message: value.to_string(),
            retry_safe: value.retry_safe(),
        }
    }
}
impl Failure {
    fn error(&self) -> OperationError {
        OperationError::Detailed {
            kind: self.kind,
            message: self.message.clone(),
            retry_safe: self.retry_safe,
        }
    }
}

#[derive(Eq, PartialEq)]
struct Key {
    path: String,
    etag: String,
    size: u64,
}

struct Reader {
    file: AsyncFile,
    serial: AsyncMutex<()>,
    users: AtomicUsize,
}
impl Reader {
    async fn release(reader: Arc<Self>) -> Result<(), OperationError> {
        if reader.users.fetch_sub(1, Ordering::AcqRel) == 1 {
            let operation = reader.file.close(None)?;
            let result = operation.clone().await;
            operation.drain().await;
            result?;
        }
        Ok(())
    }

    async fn fetch(
        &self,
        range: Range<u64>,
        stop: &CancellationToken,
    ) -> Result<Vec<u8>, OperationError> {
        let _guard =
            match futures::future::select(Box::pin(self.serial.lock()), Box::pin(stop.cancelled()))
                .await
            {
                futures::future::Either::Left((guard, _)) => guard,
                futures::future::Either::Right(_) => {
                    return Err(error(ErrorKind::Cancelled, "shared read cancelled"))
                }
            };
        async fn run(
            operation: Operation,
            stop: &CancellationToken,
        ) -> Result<Value, OperationError> {
            use std::future::IntoFuture;
            let result = match futures::future::select(
                Box::pin(operation.clone().into_future()),
                Box::pin(stop.cancelled()),
            )
            .await
            {
                futures::future::Either::Left((result, _)) => result,
                futures::future::Either::Right(_) => {
                    Err(error(ErrorKind::Cancelled, "shared read cancelled"))
                }
            };
            operation.drain().await;
            result
        }
        run(self.file.seek(SeekFrom::Start(range.start), None)?, stop).await?;
        let Value::Bytes(data) = run(
            self.file.read((range.end - range.start) as usize, None)?,
            stop,
        )
        .await?
        else {
            unreachable!()
        };
        if data.len() != (range.end - range.start) as usize {
            return Err(error(
                ErrorKind::Corrupt,
                "shared range response length mismatch",
            ));
        }
        Ok(data)
    }
}

struct Entry {
    key: Arc<Key>,
    stop: CancellationToken,
    // All mutable entry data is protected by the cache's state mutex.
    data: Mutex<EntryData>,
    done: Notify,
}
struct EntryData {
    range: Range<u64>,
    started: bool,
    finished: bool,
    waiters: usize,
    used: u64,
    result: Option<Result<Arc<Vec<u8>>, Failure>>,
}
struct State {
    entries: Vec<Arc<Entry>>,
    bytes: usize,
    tick: u64,
    closed: bool,
}

pub(super) struct RangeCache {
    limit: usize,
    runtime: Handle,
    state: Mutex<State>,
    changed: Notify,
    fetches: Semaphore,
}

struct Lease {
    cache: Arc<RangeCache>,
    entry: Arc<Entry>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.cache.state.lock().unwrap();
        let mut entry = self.entry.data.lock().unwrap();
        entry.waiters -= 1;
        if entry.waiters == 0 && !entry.finished {
            self.entry.stop.cancel();
        }
        drop(entry);
        RangeCache::discard_failed(&mut state);
        self.cache.changed.notify_waiters();
    }
}

impl RangeCache {
    pub(super) fn new(limit: usize, runtime: Handle) -> Arc<Self> {
        Arc::new(Self {
            limit,
            runtime,
            state: Mutex::new(State {
                entries: vec![],
                bytes: 0,
                tick: 0,
                closed: false,
            }),
            changed: Notify::new(),
            fetches: Semaphore::new(8),
        })
    }

    fn discard_failed(state: &mut State) {
        state.entries.retain(|entry| {
            let mut data = entry.data.lock().unwrap();
            let remove = data.finished
                && data.waiters == 0
                && (state.closed
                    || entry.stop.is_cancelled()
                    || data.result.as_ref().is_some_and(Result::is_err));
            if remove {
                data.result = None;
                state.bytes -= (data.range.end - data.range.start) as usize;
            }
            !remove
        });
    }

    fn make_room(&self, state: &mut State, bytes: usize, additional_entry: bool) -> bool {
        while state.bytes + bytes > self.limit
            || (additional_entry && state.entries.len() >= MAX_ENTRIES)
        {
            let victim = state
                .entries
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    let data = entry.data.lock().unwrap();
                    (data.finished && data.waiters == 0).then_some((index, data.used))
                })
                .min_by_key(|(_, used)| *used);
            let Some((index, _)) = victim else {
                return false;
            };
            let entry = state.entries.remove(index);
            let mut data = entry.data.lock().unwrap();
            data.result = None;
            state.bytes -= (data.range.end - data.range.start) as usize;
        }
        true
    }

    fn acquire(
        self: &Arc<Self>,
        reader: &Arc<Reader>,
        key: &Arc<Key>,
        wanted: Range<u64>,
    ) -> Result<Option<Lease>, OperationError> {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err(error(ErrorKind::Cancelled, "range cache is closed"));
        }
        state.tick = state.tick.wrapping_add(1);
        let tick = state.tick;
        // Extend a pending overlapping/adjacent range only if no other entry covers the extension.
        let candidate = state
            .entries
            .iter()
            .find(|entry| {
                let data = entry.data.lock().unwrap();
                entry.key == *key
                    && !entry.stop.is_cancelled()
                    && !data.started
                    && data.range.start <= wanted.end
                    && wanted.start <= data.range.end
            })
            .cloned();
        if let Some(entry) = candidate {
            let old = entry.data.lock().unwrap().range.clone();
            let union = old.start.min(wanted.start)..old.end.max(wanted.end);
            let extra = (union.end - union.start - (old.end - old.start)) as usize;
            let overlaps = state.entries.iter().any(|other| {
                if Arc::ptr_eq(other, &entry) || other.key != *key {
                    return false;
                }
                let data = other.data.lock().unwrap();
                union.start < data.range.end && data.range.start < union.end
            });
            if union.end - union.start <= MAX_READ as u64
                && !overlaps
                && self.make_room(&mut state, extra, false)
            {
                state.bytes += extra;
                let mut data = entry.data.lock().unwrap();
                data.range = union;
                data.waiters += 1;
                data.used = tick;
                drop(data);
                return Ok(Some(Lease {
                    cache: self.clone(),
                    entry,
                }));
            }
        }
        if let Some(entry) = state
            .entries
            .iter()
            .find(|entry| {
                let data = entry.data.lock().unwrap();
                entry.key == *key
                    && !entry.stop.is_cancelled()
                    && data.result.as_ref().is_none_or(Result::is_ok)
                    && data.range.start <= wanted.start
                    && wanted.start < data.range.end
            })
            .cloned()
        {
            let mut data = entry.data.lock().unwrap();
            data.waiters += 1;
            data.used = tick;
            drop(data);
            return Ok(Some(Lease {
                cache: self.clone(),
                entry,
            }));
        }
        let end = state
            .entries
            .iter()
            .filter_map(|entry| {
                let data = entry.data.lock().unwrap();
                (entry.key == *key
                    && !entry.stop.is_cancelled()
                    && data.result.as_ref().is_none_or(Result::is_ok)
                    && wanted.start < data.range.start
                    && data.range.start < wanted.end)
                    .then_some(data.range.start)
            })
            .min()
            .unwrap_or(wanted.end);
        let length = (end - wanted.start) as usize;
        if !self.make_room(&mut state, length, true) {
            return Ok(None);
        }
        state.bytes += length;
        let entry = Arc::new(Entry {
            key: key.clone(),
            stop: CancellationToken::new(),
            done: Notify::new(),
            data: Mutex::new(EntryData {
                range: wanted.start..end,
                started: false,
                finished: false,
                waiters: 1,
                used: tick,
                result: None,
            }),
        });
        state.entries.push(entry.clone());
        let cache = self.clone();
        let job = entry.clone();
        reader.users.fetch_add(1, Ordering::Relaxed);
        let reader = reader.clone();
        self.runtime.spawn(async move {
            // A short dispatch window lets concurrent overlapping requests coalesce.
            tokio::time::sleep(Duration::from_millis(1)).await;
            let permit = match futures::future::select(
                Box::pin(cache.fetches.acquire()),
                Box::pin(job.stop.cancelled()),
            )
            .await
            {
                futures::future::Either::Left((permit, _)) => Some(permit.unwrap()),
                futures::future::Either::Right(_) => None,
            };
            let range = {
                let _state = cache.state.lock().unwrap();
                let mut data = job.data.lock().unwrap();
                data.started = true;
                data.range.clone()
            };
            let result = if job.stop.is_cancelled() {
                Err(error(ErrorKind::Cancelled, "shared read cancelled"))
            } else {
                reader.fetch(range, &job.stop).await
            };
            drop(permit);
            let closed = Reader::release(reader).await;
            let result = result
                .and_then(|data| closed.map(|_| Arc::new(data)))
                .map_err(Failure::from);
            {
                let mut state = cache.state.lock().unwrap();
                let mut data = job.data.lock().unwrap();
                data.result = Some(result);
                data.finished = true;
                drop(data);
                Self::discard_failed(&mut state);
            }
            job.done.notify_waiters();
            cache.changed.notify_waiters();
        });
        Ok(Some(Lease {
            cache: self.clone(),
            entry,
        }))
    }

    pub(super) async fn close(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            for entry in &state.entries {
                entry.stop.cancel();
            }
        }
        self.changed.notify_waiters();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self.state.lock().unwrap();
                Self::discard_failed(&mut state);
                if state.entries.is_empty() {
                    return;
                }
            }
            changed.await;
        }
    }

    pub(super) fn file(self: &Arc<Self>, file: AsyncFile, info: FileInfo) -> CachedFile {
        let key = Arc::new(Key {
            path: info.name.clone(),
            etag: info.extra["etag"].clone(),
            size: info.size,
        });
        CachedFile {
            cache: self.clone(),
            reader: Some(Arc::new(Reader {
                file,
                serial: AsyncMutex::new(()),
                users: AtomicUsize::new(1),
            })),
            key,
            info,
            position: 0,
            pending: None,
        }
    }
}

pub(super) struct CachedFile {
    cache: Arc<RangeCache>,
    reader: Option<Arc<Reader>>,
    key: Arc<Key>,
    info: FileInfo,
    position: u64,
    pending: Option<Arc<Entry>>,
}
impl Drop for CachedFile {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.take() {
            self.cache.runtime.spawn(async move {
                let _ = Reader::release(reader).await;
            });
        }
    }
}
impl AsyncFsFile for CachedFile {
    fn read(&mut self, max_bytes: usize) -> IoFuture<'_, Vec<u8>> {
        Box::pin(async move {
            let reader = self
                .reader
                .as_ref()
                .ok_or_else(|| error(ErrorKind::Invalid, "file is closed"))?;
            let length =
                (self.info.size.saturating_sub(self.position)).min(max_bytes as u64) as usize;
            if self.cache.limit == 0 {
                let result = reader
                    .fetch(
                        self.position..self.position + length as u64,
                        &CancellationToken::new(),
                    )
                    .await?;
                self.position += length as u64;
                return Ok(result);
            }
            let mut output = Vec::with_capacity(length);
            while output.len() < length {
                let start = self.position + output.len() as u64;
                let end =
                    start + (length - output.len()).min(self.cache.limit).min(MAX_READ) as u64;
                let lease = loop {
                    let changed = self.cache.changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    if let Some(lease) = self.cache.acquire(reader, &self.key, start..end)? {
                        break lease;
                    }
                    changed.await;
                };
                self.pending = Some(lease.entry.clone());
                loop {
                    let done = lease.entry.done.notified();
                    tokio::pin!(done);
                    done.as_mut().enable();
                    let result = {
                        let data = lease.entry.data.lock().unwrap();
                        data.result
                            .clone()
                            .map(|result| (data.range.clone(), result))
                    };
                    if let Some((range, result)) = result {
                        let data = result.map_err(|error| error.error())?;
                        let end = end.min(range.end);
                        output.extend_from_slice(
                            &data[(start - range.start) as usize..(end - range.start) as usize],
                        );
                        break;
                    }
                    done.await;
                }
                self.pending = None;
                drop(lease);
            }
            self.position += output.len() as u64;
            Ok(output)
        })
    }

    fn seek(&mut self, position: SeekFrom) -> IoFuture<'_, u64> {
        Box::pin(async move {
            self.position = match position {
                SeekFrom::Start(position) => Some(position),
                SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
                SeekFrom::End(delta) => self.info.size.checked_add_signed(delta),
            }
            .ok_or_else(|| error(ErrorKind::Invalid, "invalid seek position"))?;
            Ok(self.position)
        })
    }

    fn info(&self) -> IoFuture<'_, FileInfo> {
        Box::pin(async move { Ok(self.info.clone()) })
    }

    fn close(&mut self) -> IoFuture<'_, ()> {
        Box::pin(async move {
            if let Some(entry) = self.pending.take() {
                loop {
                    let done = entry.done.notified();
                    tokio::pin!(done);
                    done.as_mut().enable();
                    {
                        let data = entry.data.lock().unwrap();
                        if data.waiters != 0 || data.finished {
                            break;
                        }
                    }
                    done.await;
                }
            }
            if let Some(reader) = self.reader.take() {
                Reader::release(reader).await?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsspec_rs::async_files::{AsyncContext, FileProvider, Limits};
    use fsspec_rs::async_io::{Completion, Job, ManagedDispatcher};
    use std::sync::atomic::{AtomicBool, AtomicU64};

    #[derive(Default)]
    struct Store {
        ranges: Mutex<Vec<Range<u64>>>,
        blocked: AtomicBool,
        fail: AtomicBool,
        released: Notify,
        cancelled: AtomicUsize,
        closed: AtomicUsize,
    }
    struct File {
        store: Arc<Store>,
        position: AtomicU64,
        runtime: Handle,
        info: FileInfo,
    }
    impl FileProvider for File {
        fn seek(&self, position: SeekFrom, completion: Completion) {
            let SeekFrom::Start(position) = position else {
                unreachable!()
            };
            self.position.store(position, Ordering::Relaxed);
            completion.complete(Ok(Value::Position(position)));
        }
        fn info(&self, completion: Completion) {
            completion.complete(Ok(Value::Info(self.info.clone())));
        }
        fn close(&self, completion: Completion) {
            self.store.closed.fetch_add(1, Ordering::Relaxed);
            completion.complete(Ok(Value::Unit));
        }
        fn read(&self, size: usize, completion: Completion) {
            let start = self.position.load(Ordering::Relaxed);
            let range = start..start + size as u64;
            self.store.ranges.lock().unwrap().push(range.clone());
            let stop = CancellationToken::new();
            let cancelled = stop.clone();
            completion
                .on_cancel(
                    Arc::new(ManagedDispatcher),
                    Job::new(move || cancelled.cancel()),
                )
                .unwrap();
            let store = self.store.clone();
            self.runtime.spawn(async move {
                let wait = async {
                    loop {
                        let released = store.released.notified();
                        tokio::pin!(released);
                        released.as_mut().enable();
                        if !store.blocked.load(Ordering::Relaxed) {
                            break;
                        }
                        released.await;
                    }
                };
                let result =
                    match futures::future::select(Box::pin(wait), Box::pin(stop.cancelled())).await
                    {
                        futures::future::Either::Right(_) => {
                            store.cancelled.fetch_add(1, Ordering::Relaxed);
                            Err(error(ErrorKind::Cancelled, "cancelled test fetch"))
                        }
                        futures::future::Either::Left(_)
                            if store.fail.swap(false, Ordering::Relaxed) =>
                        {
                            Err(OperationError::Detailed {
                                kind: ErrorKind::Unavailable,
                                message: "transient test failure".into(),
                                retry_safe: true,
                            })
                        }
                        futures::future::Either::Left(_) => Ok(Value::Bytes(
                            range.map(|offset| (offset % 251) as u8).collect(),
                        )),
                    };
                completion.complete(result);
            });
        }
    }
    fn file(
        cache: &Arc<RangeCache>,
        context: &AsyncContext,
        store: &Arc<Store>,
        path: &str,
        etag: &str,
    ) -> CachedFile {
        let mut info = FileInfo::file(path, 4096);
        info.extra.insert("etag".into(), etag.into());
        let provider = Arc::new(File {
            store: store.clone(),
            position: AtomicU64::new(0),
            runtime: Handle::current(),
            info: info.clone(),
        });
        cache.file(
            AsyncFile::new(context, Arc::new(ManagedDispatcher), provider).unwrap(),
            info,
        )
    }
    async fn until(predicate: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !predicate() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }
    fn release(store: &Store) {
        store.blocked.store(false, Ordering::Relaxed);
        store.released.notify_waiters();
    }
    fn setup(limit: usize) -> (Arc<RangeCache>, AsyncContext, Arc<Store>) {
        (
            RangeCache::new(limit, Handle::current()),
            AsyncContext::new(Limits::default()).unwrap(),
            Arc::new(Store::default()),
        )
    }
    async fn read(file: &mut CachedFile, start: u64, size: usize) -> Vec<u8> {
        file.seek(SeekFrom::Start(start)).await.unwrap();
        file.read(size).await.unwrap()
    }

    #[tokio::test]
    async fn pending_overlap_coalesces_and_cached_bytes_outlive_first_file() {
        let (cache, context, store) = setup(64);
        let mut a = file(&cache, &context, &store, "key", "v1");
        let mut b = file(&cache, &context, &store, "key", "v1");
        let (left, right) = tokio::join!(read(&mut a, 0, 32), read(&mut b, 16, 32));
        assert_eq!(left, (0..32).collect::<Vec<u8>>());
        assert_eq!(right, (16..48).collect::<Vec<u8>>());
        assert_eq!(
            store.ranges.lock().unwrap().as_slice(),
            std::slice::from_ref(&(0..48))
        );
        a.close().await.unwrap();
        assert_eq!(store.closed.load(Ordering::Relaxed), 1);
        assert_eq!(read(&mut b, 0, 48).await, (0..48).collect::<Vec<u8>>());
        assert_eq!(store.ranges.lock().unwrap().len(), 1);
        b.close().await.unwrap();
        cache.close().await;
        assert_eq!(cache.state.lock().unwrap().bytes, 0);
    }

    #[tokio::test]
    async fn on_wire_overlap_fetches_only_missing_tail() {
        let (cache, context, store) = setup(64);
        store.blocked.store(true, Ordering::Relaxed);
        let mut a = file(&cache, &context, &store, "key", "v1");
        let mut b = file(&cache, &context, &store, "key", "v1");
        let first = tokio::spawn(async move {
            let data = read(&mut a, 0, 32).await;
            a.close().await.unwrap();
            data
        });
        until(|| store.ranges.lock().unwrap().len() == 1).await;
        let second = tokio::spawn(async move {
            let data = read(&mut b, 16, 32).await;
            b.close().await.unwrap();
            data
        });
        until(|| {
            cache.state.lock().unwrap().entries[0]
                .data
                .lock()
                .unwrap()
                .waiters
                == 2
        })
        .await;
        release(&store);
        assert_eq!(first.await.unwrap(), (0..32).collect::<Vec<u8>>());
        assert_eq!(second.await.unwrap(), (16..48).collect::<Vec<u8>>());
        assert_eq!(*store.ranges.lock().unwrap(), [0..32, 32..48]);
        cache.close().await;
    }

    #[tokio::test]
    async fn cancelling_first_reader_does_not_cancel_another_consumer() {
        let (cache, context, store) = setup(64);
        store.blocked.store(true, Ordering::Relaxed);
        let mut a = file(&cache, &context, &store, "key", "v1");
        let mut b = file(&cache, &context, &store, "key", "v1");
        let first = tokio::spawn(async move { read(&mut a, 0, 32).await });
        until(|| store.ranges.lock().unwrap().len() == 1).await;
        let second = tokio::spawn(async move {
            let data = read(&mut b, 0, 32).await;
            b.close().await.unwrap();
            data
        });
        until(|| {
            cache.state.lock().unwrap().entries[0]
                .data
                .lock()
                .unwrap()
                .waiters
                == 2
        })
        .await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert_eq!(store.cancelled.load(Ordering::Relaxed), 0);
        assert_eq!(store.closed.load(Ordering::Relaxed), 0);
        release(&store);
        assert_eq!(second.await.unwrap(), (0..32).collect::<Vec<u8>>());
        assert_eq!(store.ranges.lock().unwrap().len(), 1);
        until(|| store.closed.load(Ordering::Relaxed) == 2).await;
        cache.close().await;
    }

    #[tokio::test]
    async fn last_reader_cancellation_drains_and_releases_reservation() {
        let (cache, context, store) = setup(32);
        store.blocked.store(true, Ordering::Relaxed);
        let mut a = file(&cache, &context, &store, "key", "v1");
        assert!(tokio::time::timeout(Duration::from_millis(30), a.read(32))
            .await
            .is_err());
        a.close().await.unwrap();
        assert_eq!(store.cancelled.load(Ordering::Relaxed), 1);
        assert_eq!(store.closed.load(Ordering::Relaxed), 1);
        assert_eq!(cache.state.lock().unwrap().bytes, 0);
        cache.close().await;
    }

    #[tokio::test]
    async fn byte_budget_waits_and_large_logical_reads_split() {
        let (cache, context, store) = setup(4);
        store.blocked.store(true, Ordering::Relaxed);
        let mut a = file(&cache, &context, &store, "key", "v1");
        let mut b = file(&cache, &context, &store, "key", "v1");
        let first = tokio::spawn(async move {
            let data = read(&mut a, 0, 10).await;
            a.close().await.unwrap();
            data
        });
        until(|| store.ranges.lock().unwrap().len() == 1).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), read(&mut b, 32, 4))
                .await
                .is_err()
        );
        assert_eq!(store.ranges.lock().unwrap().len(), 1);
        assert_eq!(cache.state.lock().unwrap().bytes, 4);
        release(&store);
        assert_eq!(first.await.unwrap(), (0..10).collect::<Vec<u8>>());
        assert_eq!(*store.ranges.lock().unwrap(), [0..4, 4..8, 8..10]);
        assert!(cache.state.lock().unwrap().bytes <= 4);
        b.close().await.unwrap();
        cache.close().await;
    }

    #[tokio::test]
    async fn eviction_is_lru_and_failed_reads_are_not_cached() {
        let (cache, context, store) = setup(32);
        let mut a = file(&cache, &context, &store, "key", "v1");
        for start in [0, 32, 0, 64, 0, 32] {
            read(&mut a, start, 16).await;
        }
        assert_eq!(
            *store.ranges.lock().unwrap(),
            [0..16, 32..48, 64..80, 32..48]
        );
        store.fail.store(true, Ordering::Relaxed);
        a.seek(SeekFrom::Start(128)).await.unwrap();
        let error = a.read(16).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unavailable);
        assert!(error.retry_safe());
        assert_eq!(read(&mut a, 128, 16).await, (128..144).collect::<Vec<u8>>());
        assert_eq!(store.ranges.lock().unwrap().len(), 6);
        a.close().await.unwrap();
        cache.close().await;
    }

    #[tokio::test]
    async fn snapshots_paths_and_sources_are_isolated() {
        let (cache, context, store) = setup(64);
        for (path, etag) in [("a", "v1"), ("a", "v2"), ("b", "v1")] {
            let mut a = file(&cache, &context, &store, path, etag);
            read(&mut a, 0, 8).await;
            a.close().await.unwrap();
        }
        let other = RangeCache::new(64, Handle::current());
        let mut b = file(&other, &context, &store, "a", "v1");
        read(&mut b, 0, 8).await;
        assert_eq!(store.ranges.lock().unwrap().len(), 4);
        b.close().await.unwrap();
        other.close().await;
        cache.close().await;
    }

    #[tokio::test]
    async fn shutdown_cancels_pending_and_rejects_later_reads() {
        let (cache, context, store) = setup(32);
        store.blocked.store(true, Ordering::Relaxed);
        let mut a = file(&cache, &context, &store, "key", "v1");
        let task = tokio::spawn(async move {
            let result = a.read(32).await;
            a.close().await.unwrap();
            result
        });
        until(|| store.ranges.lock().unwrap().len() == 1).await;
        cache.close().await;
        assert_eq!(
            task.await.unwrap().unwrap_err().kind(),
            ErrorKind::Cancelled
        );
        assert_eq!(store.cancelled.load(Ordering::Relaxed), 1);
        let mut b = file(&cache, &context, &store, "key", "v1");
        assert_eq!(b.read(1).await.unwrap_err().kind(), ErrorKind::Cancelled);
        b.close().await.unwrap();
        assert_eq!(cache.state.lock().unwrap().bytes, 0);
    }
    #[tokio::test]
    async fn eviction_releases_buffers_even_when_cleanup_retains_an_entry() {
        let (cache, context, store) = setup(16);
        let mut a = file(&cache, &context, &store, "key", "v1");
        read(&mut a, 0, 16).await;
        let retired = cache.state.lock().unwrap().entries[0].clone();
        read(&mut a, 32, 16).await;
        assert!(retired.data.lock().unwrap().result.is_none());
        assert_eq!(cache.state.lock().unwrap().bytes, 16);
        let current = cache.state.lock().unwrap().entries[0].clone();
        a.close().await.unwrap();
        cache.close().await;
        assert!(current.data.lock().unwrap().result.is_none());
        assert_eq!(cache.state.lock().unwrap().bytes, 0);
    }
}
