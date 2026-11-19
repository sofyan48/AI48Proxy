use super::*;
use serde_json::json;
use std::fs;
use tempfile::tempdir;

fn open(path: &Path) -> ReportingQueue {
    ReportingQueue::open(path, QueueLimits::default()).unwrap()
}

fn files(path: &Path) -> Vec<std::path::PathBuf> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect()
}

#[test]
fn restart_preserves_fifo_generated_ids_exact_bytes_and_out_of_order_ack() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("nested/outbox");
    let queue = open(&path);
    let first = queue
        .enqueue(json!({"model": "a\n\"\\", "tokens": 42}))
        .unwrap();
    let second = queue
        .enqueue(json!({"execution_id": "../../secret", "model": "b"}))
        .unwrap();
    let third = queue.enqueue(json!({"model": "c"})).unwrap();
    let before = queue.pending_batch(10, usize::MAX);
    assert!(queue.acknowledge(&second));
    drop(queue);
    let queue = open(&path);
    let after = queue.pending_batch(10, usize::MAX);
    assert_eq!(
        after.iter().map(|entry| &entry.id).collect::<Vec<_>>(),
        vec![&first, &third]
    );
    assert_eq!(after[0].payload, before[0].payload);
    assert_eq!(after[1].payload, before[2].payload);
    assert_eq!(
        queue.pending_bytes(),
        after.iter().map(|e| e.payload.len()).sum::<usize>()
    );
    queue
        .enqueue(serde_json::from_slice(&after[0].payload).unwrap())
        .unwrap();
    assert_eq!(
        queue.enqueue(json!({"execution_id": first, "model": "changed"})),
        Err(EnqueueError::ConflictingId)
    );
    assert!(queue.acknowledge(&first));
    assert!(queue.acknowledge(&third));
    drop(queue);
    assert!(open(&path).is_empty());
    assert!(files(&path).is_empty());
}

#[test]
fn corrupt_truncated_and_unknown_records_fail_closed_without_deletion() {
    for corruption in [b"{".as_slice(), b"not json", b"{}", b"{\"version\":1}"] {
        let directory = tempdir().unwrap();
        let queue = open(directory.path());
        queue.enqueue(json!({"execution_id": "one"})).unwrap();
        queue.enqueue(json!({"execution_id": "two"})).unwrap();
        drop(queue);
        let path = files(directory.path()).remove(0);
        fs::write(&path, corruption).unwrap();
        assert!(ReportingQueue::open(directory.path(), QueueLimits::default()).is_err());
        assert_eq!(files(directory.path()).len(), 2);
        assert_eq!(fs::read(path).unwrap(), corruption);
    }
    let directory = tempdir().unwrap();
    drop(open(directory.path()));
    fs::write(directory.path().join("unexpected"), b"sensitive").unwrap();
    let error = ReportingQueue::open(directory.path(), QueueLimits::default())
        .err()
        .unwrap();
    assert!(!format!("{error:#}").contains("sensitive"));
    assert!(directory.path().join("unexpected").exists());
}

#[test]
fn checksum_and_filename_corruption_are_rejected() {
    for rename in [false, true] {
        let directory = tempdir().unwrap();
        let queue = open(directory.path());
        queue
            .enqueue(json!({"execution_id": "one", "model": "a"}))
            .unwrap();
        drop(queue);
        let path = files(directory.path()).remove(0);
        if rename {
            fs::rename(
                &path,
                directory.path().join(format!("{}.json", "0".repeat(64))),
            )
            .unwrap();
        } else {
            let mut record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            record["payload"] = json!(
                record["payload"]
                    .as_str()
                    .unwrap()
                    .replace("\"a\"", "\"b\"")
            );
            fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        }
        assert!(ReportingQueue::open(directory.path(), QueueLimits::default()).is_err());
        assert_eq!(files(directory.path()).len(), 1);
    }
}

#[test]
fn restart_enforces_all_limits_without_evicting() {
    let directory = tempdir().unwrap();
    let queue = open(directory.path());
    for id in ["one", "two"] {
        queue.enqueue(json!({"execution_id": id})).unwrap();
    }
    let bytes = queue.pending_bytes();
    let payload_bytes = queue.pending_batch(1, usize::MAX)[0].payload.len();
    drop(queue);
    for limits in [
        QueueLimits {
            max_items: 1,
            ..Default::default()
        },
        QueueLimits {
            max_bytes: bytes - 1,
            ..Default::default()
        },
        QueueLimits {
            max_payload_bytes: payload_bytes - 1,
            ..Default::default()
        },
    ] {
        assert!(ReportingQueue::open(directory.path(), limits).is_err());
        assert_eq!(files(directory.path()).len(), 2);
    }
    let queue = ReportingQueue::open(
        directory.path(),
        QueueLimits {
            max_items: 2,
            max_bytes: bytes,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        queue.enqueue(json!({"execution_id": "new"})),
        Err(EnqueueError::Full)
    );
    assert!(queue.acknowledge("one"));
    queue.enqueue(json!({"execution_id": "new"})).unwrap();
}

#[test]
fn durable_concurrent_ids_are_unique_and_same_id_is_idempotent() {
    let directory = tempdir().unwrap();
    let queue = Arc::new(open(directory.path()));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                for _ in 0..10 {
                    queue.enqueue(json!({"model": "model"})).unwrap();
                    queue.enqueue(json!({"execution_id": "same"})).unwrap();
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let before = queue.pending_batch(100, usize::MAX);
    let ids: std::collections::BTreeSet<_> = before.iter().map(|e| &e.id).collect();
    assert_eq!(ids.len(), 81);
    drop(queue);
    let after = open(directory.path()).pending_batch(100, usize::MAX);
    assert_eq!(before.len(), after.len());
    for (before, after) in before.iter().zip(&after) {
        assert_eq!(before.id, after.id);
        assert_eq!(before.payload, after.payload);
    }
}

#[test]
fn process_exit_without_drop_preserves_enqueue_and_ack() {
    const CHILD_PATH: &str = "AI48PROXY_REPORTING_CRASH_TEST_PATH";
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        let queue = open(Path::new(&path));
        queue.enqueue(json!({"execution_id": "acked"})).unwrap();
        queue.enqueue(json!({"execution_id": "pending"})).unwrap();
        assert!(queue.acknowledge("acked"));
        // Skip destructors; durability cannot depend on orderly queue shutdown.
        std::process::exit(0);
    }
    let directory = tempdir().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("process_exit_without_drop_preserves_enqueue_and_ack")
        .env(CHILD_PATH, directory.path())
        .status()
        .unwrap();
    assert!(status.success());
    let queue = open(directory.path());
    let batch = queue.pending_batch(10, usize::MAX);
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].id, "pending");
}

#[test]
fn exclusive_lock_and_unpublished_temporary_records() {
    let directory = tempdir().unwrap();
    let queue = open(directory.path());
    assert!(ReportingQueue::open(directory.path(), QueueLimits::default()).is_err());
    queue.enqueue(json!({"execution_id": "one"})).unwrap();
    let temporary = directory.path().join(format!(".tmp-{}", Uuid::new_v4()));
    fs::write(&temporary, b"truncated unpublished write").unwrap();
    drop(queue);
    let queue = open(directory.path());
    assert_eq!(queue.len(), 1);
    assert!(temporary.exists());
}

#[test]
fn persistence_failure_never_accepts_or_discards_pending_work() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("outbox");
    let moved = directory.path().join("moved");
    let queue = open(&path);
    queue.enqueue(json!({"execution_id": "one"})).unwrap();
    fs::rename(&path, &moved).unwrap();
    assert!(!queue.acknowledge("one"));
    assert_eq!(queue.len(), 1);
    assert_eq!(
        queue.enqueue(json!({"execution_id": "two"})),
        Err(EnqueueError::Persistence)
    );
    assert_eq!(queue.len(), 1);
    fs::rename(&moved, &path).unwrap();
    drop(queue);
    assert_eq!(open(&path).len(), 1);
}

#[cfg(unix)]
#[test]
fn private_permissions_and_symlink_rejection() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = tempdir().unwrap();
    let path = directory.path().join("outbox");
    let queue = open(&path);
    queue.enqueue(json!({"execution_id": "one"})).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for entry in fs::read_dir(&path).unwrap() {
        assert_eq!(
            entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    drop(queue);
    let external = directory.path().join("external");
    fs::write(&external, b"do not touch").unwrap();
    let record = files(&path).remove(0);
    fs::remove_file(&record).unwrap();
    symlink(&external, record).unwrap();
    assert!(ReportingQueue::open(&path, QueueLimits::default()).is_err());
    assert_eq!(fs::read(external).unwrap(), b"do not touch");
    let alias = directory.path().join("alias");
    symlink(&path, &alias).unwrap();
    assert!(ReportingQueue::open(&alias, QueueLimits::default()).is_err());
}
