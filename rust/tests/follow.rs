use std::sync::Arc;
use std::time::Duration;

use arrow::array::{Int64Array, RecordBatch};
use fsspec_data::follow::{FollowEnd, FollowOptions, FollowPosition, ManifestFollower};
use fsspec_data::manifest::{ManifestReader, ReadLimits};
use fsspec_data::sink::{FileDataSink, SinkOptions, WriteMode};
use fsspec_data::{CancellationToken, SharedFileSystem};
use fsspec_rs::MemoryFs;

fn batch(start: i64) -> RecordBatch {
    RecordBatch::try_from_iter([(
        "value",
        Arc::new(Int64Array::from(vec![start, start + 1, start + 2])) as _,
    )])
    .unwrap()
}
fn writer(fs: &SharedFileSystem, root: &str, mode: WriteMode) -> FileDataSink {
    FileDataSink::open(
        fs.clone(),
        root,
        mode,
        batch(0).schema(),
        SinkOptions::default(),
    )
    .unwrap()
}
fn follower(fs: &SharedFileSystem, root: &str, options: FollowOptions) -> ManifestFollower {
    ManifestFollower::new(
        ManifestReader::new(fs.clone(), root, ReadLimits::default()).unwrap(),
        root.into(),
        options,
        CancellationToken::new(),
    )
    .unwrap()
}
fn setup() -> (SharedFileSystem, String, FileDataSink) {
    let fs: SharedFileSystem = Arc::new(MemoryFs::new());
    let root = format!("/{}", uuid::Uuid::new_v4());
    let mut sink = writer(&fs, &root, WriteMode::Create);
    sink.write_batch(0, "first", &batch(0)).unwrap();
    sink.commit().unwrap();
    (fs, root, sink)
}
#[test]
fn resume_across_parts_and_generations_without_duplicates() {
    let (fs, root, mut sink) = setup();
    let mut follow = follower(
        &fs,
        &root,
        FollowOptions {
            batch_size: 2,
            ..Default::default()
        },
    );
    let first = follow.next().unwrap().unwrap();
    assert_eq!(first.batch.num_rows(), 2);
    assert_eq!(first.position.progress, 2);
    let start: FollowPosition =
        serde_json::from_str(&serde_json::to_string(&first.position).unwrap()).unwrap();
    sink.write_batch(3, "second", &batch(3)).unwrap();
    sink.commit().unwrap();
    sink.write_batch(6, "third", &batch(6)).unwrap();
    sink.finish().unwrap();
    let mut resumed = follower(
        &fs,
        &root,
        FollowOptions {
            start: Some(start),
            batch_size: 2,
            ..Default::default()
        },
    );
    let items: Vec<_> = resumed.by_ref().map(Result::unwrap).collect();
    let values: Vec<_> = items
        .iter()
        .flat_map(|item| {
            item.batch
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .values()
                .to_vec()
        })
        .collect();
    assert_eq!(values, (2..9).collect::<Vec<_>>());
    assert_eq!(
        items
            .iter()
            .map(|i| i.position.generation)
            .collect::<Vec<_>>(),
        [0, 1, 1, 2, 2]
    );
    assert_eq!(resumed.end_reason(), Some(FollowEnd::Complete));
    assert!(resumed.next().is_none());
    assert_eq!(resumed.position().progress, 9);
}
#[test]
fn replacement_is_terminal_even_when_later_generations_exist() {
    let (fs, root, _sink) = setup();
    let mut follow = follower(&fs, &root, FollowOptions::default());
    follow.next().unwrap().unwrap();
    let checkpoint = follow.position().clone();
    let mut replacement = writer(&fs, &root, WriteMode::Replace);
    replacement.write_batch(0, "other", &batch(100)).unwrap();
    replacement.commit().unwrap();
    replacement.write_batch(3, "other2", &batch(200)).unwrap();
    replacement.commit().unwrap();
    assert!(follow.next().is_none());
    assert_eq!(follow.end_reason(), Some(FollowEnd::Replaced));
    assert_eq!(follow.position(), &checkpoint);
    let mut resumed = follower(
        &fs,
        &root,
        FollowOptions {
            start: Some(checkpoint),
            ..Default::default()
        },
    );
    assert!(resumed.next().is_none());
    assert_eq!(resumed.end_reason(), Some(FollowEnd::Replaced));
}
#[test]
fn zero_idle_timeout_drains_available_rows_and_cancellation_wakes_waits() {
    let (fs, root, _sink) = setup();
    let mut follow = follower(
        &fs,
        &root,
        FollowOptions {
            idle_timeout: Some(Duration::ZERO),
            ..Default::default()
        },
    );
    follow.next().unwrap().unwrap();
    assert!(follow.next().is_none());
    assert_eq!(follow.end_reason(), Some(FollowEnd::IdleTimeout));
    let mut follow = follower(
        &fs,
        &root,
        FollowOptions {
            poll_interval: Duration::from_secs(60),
            ..Default::default()
        },
    );
    let cancel = follow.cancellation_token();
    follow.next().unwrap().unwrap();
    let thread = std::thread::spawn(move || {
        assert!(follow.next().is_none());
        follow.end_reason()
    });
    std::thread::sleep(Duration::from_millis(10));
    cancel.cancel();
    assert_eq!(thread.join().unwrap(), Some(FollowEnd::Cancelled));
}
#[test]
fn invalid_positions_and_options_fail_before_iteration() {
    let (fs, root, _sink) = setup();
    let saved = follower(&fs, &root, FollowOptions::default())
        .position()
        .clone();
    for field in [
        "source",
        "namespace_id",
        "table_id",
        "generation",
        "manifest_sha256",
        "progress",
        "progress_unit",
    ] {
        let mut value = serde_json::to_value(&saved).unwrap();
        value[field] = match field {
            "generation" => 99.into(),
            "progress" => 99.into(),
            "manifest_sha256" => "00".repeat(32).into(),
            _ => "other".into(),
        };
        let start = serde_json::from_value(value).unwrap();
        let result = ManifestFollower::new(
            ManifestReader::new(fs.clone(), &root, ReadLimits::default()).unwrap(),
            root.clone(),
            FollowOptions {
                start: Some(start),
                ..Default::default()
            },
            CancellationToken::new(),
        );
        assert!(result.is_err(), "{field}");
    }
    for options in [
        FollowOptions {
            batch_size: 0,
            ..Default::default()
        },
        FollowOptions {
            poll_interval: Duration::ZERO,
            ..Default::default()
        },
        FollowOptions {
            columns: Some(vec!["missing".into()]),
            ..Default::default()
        },
    ] {
        assert!(ManifestFollower::new(
            ManifestReader::new(fs.clone(), &root, ReadLimits::default()).unwrap(),
            root.clone(),
            options,
            CancellationToken::new()
        )
        .is_err());
    }
}

#[test]
fn storage_failure_is_terminal_without_advancing_a_checkpoint() {
    let (fs, root, _sink) = setup();
    let mut follow = follower(&fs, &root, FollowOptions::default());
    let saved = follow.position().clone();
    let snapshot = ManifestReader::new(fs.clone(), &root, ReadLimits::default())
        .unwrap()
        .read(fsspec_data::manifest::Selection::Latest)
        .unwrap();
    fs.rm_file(&format!("{root}/{}", snapshot.manifest().parts[0].key))
        .unwrap();
    assert!(follow.next().unwrap().is_err());
    assert_eq!(follow.position(), &saved);
    assert!(follow.next().is_none());
    assert_eq!(follow.end_reason(), None);
}

#[test]
fn malformed_successor_propagates_error_and_empty_finish_preserves_checkpoint() {
    let (fs, root, mut sink) = setup();
    let mut follow = follower(&fs, &root, FollowOptions::default());
    let saved = follow.next().unwrap().unwrap().position;
    sink.finish().unwrap();
    let mut complete = follower(
        &fs,
        &root,
        FollowOptions {
            start: Some(saved.clone()),
            ..Default::default()
        },
    );
    assert!(complete.next().is_none());
    assert_eq!(complete.end_reason(), Some(FollowEnd::Complete));
    assert_eq!(complete.position(), &saved);
    fs.write(
        &format!("{root}/manifests/00000000000000000001/manifest.json"),
        b"bad",
        Default::default(),
    )
    .unwrap();
    assert!(follow.next().unwrap().is_err());
    assert_eq!(follow.position(), &saved);
    assert!(follow.next().is_none());
}

#[test]
fn state_keeps_event_progress_separate_and_follow_rejects_it() {
    let (fs, root, _sink) = setup();
    let reader = ManifestReader::new(fs.clone(), &root, ReadLimits::default()).unwrap();
    let mut m = reader
        .read(fsspec_data::manifest::Selection::Latest)
        .unwrap()
        .manifest()
        .clone();
    m.progress_unit = "events".into();
    m.progress = 1;
    m.parts[0].progress.end = 1;
    m.receipts[0].progress.end = 1;
    m.partition_values
        .insert("date".into(), "2026-11-27".into());
    fs.write(
        &format!("{root}/manifests/00000000000000000000/manifest.json"),
        &m.encode().unwrap(),
        Default::default(),
    )
    .unwrap();
    let state = reader
        .read(fsspec_data::manifest::Selection::Latest)
        .unwrap()
        .state();
    assert_eq!(state["progress"], 1);
    assert_eq!(state["num_rows"], 3);
    assert_eq!(state["num_rows_exact"], true);
    assert_eq!(state["complete"], false);
    assert_eq!(state["distinct.date"], serde_json::json!(["2026-11-27"]));
    assert!(!state.contains_key("modified"));
    assert!(ManifestFollower::new(
        reader,
        root,
        FollowOptions::default(),
        CancellationToken::new()
    )
    .is_err());
}

#[test]
fn deletion_ends_follow_and_new_follow_rejects_tombstone() {
    use fsspec_data::manifest::{Parent, PublicationKind, Selection};
    let (fs, root, _sink) = setup();
    let mut follow = follower(&fs, &root, FollowOptions::default());
    follow.next().unwrap().unwrap();
    let reader = ManifestReader::new(fs.clone(), &root, ReadLimits::default()).unwrap();
    let snapshot = reader.read(Selection::Latest).unwrap();
    let mut m = snapshot.manifest().clone();
    m.generation = 1;
    m.parent = Some(Parent {
        generation: 0,
        sha256: snapshot.checksum(),
    });
    m.kind = PublicationKind::Delete;
    m.publication_id = "deletion".into();
    m.table_id = "deleted".into();
    m.deleted = true;
    m.parts.clear();
    m.receipts.clear();
    m.num_rows = 0;
    m.progress = 0;
    fs.write(
        &format!("{root}/manifests/00000000000000000001/manifest.json"),
        &m.encode().unwrap(),
        Default::default(),
    )
    .unwrap();
    assert!(follow.next().is_none());
    assert_eq!(follow.end_reason(), Some(FollowEnd::Replaced));
    assert!(ManifestFollower::new(
        reader,
        root,
        FollowOptions::default(),
        CancellationToken::new()
    )
    .is_err());
}

#[test]
fn parquet_scan_rejects_invalid_encoding_metadata_and_contents() {
    use fsspec_data::manifest::{Checksum, Selection};
    use fsspec_data::{ScanSelection, StreamOptions};
    for change in ["encoding", "options", "schema", "rows", "bytes"] {
        let (fs, root, _sink) = setup();
        let reader = ManifestReader::new(fs.clone(), &root, ReadLimits::default()).unwrap();
        let mut m = reader.read(Selection::Latest).unwrap().manifest().clone();
        match change {
            "encoding" => m.parts[0].encoding = "arrow".into(),
            "options" => {
                m.parts[0].options.insert("unknown".into(), true.into());
            }
            "schema" => {
                m.schema = arrow::datatypes::Schema::new(vec![arrow::datatypes::Field::new(
                    "other",
                    arrow::datatypes::DataType::Int64,
                    false,
                )]);
            }
            "rows" => {
                m.num_rows = 4;
                m.progress = 4;
                m.parts[0].num_rows = 4;
                m.parts[0].progress.end = 4;
                m.receipts[0].progress.end = 4;
            }
            "bytes" => {
                fs.write(
                    &format!("{root}/{}", m.parts[0].key),
                    b"bad parquet",
                    Default::default(),
                )
                .unwrap();
                m.parts[0].sha256 = Checksum::of(b"bad parquet");
                m.parts[0].size = 11;
            }
            _ => unreachable!(),
        }
        fs.write(
            &format!("{root}/manifests/00000000000000000000/manifest.json"),
            &m.encode().unwrap(),
            Default::default(),
        )
        .unwrap();
        let snapshot = Arc::new(reader.read(Selection::Latest).unwrap());
        let mut scan = snapshot
            .scan(
                ScanSelection::default(),
                StreamOptions::default(),
                CancellationToken::new(),
            )
            .unwrap();
        assert!(scan.next().unwrap().is_err(), "{change}");
        assert!(scan.next().is_none());
    }
}

#[test]
fn positive_idle_timeout_and_schema_projection() {
    let (fs, root, _sink) = setup();
    let mut follow = follower(
        &fs,
        &root,
        FollowOptions {
            columns: Some(vec![]),
            idle_timeout: Some(Duration::from_millis(5)),
            poll_interval: Duration::from_millis(1),
            ..Default::default()
        },
    );
    assert!(follow.schema().fields().is_empty());
    assert_eq!(follow.next().unwrap().unwrap().batch.num_rows(), 3);
    assert!(follow.next().is_none());
    assert_eq!(follow.end_reason(), Some(FollowEnd::IdleTimeout));
}

#[test]
fn rewritten_history_does_not_silently_change_the_followed_data() {
    use fsspec_data::manifest::{Checksum, Selection};
    for new_namespace in [false, true] {
        let (fs, root, mut sink) = setup();
        let mut follow = follower(&fs, &root, FollowOptions::default());
        follow.next().unwrap().unwrap();
        let saved = follow.position().clone();
        sink.finish().unwrap();
        let reader = ManifestReader::new(fs.clone(), &root, ReadLimits::default()).unwrap();
        let mut first = reader
            .read(Selection::Generation(0))
            .unwrap()
            .manifest()
            .clone();
        let mut next = reader
            .read(Selection::Generation(1))
            .unwrap()
            .manifest()
            .clone();
        if new_namespace {
            first.namespace_id = "another-namespace".into();
            next.namespace_id = first.namespace_id.clone();
        } else {
            first.table_id = "another-table".into();
            next.table_id = first.table_id.clone();
        }
        let first = first.encode().unwrap();
        next.parent.as_mut().unwrap().sha256 = Checksum::of(&first);
        fs.write(
            &format!("{root}/manifests/00000000000000000000/manifest.json"),
            &first,
            Default::default(),
        )
        .unwrap();
        fs.write(
            &format!("{root}/manifests/00000000000000000001/manifest.json"),
            &next.encode().unwrap(),
            Default::default(),
        )
        .unwrap();
        if new_namespace {
            assert!(follow.next().is_none());
            assert_eq!(follow.end_reason(), Some(FollowEnd::Replaced));
        } else {
            assert!(follow.next().unwrap().is_err());
        }
        assert_eq!(follow.position(), &saved);
    }
}
