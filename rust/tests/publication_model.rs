//! Protocol experiments run by the normal Rust test suite. These are deliberately
//! independent of filesystem adapters and do not expose a supported writer API.

#[path = "support/publication.rs"]
mod publication;

use fsspec_rs::ErrorKind;
use publication::*;

fn publish(store: &mut Store, writer: &mut Writer, finish: bool) -> Manifest {
    let attempt = writer.prepare(store, finish).unwrap();
    if writer.state == State::Open {
        return store.read(attempt.0).unwrap().unwrap();
    }
    let request = writer.submit(store).unwrap();
    store.deliver(request).unwrap();
    writer.resolve(store).unwrap()
}

fn initial() -> Store {
    let mut store = Store::default();
    let mut writer = Writer::open(&mut store, Mode::Create).unwrap();
    writer.write(1, 0, &[10]).unwrap();
    publish(&mut store, &mut writer, false);
    store
}

fn sweep(store: &mut Store, generation: u64, retention: u64) {
    let listed = store.parts.keys().cloned().collect::<Vec<_>>();
    store.sweep(generation, retention, &listed).unwrap();
}

#[test]
fn competing_creators_never_mix_parts() {
    for first in 0..2 {
        let mut store = Store::default();
        let mut writers = [
            Writer::open(&mut store, Mode::Create).unwrap(),
            Writer::open(&mut store, Mode::Create).unwrap(),
        ];
        let mut requests = Vec::new();
        for (i, writer) in writers.iter_mut().enumerate() {
            writer.write(7, 0, &[i as u64]).unwrap();
            writer.prepare(&mut store, false).unwrap();
            requests.push(writer.submit(&mut store).unwrap());
        }
        store.deliver(requests[first]).unwrap();
        assert_eq!(store.deliver(requests[1 - first]), Err(ErrorKind::Conflict));
        assert_eq!(
            writers[1 - first].resolve(&mut store),
            Err(ErrorKind::Conflict)
        );
        writers[first].resolve(&mut store).unwrap();
        assert_eq!(store.snapshot(0).unwrap(), [first as u64]);
        assert_eq!(store.manifests.len(), 1);
    }
}

#[test]
fn batch_keys_deduplicate_pending_and_published_work() {
    let mut store = initial();
    let mut writer = Writer::open(&mut store, Mode::Append).unwrap();
    let receipt = writer.write(2, 1, &[20, 30]).unwrap();
    assert_eq!(writer.write(2, 1, &[20, 30]).unwrap(), receipt);
    assert_eq!(writer.write(2, 1, &[99]), Err(ErrorKind::Conflict));
    assert_eq!(writer.write(2, 3, &[20, 30]), Err(ErrorKind::Conflict));
    assert_eq!(writer.write(3, 4, &[40]), Err(ErrorKind::Gap));
    assert_eq!(writer.write(3, 2, &[40]), Err(ErrorKind::Conflict));
    assert_eq!(writer.write(3, 3, &[]), Err(ErrorKind::Invalid));
    let committed = publish(&mut store, &mut writer, false);
    drop(writer);
    let mut restarted = Writer::open(&mut store, Mode::Append).unwrap();
    assert_eq!(restarted.write(2, 1, &[20, 30]).unwrap(), receipt);
    assert_eq!(publish(&mut store, &mut restarted, false), committed);
    assert_eq!(store.snapshot(1).unwrap(), [10, 20, 30]);
    assert_eq!(store.manifests.len(), 2);
}

#[test]
fn commit_finish_abort_and_drop_have_distinct_effects() {
    let mut store = Store::default();
    let mut writer = Writer::open(&mut store, Mode::Create).unwrap();
    publish(&mut store, &mut writer, false); // Empty Create still publishes.
    writer.write(1, 0, &[10]).unwrap();
    writer.flush(&mut store).unwrap();
    assert!(store.snapshot(0).unwrap().is_empty());
    publish(&mut store, &mut writer, false);
    assert_eq!(writer.state, State::Open);
    writer.write(2, 1, &[20]).unwrap();
    writer.flush(&mut store).unwrap();
    writer.abort().unwrap();
    assert_eq!(store.snapshot(1).unwrap(), [10]);
    assert_eq!(writer.write(3, 1, &[30]), Err(ErrorKind::Invalid));

    let mut writer = Writer::open(&mut store, Mode::Append).unwrap();
    writer.write(4, 1, &[40]).unwrap();
    writer.prepare(&mut store, false).unwrap();
    drop(writer); // A durable candidate is not a published table.
    assert_eq!(store.manifests.len(), 2);

    let mut writer = Writer::open(&mut store, Mode::Append).unwrap();
    let finished = publish(&mut store, &mut writer, true);
    assert_eq!(writer.state, State::Closed);
    assert!(finished.complete);
    assert_eq!(store.recover(finished.attempt).unwrap(), finished);
    assert_eq!(
        Writer::open(&mut store, Mode::Append).unwrap_err(),
        ErrorKind::Conflict
    );
    assert!(!store.markers.contains(&2)); // Crash before the compatibility marker.
    store.marker(2).unwrap();
    store.marker(2).unwrap();
    assert_eq!(store.marker(1), Err(ErrorKind::Invalid));
}

#[test]
fn absent_after_timeout_is_not_an_abort_or_permission_to_rebase() {
    let mut store = initial();
    let mut writer = Writer::open(&mut store, Mode::Append).unwrap();
    writer.write(2, 1, &[20]).unwrap();
    let attempt = writer.prepare(&mut store, false).unwrap();
    let delayed = writer.submit(&mut store).unwrap();
    assert_eq!(writer.resolve(&mut store), Err(ErrorKind::UnknownOutcome));
    assert_eq!(writer.abort(), Err(ErrorKind::UnknownOutcome));
    assert_eq!(writer.write(3, 2, &[30]), Err(ErrorKind::Invalid));
    assert_eq!(writer.prepare(&mut store, false), Err(ErrorKind::Invalid));
    let retry = writer.submit(&mut store).unwrap();
    store.deliver(retry).unwrap();
    assert_eq!(store.deliver(delayed), Err(ErrorKind::Conflict));
    drop(writer); // Response lost; recover without the process's writer state.
    let recovered = store.recover(attempt).unwrap();
    assert_eq!(recovered.progress(), 2);
    assert_eq!(store.snapshot(1).unwrap(), [10, 20]);
    assert_eq!(store.manifests.len(), 2);
}

#[test]
fn delayed_original_request_can_win_after_client_crash() {
    let mut store = initial();
    let mut writer = Writer::open(&mut store, Mode::Append).unwrap();
    writer.write(2, 1, &[20]).unwrap();
    let attempt = writer.prepare(&mut store, false).unwrap();
    let delayed = writer.submit(&mut store).unwrap();
    drop(writer);
    assert_eq!(store.recover(attempt), Err(ErrorKind::UnknownOutcome));
    store.deliver(delayed).unwrap();
    assert_eq!(store.recover(attempt).unwrap().progress(), 2);
}

#[test]
fn crash_before_publication_replays_only_uncommitted_batches() {
    let mut store = initial();
    let mut writer = Writer::open(&mut store, Mode::Append).unwrap();
    writer.write(2, 1, &[20]).unwrap();
    writer.flush(&mut store).unwrap();
    writer.write(3, 2, &[30]).unwrap(); // Crash with only some parts uploaded.
    drop(writer);
    assert_eq!(store.snapshot(0).unwrap(), [10]);
    let mut restarted = Writer::open(&mut store, Mode::Append).unwrap();
    restarted.write(1, 0, &[10]).unwrap(); // Already committed.
    restarted.write(2, 1, &[20]).unwrap();
    restarted.write(3, 2, &[30]).unwrap();
    publish(&mut store, &mut restarted, false);
    store.now = 10;
    sweep(&mut store, 1, 5);
    assert_eq!(store.snapshot(1).unwrap(), [10, 20, 30]);
    assert_eq!(store.parts.len(), 3);
}

#[test]
fn replacement_conflicts_with_a_concurrent_append() {
    let mut store = initial();
    let mut replacement = Writer::open(&mut store, Mode::Replace).unwrap();
    replacement.write(2, 0, &[99]).unwrap();
    replacement.prepare(&mut store, false).unwrap();
    let late = replacement.submit(&mut store).unwrap();
    let mut append = Writer::open(&mut store, Mode::Append).unwrap();
    append.write(2, 1, &[20]).unwrap();
    publish(&mut store, &mut append, false);
    assert_eq!(store.deliver(late), Err(ErrorKind::Conflict));
    assert_eq!(replacement.resolve(&mut store), Err(ErrorKind::Conflict));
    assert_eq!(
        replacement.prepare(&mut store, false),
        Err(ErrorKind::Invalid)
    );
    assert_eq!(store.snapshot(1).unwrap(), [10, 20]);
    assert_eq!(store.manifests.len(), 2);
}

#[test]
fn deletion_and_recreation_preserve_history_without_resetting_generations() {
    let mut store = initial();
    // Deletion is a publication type rather than a write_batch operation.
    let original = store.latest().unwrap().unwrap();
    let attempt = store.allocate(1);
    let tombstone = Manifest {
        attempt,
        parent: Some(original.attempt),
        instance: attempt.1,
        parts: Vec::new(),
        receipts: Vec::new(),
        complete: false,
        deleted: true,
    };
    store.candidates.insert(attempt, (tombstone, store.now));
    let request = store.submit(attempt).unwrap();
    store.deliver(request).unwrap();
    assert_eq!(store.snapshot(1), Err(ErrorKind::NotFound));
    assert_eq!(follow(&store, 0, 0).unwrap(), (vec![10], End::Replaced));
    assert_eq!(
        Writer::open(&mut store, Mode::Append).unwrap_err(),
        ErrorKind::NotFound
    );
    let mut recreated = Writer::open(&mut store, Mode::Create).unwrap();
    recreated.write(1, 0, &[99]).unwrap();
    assert_eq!(publish(&mut store, &mut recreated, false).attempt.0, 2);
    store.now = 10;
    sweep(&mut store, 2, 5);
    assert_eq!(store.snapshot(0).unwrap(), [10]);
    assert_eq!(store.snapshot(2).unwrap(), [99]);
}

#[test]
fn snapshot_and_follow_do_not_switch_to_a_replacement() {
    let mut store = initial();
    let mut writer = Writer::open(&mut store, Mode::Append).unwrap();
    writer.write(2, 1, &[20]).unwrap();
    publish(&mut store, &mut writer, false);
    assert_eq!(follow(&store, 0, 1).unwrap(), (vec![20], End::Waiting));
    assert_eq!(follow(&store, 0, 2), Err(ErrorKind::Invalid));
    let old_instance = writer.instance;
    let mut replacement = Writer::open(&mut store, Mode::Replace).unwrap();
    replacement.write(1, 0, &[99]).unwrap(); // Same key belongs to a new instance.
    publish(&mut store, &mut replacement, true);
    assert_ne!(old_instance, replacement.instance);
    assert_eq!(store.snapshot(0).unwrap(), [10]);
    assert_eq!(store.snapshot(1).unwrap(), [10, 20]);
    assert_eq!(store.snapshot(2).unwrap(), [99]);
    assert_eq!(follow(&store, 0, 1).unwrap(), (vec![20], End::Replaced));
    assert_eq!(follow(&store, 2, 0).unwrap(), (vec![99], End::Complete));
    store.now = 100;
    sweep(&mut store, 2, 5);
    assert_eq!(store.snapshot(0).unwrap(), [10]);
}

#[test]
fn follow_observes_completion_before_later_replacement() {
    let mut store = initial();
    let mut writer = Writer::open(&mut store, Mode::Append).unwrap();
    publish(&mut store, &mut writer, true);
    let mut replacement = Writer::open(&mut store, Mode::Replace).unwrap();
    replacement.write(2, 0, &[99]).unwrap();
    publish(&mut store, &mut replacement, false);
    store.marker(1).unwrap(); // Delayed old marker must not complete generation two.
    assert!(!store.markers.contains(&2));
    assert_eq!(follow(&store, 0, 0).unwrap(), (vec![10], End::Complete));
}

#[test]
fn discovery_rechecks_a_missing_successor_after_concurrent_publication() {
    let mut store = initial();
    let mut writer = Writer::open(&mut store, Mode::Append).unwrap();
    writer.write(2, 1, &[20]).unwrap();
    writer.prepare(&mut store, false).unwrap();
    let request = writer.submit(&mut store).unwrap();
    let mut injected = false;
    let found = latest(
        |g| {
            let observed = store.read(g)?;
            if g == 1 && !injected {
                store.deliver(request).unwrap();
                injected = true;
            }
            Ok(observed)
        },
        20,
    )
    .unwrap()
    .unwrap();
    assert_eq!(found.attempt.0, 1);
}

#[test]
fn discovery_is_bounded_and_does_not_mask_read_errors() {
    let mut store = initial();
    for _ in 0..64 {
        let attempt = store.cleanup_candidate().unwrap();
        let request = store.submit(attempt).unwrap();
        store.deliver(request).unwrap();
    }
    let mut probes = 0;
    let found = latest(
        |g| {
            probes += 1;
            store.read(g)
        },
        20,
    )
    .unwrap()
    .unwrap();
    assert_eq!(found.attempt.0, 64);
    assert!(probes <= 20);
    assert_eq!(latest(|g| store.read(g), 2), Err(ErrorKind::Timeout));
    for error in [
        ErrorKind::PermissionDenied,
        ErrorKind::Unavailable,
        ErrorKind::Timeout,
    ] {
        store.read_errors.insert(1, error);
        assert_eq!(store.latest(), Err(error));
    }
    store.read_errors.clear();
    store.manifests.remove(&63);
    assert_eq!(store.read(64), Err(ErrorKind::Corrupt));
}

#[test]
fn continuous_publication_exhausts_discovery_budget() {
    let mut store = initial();
    assert_eq!(
        latest(
            |g| {
                while !store.manifests.contains_key(&g) {
                    let candidate = store.cleanup_candidate()?;
                    let request = store.submit(candidate)?;
                    store.deliver(request)?;
                }
                store.read(g)
            },
            8
        ),
        Err(ErrorKind::Timeout)
    );
}

#[test]
fn cleanup_retains_open_attempts_history_and_successful_recovery_records() {
    let mut store = initial();
    let mut loser = Writer::open(&mut store, Mode::Append).unwrap();
    loser.write(2, 1, &[20]).unwrap();
    let attempt = loser.prepare(&mut store, false).unwrap();
    let late = loser.submit(&mut store).unwrap();
    store.now = 10;
    sweep(&mut store, 0, 1); // Old but still eligible for publication.
    assert!(store.candidates.contains_key(&attempt));
    let boundary = store.cleanup_candidate().unwrap();
    let request = store.submit(boundary).unwrap();
    store.deliver(request).unwrap();
    store.now = 20;
    store.sweep(1, 1, &[]).unwrap(); // Incomplete listing only leaves garbage.
    let orphan = PartId(attempt, 0);
    assert!(store.parts.contains_key(&orphan));
    sweep(&mut store, 1, 1);
    assert!(!store.parts.contains_key(&orphan));
    assert!(store.candidates.contains_key(&boundary));
    assert_eq!(store.deliver(late), Err(ErrorKind::Conflict));
    assert_eq!(loser.resolve(&mut store), Err(ErrorKind::Conflict));
    assert_eq!(store.snapshot(0).unwrap(), [10]);
    sweep(&mut store, 1, 1); // Repeating an interrupted sweep is safe.
}

#[test]
fn cleanup_checks_retention_and_stops_before_deleting_on_read_failure() {
    let mut store = initial();
    let mut loser = Writer::open(&mut store, Mode::Append).unwrap();
    loser.write(2, 1, &[20]).unwrap();
    loser.flush(&mut store).unwrap();
    let orphan = PartId(loser.attempt, 0);
    let boundary = store.cleanup_candidate().unwrap();
    let request = store.submit(boundary).unwrap();
    store.deliver(request).unwrap();
    store.now = 4;
    sweep(&mut store, 1, 5);
    assert!(store.parts.contains_key(&orphan));
    store.now = 5;
    store.read_errors.insert(1, ErrorKind::PermissionDenied);
    assert_eq!(
        store.sweep(1, 5, std::slice::from_ref(&orphan)),
        Err(ErrorKind::PermissionDenied)
    );
    assert!(store.parts.contains_key(&orphan));
    store.read_errors.clear();
    sweep(&mut store, 1, 5);
    assert!(!store.parts.contains_key(&orphan));
}

#[test]
fn corrupted_parts_and_recovery_descriptors_fail_explicitly() {
    let mut store = initial();
    let manifest = store.read(0).unwrap().unwrap();
    let part = manifest.parts[0].0.clone();
    store.parts.get_mut(&part).unwrap().0[0] = 99;
    assert_eq!(store.snapshot(0), Err(ErrorKind::Corrupt));
    store.parts.remove(&part);
    assert_eq!(store.snapshot(0), Err(ErrorKind::Corrupt));
    store
        .candidates
        .get_mut(&manifest.attempt)
        .unwrap()
        .0
        .complete = true;
    assert_eq!(store.recover(manifest.attempt), Err(ErrorKind::Conflict));
}

// Enumerate every interleaving of two appenders (stage, submit, deliver, resolve)
// and a cleaner (submit, deliver, sweep). Uploads can happen after their slot closes.
#[derive(Clone)]
struct Race {
    store: Store,
    writers: [Writer; 2],
    cleaner: Attempt,
    steps: [usize; 3],
    requests: [Option<usize>; 3],
    trace: Vec<usize>,
}

fn explore(race: Race) -> usize {
    if race.steps == [4, 4, 3] {
        let mut store = race.store;
        store.now += 10;
        sweep(&mut store, 1, 2);
        assert_eq!(store.snapshot(0).unwrap(), [10], "{:?}", race.trace);
        let rows = store.snapshot(1).unwrap();
        assert!(
            rows == [10] || rows == [10, 20] || rows == [10, 30],
            "{:?}",
            race.trace
        );
        assert_eq!(store.manifests.len(), 2);
        assert_eq!(store.parts.len(), rows.len()); // One row per part in this model.
        for m in store.manifests.values() {
            store.recover(m.attempt).unwrap();
        }
        return 1;
    }
    let mut count = 0;
    for actor in 0..3 {
        if race.steps[actor] == [4, 4, 3][actor] {
            continue;
        }
        let mut next = race.clone();
        next.store.now += 1;
        next.trace.push(actor);
        if actor < 2 {
            let writer = &mut next.writers[actor];
            match next.steps[actor] {
                0 => {
                    writer.prepare(&mut next.store, false).unwrap();
                }
                1 => {
                    next.requests[actor] = Some(writer.submit(&mut next.store).unwrap());
                }
                2 => {
                    let _ = next.store.deliver(next.requests[actor].unwrap());
                }
                3 => {
                    let result = writer.resolve(&mut next.store);
                    assert!(
                        result.is_ok() || result == Err(ErrorKind::Conflict),
                        "{:?}: {result:?}",
                        next.trace
                    );
                }
                _ => unreachable!(),
            }
        } else {
            match next.steps[actor] {
                0 => {
                    next.requests[actor] = Some(next.store.submit(next.cleaner).unwrap());
                }
                1 => {
                    let _ = next.store.deliver(next.requests[actor].unwrap());
                }
                2 => {
                    sweep(&mut next.store, 1, 2);
                }
                _ => unreachable!(),
            }
        }
        next.steps[actor] += 1;
        // Historical readers remain valid at every intermediate step, not just at the end.
        assert_eq!(next.store.snapshot(0).unwrap(), [10], "{:?}", next.trace);
        if next.store.manifests.contains_key(&1) {
            next.store.snapshot(1).unwrap();
        }
        count += explore(next);
    }
    count
}

#[test]
fn all_appender_and_cleanup_interleavings_preserve_published_data() {
    let mut store = initial();
    let mut a = Writer::open(&mut store, Mode::Append).unwrap();
    let mut b = Writer::open(&mut store, Mode::Append).unwrap();
    a.write(2, 1, &[20]).unwrap();
    b.write(3, 1, &[30]).unwrap();
    let cleaner = store.cleanup_candidate().unwrap();
    let count = explore(Race {
        store,
        writers: [a, b],
        cleaner,
        steps: [0; 3],
        requests: [None; 3],
        trace: Vec::new(),
    });
    assert_eq!(count, 11_550);
}
