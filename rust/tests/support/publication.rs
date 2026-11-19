//! Executable, single-partition publication model, not a storage implementation.
//!
//! Integer rows and exact structural equality stand in for Arrow batches, serialization,
//! and collision-resistant fingerprints. Attempts have unique identities supplied by the
//! store. Submission, server application, and client resolution are separate actions.
//! No threads, clocks, network, or runtime scheduling are needed to reproduce a trace.
//! Real encoders, byte accounting, cross-language hashing, and store durability remain
//! separate integration work. A model success does not certify a storage backend.

use std::collections::{BTreeMap, BTreeSet};

use fsspec_rs::ErrorKind;

pub type Result<T> = std::result::Result<T, ErrorKind>;
pub type Attempt = (u64, u64); // Target generation and unique attempt ID.

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PartId(pub Attempt, pub usize);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub key: u64,
    pub position: usize,
    pub rows: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub attempt: Attempt,
    pub parent: Option<Attempt>,
    pub instance: u64,
    pub parts: Vec<(PartId, Vec<u64>)>,
    pub receipts: Vec<Receipt>,
    pub complete: bool,
    pub deleted: bool,
}

impl Manifest {
    pub fn progress(&self) -> usize {
        self.parts.iter().map(|(_, rows)| rows.len()).sum()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Store {
    pub manifests: BTreeMap<u64, Manifest>,
    pub parts: BTreeMap<PartId, (Vec<u64>, u64)>,
    pub candidates: BTreeMap<Attempt, (Manifest, u64)>,
    pub markers: BTreeSet<u64>,
    pub read_errors: BTreeMap<u64, ErrorKind>,
    pub now: u64,
    next_id: u64,
    requests: Vec<Manifest>,
}

impl Store {
    pub fn allocate(&mut self, generation: u64) -> Attempt {
        self.next_id += 1;
        (generation, self.next_id)
    }

    pub fn read(&self, generation: u64) -> Result<Option<Manifest>> {
        if let Some(error) = self.read_errors.get(&generation) {
            return Err(*error);
        }
        let Some(manifest) = self.manifests.get(&generation) else {
            return Ok(None);
        };
        if manifest.attempt.0 != generation {
            return Err(ErrorKind::Corrupt);
        }
        match (generation, manifest.parent) {
            (0, None) => {}
            (g, Some(parent)) if g > 0 && parent.0 == g - 1 => {
                if self.read_errors.contains_key(&(g - 1)) {
                    return Err(self.read_errors[&(g - 1)]);
                }
                if self.manifests.get(&(g - 1)).map(|m| m.attempt) != Some(parent) {
                    return Err(ErrorKind::Corrupt);
                }
            }
            _ => return Err(ErrorKind::Corrupt),
        }
        Ok(Some(manifest.clone()))
    }

    pub fn latest(&self) -> Result<Option<Manifest>> {
        latest(|g| self.read(g), 128)
    }

    pub fn snapshot(&self, generation: u64) -> Result<Vec<u64>> {
        let manifest = self.read(generation)?.ok_or(ErrorKind::NotFound)?;
        if manifest.deleted {
            return Err(ErrorKind::NotFound);
        }
        let mut rows = Vec::new();
        let mut seen = BTreeSet::new();
        for (id, expected) in &manifest.parts {
            if !seen.insert(id) || self.parts.get(id).map(|p| &p.0) != Some(expected) {
                return Err(ErrorKind::Corrupt);
            }
            rows.extend(expected);
        }
        Ok(rows)
    }

    // The simulated server does only atomic create-if-absent. Protocol validation
    // belongs to the writer; the server must not repair an invalid client request.
    pub fn deliver(&mut self, request: usize) -> Result<()> {
        let candidate = self.requests[request].clone();
        match self.manifests.entry(candidate.attempt.0) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(candidate);
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(_) => Err(ErrorKind::Conflict),
        }
    }

    pub fn submit(&mut self, attempt: Attempt) -> Result<usize> {
        let candidate = self
            .candidates
            .get(&attempt)
            .ok_or(ErrorKind::Corrupt)?
            .0
            .clone();
        self.requests.push(candidate);
        Ok(self.requests.len() - 1)
    }

    pub fn recover(&self, attempt: Attempt) -> Result<Manifest> {
        let actual = self.read(attempt.0)?;
        // A losing attempt's descriptor may already have been collected. The
        // immutable winner still proves that this attempt cannot publish.
        if actual.as_ref().is_some_and(|m| m.attempt != attempt) {
            return Err(ErrorKind::Conflict);
        }
        let expected = &self.candidates.get(&attempt).ok_or(ErrorKind::Corrupt)?.0;
        match actual {
            Some(actual) if &actual == expected => Ok(actual),
            Some(_) => Err(ErrorKind::Conflict),
            None => Err(ErrorKind::UnknownOutcome),
        }
    }

    pub fn marker(&mut self, generation: u64) -> Result<()> {
        if !self.read(generation)?.ok_or(ErrorKind::NotFound)?.complete {
            return Err(ErrorKind::Invalid);
        }
        self.markers.insert(generation);
        Ok(())
    }

    pub fn cleanup_candidate(&mut self) -> Result<Attempt> {
        let mut candidate = self.latest()?.ok_or(ErrorKind::NotFound)?;
        let parent = candidate.attempt;
        candidate.attempt = self.allocate(parent.0.checked_add(1).ok_or(ErrorKind::Invalid)?);
        candidate.parent = Some(parent);
        candidate.receipts.clear();
        let attempt = candidate.attempt;
        self.candidates.insert(attempt, (candidate, self.now));
        Ok(attempt)
    }

    // Listing may omit candidates. Reads must establish the full reference set before
    // any deletion; this intentionally does not infer liveness from the latest table.
    pub fn sweep(&mut self, boundary: u64, retention: u64, listed: &[PartId]) -> Result<()> {
        let mut referenced = BTreeSet::new();
        let mut winners = BTreeSet::new();
        for g in 0..=boundary {
            let manifest = self.read(g)?.ok_or(ErrorKind::Corrupt)?;
            if !manifest.deleted {
                self.snapshot(g)?;
            }
            referenced.extend(manifest.parts.iter().map(|p| p.0.clone()));
            winners.insert(manifest.attempt);
        }
        for id in listed {
            if id.0 .0 <= boundary && !referenced.contains(id) {
                if let Some((_, created)) = self.parts.get(id) {
                    if self.now.saturating_sub(*created) >= retention {
                        self.parts.remove(id);
                    }
                }
            }
        }
        self.candidates.retain(|attempt, (_, created)| {
            attempt.0 > boundary
                || winners.contains(attempt)
                || self.now.saturating_sub(*created) < retention
        });
        Ok(())
    }
}

/// Uses key reads only. The read callback can inject publication between probes.
pub fn latest(
    mut read: impl FnMut(u64) -> Result<Option<Manifest>>,
    budget: usize,
) -> Result<Option<Manifest>> {
    let mut remaining = budget;
    let mut probe = |g| {
        remaining = remaining.checked_sub(1).ok_or(ErrorKind::Timeout)?;
        read(g)
    };
    let Some(mut low) = probe(0)? else {
        return Ok(None);
    };
    loop {
        let mut distance = 1;
        let mut high;
        loop {
            high = low
                .attempt
                .0
                .checked_add(distance)
                .ok_or(ErrorKind::Invalid)?;
            match probe(high)? {
                Some(found) => {
                    low = found;
                    distance = distance.checked_mul(2).ok_or(ErrorKind::Invalid)?;
                }
                None => break,
            }
        }
        while high - low.attempt.0 > 1 {
            let mid = low.attempt.0 + (high - low.attempt.0) / 2;
            match probe(mid)? {
                Some(found) => low = found,
                None => high = mid,
            }
        }
        // A previously missing upper bound may have appeared during discovery.
        match probe(high)? {
            Some(found) => low = found,
            None => return Ok(Some(low)),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Mode {
    Create,
    Append,
    Replace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Open,
    Prepared,
    Unknown,
    Failed,
    Closed,
}

#[derive(Clone, Debug)]
pub struct Writer {
    pub attempt: Attempt,
    pub state: State,
    pub instance: u64,
    base: Option<Manifest>,
    parts: Vec<(PartId, Vec<u64>)>,
    receipts: Vec<Receipt>,
    inherited: Vec<Receipt>,
    new_instance: bool,
    prepared: Option<Manifest>,
}

impl Writer {
    pub fn open(store: &mut Store, mode: Mode) -> Result<Self> {
        let base = store.latest()?;
        match (mode, &base) {
            (Mode::Create, Some(m)) if !m.deleted => return Err(ErrorKind::AlreadyExists),
            (Mode::Append | Mode::Replace, None) => return Err(ErrorKind::NotFound),
            (Mode::Append | Mode::Replace, Some(m)) if m.deleted => {
                return Err(ErrorKind::NotFound)
            }
            (Mode::Append, Some(m)) if m.complete => return Err(ErrorKind::Conflict),
            _ => {}
        }
        let generation = match &base {
            Some(m) => m.attempt.0.checked_add(1).ok_or(ErrorKind::Invalid)?,
            None => 0,
        };
        let attempt = store.allocate(generation);
        let new_instance = !matches!(mode, Mode::Append);
        let instance = if new_instance {
            attempt.1
        } else {
            base.as_ref().unwrap().instance
        };
        let parts = if new_instance {
            Vec::new()
        } else {
            base.as_ref().unwrap().parts.clone()
        };
        let mut inherited = Vec::new();
        if !new_instance {
            for g in (0..generation).rev() {
                let m = store.read(g)?.ok_or(ErrorKind::Corrupt)?;
                if m.instance != instance {
                    break;
                }
                inherited.extend(m.receipts);
            }
        }
        Ok(Self {
            attempt,
            state: State::Open,
            instance,
            base,
            parts,
            receipts: Vec::new(),
            inherited,
            new_instance,
            prepared: None,
        })
    }

    pub fn write(&mut self, key: u64, position: usize, rows: &[u64]) -> Result<Receipt> {
        if self.state != State::Open {
            return Err(ErrorKind::Invalid);
        }
        let receipt = Receipt {
            key,
            position,
            rows: rows.to_vec(),
        };
        if let Some(previous) = self
            .receipts
            .iter()
            .chain(&self.inherited)
            .find(|r| r.key == key)
        {
            return if previous == &receipt {
                Ok(previous.clone())
            } else {
                Err(ErrorKind::Conflict)
            };
        }
        let progress: usize = self.parts.iter().map(|p| p.1.len()).sum();
        if position != progress {
            return Err(if position > progress {
                ErrorKind::Gap
            } else {
                ErrorKind::Conflict
            });
        }
        if rows.is_empty() {
            return Err(ErrorKind::Invalid);
        }
        self.parts
            .push((PartId(self.attempt, self.receipts.len()), rows.to_vec()));
        self.receipts.push(receipt.clone());
        Ok(receipt)
    }

    pub fn flush(&self, store: &mut Store) -> Result<()> {
        if self.state != State::Open {
            return Err(ErrorKind::Invalid);
        }
        for (id, rows) in self.parts.iter().filter(|p| p.0 .0 == self.attempt) {
            store
                .parts
                .entry(id.clone())
                .or_insert((rows.clone(), store.now));
        }
        Ok(())
    }

    pub fn prepare(&mut self, store: &mut Store, finish: bool) -> Result<Attempt> {
        if self.state != State::Open {
            return Err(ErrorKind::Invalid);
        }
        if !finish && !self.new_instance && self.receipts.is_empty() {
            return Ok(self.base.as_ref().unwrap().attempt);
        }
        self.flush(store)?;
        let candidate = Manifest {
            attempt: self.attempt,
            parent: self.base.as_ref().map(|m| m.attempt),
            instance: self.instance,
            parts: self.parts.clone(),
            receipts: self.receipts.clone(),
            complete: finish,
            deleted: false,
        };
        store
            .candidates
            .insert(self.attempt, (candidate.clone(), store.now));
        self.prepared = Some(candidate);
        self.state = State::Prepared;
        Ok(self.attempt)
    }

    pub fn submit(&mut self, store: &mut Store) -> Result<usize> {
        if !matches!(self.state, State::Prepared | State::Unknown) {
            return Err(ErrorKind::Invalid);
        }
        // The client retains the exact request even if cleanup removes a losing
        // descriptor before submission. A closed slot still rejects the request.
        store
            .requests
            .push(self.prepared.clone().ok_or(ErrorKind::Invalid)?);
        let request = store.requests.len() - 1;
        self.state = State::Unknown;
        Ok(request)
    }

    pub fn resolve(&mut self, store: &mut Store) -> Result<Manifest> {
        if self.state != State::Unknown {
            return Err(ErrorKind::Invalid);
        }
        match store.recover(self.attempt) {
            Ok(manifest) => {
                self.inherited.extend(std::mem::take(&mut self.receipts));
                self.base = Some(manifest.clone());
                self.prepared = None;
                self.new_instance = false;
                self.state = if manifest.complete {
                    State::Closed
                } else {
                    State::Open
                };
                self.attempt = store.allocate(
                    manifest
                        .attempt
                        .0
                        .checked_add(1)
                        .ok_or(ErrorKind::Invalid)?,
                );
                Ok(manifest)
            }
            Err(error) => {
                if error == ErrorKind::Conflict {
                    self.state = State::Failed;
                }
                Err(error)
            }
        }
    }

    pub fn abort(&mut self) -> Result<()> {
        if self.state == State::Unknown {
            return Err(ErrorKind::UnknownOutcome);
        }
        self.state = State::Closed;
        self.receipts.clear();
        self.parts.clear();
        self.prepared = None;
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum End {
    Waiting,
    Complete,
    Replaced,
}

pub fn follow(store: &Store, generation: u64, position: usize) -> Result<(Vec<u64>, End)> {
    let initial = store.read(generation)?.ok_or(ErrorKind::Invalid)?;
    if initial.deleted || position > initial.progress() {
        return Err(ErrorKind::Invalid);
    }
    let mut cursor = position;
    let mut output = Vec::new();
    let mut g = generation;
    while let Some(m) = store.read(g)? {
        if m.instance != initial.instance || m.deleted {
            return Ok((output, End::Replaced));
        }
        let rows = store.snapshot(g)?;
        if cursor > rows.len() {
            return Err(ErrorKind::Corrupt);
        }
        output.extend_from_slice(&rows[cursor..]);
        cursor = rows.len();
        if m.complete {
            return Ok((output, End::Complete));
        }
        g = g.checked_add(1).ok_or(ErrorKind::Invalid)?;
    }
    Ok((output, End::Waiting))
}
