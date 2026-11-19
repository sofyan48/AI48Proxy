use super::*;
use crate::config::Observation;
use std::time::Duration;

fn begin(registry: &Arc<Registry>, accounted: bool) -> ExecutionGuard {
    registry.begin(
        "cred".into(),
        "model".into(),
        accounted,
        "req".into(),
        "chat".into(),
    )
}

fn limits() -> Observation {
    Observation {
        snapshot_interval: Duration::from_secs(2),
        max_part_bytes: 1024,
        max_part_count: 64,
        max_revision_bytes: 65536,
        max_aggregate_groups: 100,
        max_details: 100,
        max_string_bytes: 256,
    }
}

#[test]
fn completion_is_exactly_once_and_ack_is_cumulative() {
    let registry = Registry::new(CancellationToken::new());
    let mut first = begin(&registry, true);
    let old = first.finish().unwrap();
    assert_eq!(old.release_seq, 1);
    assert!(first.finish().is_none());
    drop(first);
    drop(begin(&registry, false));
    drop(begin(&registry, true));
    assert_eq!(registry.pending_releases()[0].release_seq, 2);
    registry.acknowledge(&old);
    assert_eq!(registry.pending_releases()[0].release_seq, 2);
    let mut future = old.clone();
    future.release_seq = 3;
    registry.acknowledge(&future);
    assert!(!registry.is_drained());
    registry.acknowledge(&registry.pending_releases()[0]);
    assert!(registry.is_drained());
    drop(begin(&registry, true));
    assert_eq!(registry.pending_releases()[0].release_seq, 3);
}

#[test]
fn cancellation_is_scoped_and_does_not_finish_work() {
    let lifetime = CancellationToken::new();
    let registry = Registry::new(lifetime.clone());
    let mut first = begin(&registry, true);
    let second = begin(&registry, true);
    first.cancellation().cancel();
    assert!(!second.cancellation().is_cancelled());
    assert!(!lifetime.is_cancelled());
    first.finish();
    lifetime.cancel();
    assert!(second.cancellation().is_cancelled());
    assert_eq!(registry.active_count(), 1);
    assert_eq!(registry.pending_releases()[0].release_seq, 1);
    drop(second);
    assert_eq!(registry.pending_releases()[0].release_seq, 2);
    let other = Registry::new(CancellationToken::new());
    drop(begin(&other, true));
    assert_eq!(other.pending_releases()[0].release_seq, 1);
}

#[test]
fn admission_handoff_and_unwind_drop() {
    let registry = Registry::new(CancellationToken::new());
    let admission = registry.admission();
    assert!(!registry.is_drained());
    assert_eq!(registry.pending_admissions(), 1);
    let execution = admission.begin(
        "cred".into(),
        "model".into(),
        true,
        "req".into(),
        "chat".into(),
    );
    assert_eq!(registry.pending_admissions(), 0);
    assert!(!registry.is_drained());
    drop(execution);
    let result = std::panic::catch_unwind({
        let registry = Arc::clone(&registry);
        move || {
            let _guard = begin(&registry, true);
            panic!("test unwind");
        }
    });
    assert!(result.is_err());
    assert_eq!(registry.active_count(), 0);
    assert_eq!(registry.pending_releases()[0].release_seq, 2);
}

#[test]
fn unresolved_admission_fences_lifetime_but_explicit_rejection_does_not() {
    let lifetime = CancellationToken::new();
    let registry = Registry::new(lifetime.clone());
    let mut rejected = registry.admission();
    rejected.finish();
    drop(rejected);
    assert!(!lifetime.is_cancelled());
    drop(registry.admission());
    assert!(lifetime.is_cancelled());
    assert_eq!(registry.pending_admissions(), 0);
}

#[test]
fn concurrent_completion_and_stale_acks_do_not_lose_releases() {
    let registry = Registry::new(CancellationToken::new());
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || {
                for _ in 0..100 {
                    drop(begin(&registry, true));
                    for frame in registry.pending_releases() {
                        registry.acknowledge(&frame);
                    }
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    drop(begin(&registry, true));
    let frame = registry.pending_releases().pop().unwrap();
    assert_eq!(frame.release_seq, 801);
    registry.acknowledge(&frame);
    assert!(registry.is_drained());
}

#[test]
fn snapshots_partition_complete_aggregates_and_details() {
    let registry = Registry::new(CancellationToken::new());
    let guards: Vec<_> = (0..30)
        .map(|n| {
            registry.begin(
                format!("cred-{n}"),
                "model".into(),
                n % 2 == 0,
                format!("req-{n}"),
                "chat".into(),
            )
        })
        .collect();
    let time = DateTime::parse_from_rfc3339("2026-07-21T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let snapshot = registry.snapshot(&limits(), time, 42).unwrap();
    assert!(snapshot.frames.len() > 1);
    assert_eq!(
        snapshot
            .frames
            .iter()
            .map(|f| f.aggregates.len())
            .sum::<usize>(),
        30
    );
    assert_eq!(
        snapshot
            .frames
            .iter()
            .map(|f| f.details.len())
            .sum::<usize>(),
        30
    );
    for (index, frame) in snapshot.frames.iter().enumerate() {
        assert_eq!(frame.part_index, Some(index));
        assert_eq!(frame.part_count, Some(snapshot.frames.len()));
        assert_eq!(frame.observed_at, time);
        assert_eq!(frame.barrier_revision, 42);
        assert!(serde_json::to_vec(frame).unwrap().len() <= limits().max_part_bytes);
        assert!(!frame.details_truncated);
    }
    drop(guards);
    let empty = registry.snapshot(&limits(), time, 42).unwrap();
    assert_eq!(empty.revision, snapshot.revision + 1);
    assert_eq!(empty.frames.len(), 1);
    assert!(empty.frames[0].aggregates.is_empty());
}

#[test]
fn snapshot_truncates_details_but_never_aggregates() {
    let registry = Registry::new(CancellationToken::new());
    let _a = begin(&registry, true);
    let _b = begin(&registry, false);
    let mut config = limits();
    config.max_details = 1;
    let frame = registry
        .snapshot(&config, Utc::now(), 0)
        .unwrap()
        .frames
        .remove(0);
    assert!(frame.details_truncated);
    assert_eq!(frame.details.len(), 1);
    assert_eq!(frame.aggregates.len(), 2);
    config.max_aggregate_groups = 1;
    let frame = registry
        .snapshot(&config, Utc::now(), 0)
        .unwrap()
        .frames
        .remove(0);
    assert_eq!(frame.kind, "overflow");
    assert_eq!(frame.aggregate_group_count, 2);
    assert!(frame.part_index.is_none());
    assert!(frame.aggregates.is_empty());
    let json = serde_json::to_value(frame).unwrap();
    for forbidden in [
        "part_index",
        "part_count",
        "details_truncated",
        "aggregates",
        "details",
    ] {
        assert!(
            json.get(forbidden).is_none(),
            "overflow includes {forbidden}"
        );
    }
}

#[test]
fn snapshot_byte_limits_and_string_identity_are_strict() {
    let registry = Registry::new(CancellationToken::new());
    let _guard = registry.begin(
        "long-credential".into(),
        "model".into(),
        true,
        "req".into(),
        "chat".into(),
    );
    let mut config = limits();
    config.max_string_bytes = 4;
    assert_eq!(
        registry.snapshot(&config, Utc::now(), 0).unwrap().frames[0].kind,
        "overflow"
    );
    config = limits();
    config.max_part_bytes = 1;
    assert_eq!(
        registry.snapshot(&config, Utc::now(), 0).unwrap_err(),
        SnapshotError::CannotEncode
    );
    config = limits();
    config.max_part_count = 1;
    config.max_part_bytes = 300;
    let aggregate_only = registry.snapshot(&config, Utc::now(), 0).unwrap();
    assert_eq!(aggregate_only.frames[0].kind, "part");
    assert_eq!(aggregate_only.frames[0].aggregates.len(), 1);
    assert!(aggregate_only.frames[0].details.is_empty());
    assert!(aggregate_only.frames[0].details_truncated);
    config = limits();
    config.max_revision_bytes = 200;
    let result = registry.snapshot(&config, Utc::now(), 0).unwrap();
    assert_eq!(result.frames[0].kind, "overflow");
}
