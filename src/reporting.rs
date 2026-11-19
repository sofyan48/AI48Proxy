//! Transport-independent, bounded, optionally durable at-least-once usage delivery.
//! Send each pending payload with LPUSH usage; acknowledge only a successful integer reply.
//! Queue ownership may outlive membership, unlike the execution release ledger.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[path = "reporting/spool.rs"]
mod spool;
#[path = "reporting/usage.rs"]
mod usage;
pub use usage::{TokenBreakdown, UsageRecord, usage_payload};

#[derive(Clone, Copy, Debug)]
pub struct QueueLimits {
    pub max_items: usize,
    pub max_bytes: usize,
    pub max_payload_bytes: usize,
    /// Recently acknowledged IDs retained for local deduplication. Home dedupes older replays.
    pub max_recent_ids: usize,
}

impl Default for QueueLimits {
    fn default() -> Self {
        Self {
            max_items: 4096,
            max_bytes: 16 * 1024 * 1024,
            max_payload_bytes: 256 * 1024,
            max_recent_ids: 4096,
        }
    }
}

/// No Debug implementation: payloads may contain sensitive caller-provided fields.
#[derive(Clone)]
pub struct Delivery {
    pub id: String,
    pub payload: Arc<[u8]>,
}

impl Delivery {
    pub fn command(&self) -> Vec<Vec<u8>> {
        vec![b"LPUSH".to_vec(), b"usage".to_vec(), self.payload.to_vec()]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnqueueError {
    NotObject,
    InvalidId,
    TooLarge,
    Full,
    ConflictingId,
    /// The event was not accepted; retain and retry it with the same execution ID.
    Persistence,
}

struct Entry {
    delivery: Delivery,
    digest: [u8; 32],
}

#[derive(Default)]
struct State {
    pending: VecDeque<Entry>,
    bytes: usize,
    recent: BTreeMap<String, [u8; 32]>,
    recent_order: VecDeque<String>,
    next_sequence: u64,
    persistence_failed: bool,
}

pub struct ReportingQueue {
    limits: QueueLimits,
    state: Mutex<State>,
    spool: Option<spool::Spool>,
}

impl Default for ReportingQueue {
    fn default() -> Self {
        Self::new(QueueLimits::default())
    }
}

impl ReportingQueue {
    pub fn new(limits: QueueLimits) -> Self {
        Self {
            limits,
            state: Mutex::new(State::default()),
            spool: None,
        }
    }

    /// Open an exclusively owned durable outbox. Invalid records or replay exceeding
    /// the supplied limits fail closed without deleting pending data. Recent ACK
    /// deduplication is process-local; Home must deduplicate older execution IDs.
    pub fn open(path: &Path, limits: QueueLimits) -> anyhow::Result<Self> {
        let (spool, state) = spool::Spool::open(path, limits)?;
        Ok(Self {
            limits,
            state: Mutex::new(state),
            spool: Some(spool),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Preserve execution_id if supplied; otherwise allocate once before storing immutable bytes.
    /// On Full the caller must retain/retry the original event; nothing is evicted silently.
    /// Re-enqueueing an identical ID/payload is idempotent; a changed payload is an error.
    pub fn enqueue(&self, mut payload: Value) -> Result<String, EnqueueError> {
        let object = payload.as_object_mut().ok_or(EnqueueError::NotObject)?;
        let id = match object.get("execution_id") {
            None => Uuid::new_v4().to_string(),
            Some(Value::String(id)) if !id.trim().is_empty() && id.len() <= 256 => id.clone(),
            _ => return Err(EnqueueError::InvalidId),
        };
        object.insert("execution_id".into(), Value::String(id.clone()));
        let bytes = serde_json::to_vec(&payload).map_err(|_| EnqueueError::TooLarge)?;
        if bytes.len() > self.limits.max_payload_bytes || bytes.len() > self.limits.max_bytes {
            return Err(EnqueueError::TooLarge);
        }
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        let mut state = self.state();
        if state.persistence_failed {
            return Err(EnqueueError::Persistence);
        }
        let existing = state
            .pending
            .iter()
            .find(|entry| entry.delivery.id == id)
            .map(|entry| &entry.digest)
            .or_else(|| state.recent.get(&id));
        if let Some(existing) = existing {
            return if *existing == digest {
                Ok(id)
            } else {
                Err(EnqueueError::ConflictingId)
            };
        }
        if state.pending.len() >= self.limits.max_items
            || bytes.len() > self.limits.max_bytes.saturating_sub(state.bytes)
        {
            return Err(EnqueueError::Full);
        }
        let next_sequence = state
            .next_sequence
            .checked_add(1)
            .ok_or(EnqueueError::Persistence)?;
        if let Some(spool) = &self.spool
            && spool.store(&id, &bytes, state.next_sequence).is_err()
        {
            // Publication may have succeeded before fsync failed. Require
            // reopening/replay rather than reusing capacity or sequence IDs.
            state.persistence_failed = true;
            return Err(EnqueueError::Persistence);
        }
        state.next_sequence = next_sequence;
        state.bytes += bytes.len();
        state.pending.push_back(Entry {
            delivery: Delivery {
                id: id.clone(),
                payload: bytes.into(),
            },
            digest,
        });
        Ok(id)
    }

    /// Non-destructive FIFO batch. Failed/ambiguous sends remain pending with identical bytes.
    pub fn pending_batch(&self, max_items: usize, max_bytes: usize) -> Vec<Delivery> {
        let state = self.state();
        let mut bytes = 0usize;
        let mut batch = Vec::new();
        for entry in state.pending.iter().take(max_items) {
            if entry.delivery.payload.len() > max_bytes.saturating_sub(bytes) {
                break;
            }
            bytes += entry.delivery.payload.len();
            batch.push(entry.delivery.clone());
        }
        batch
    }

    /// Exact ID acknowledgment permits independently delivered/out-of-order batches.
    /// A persistence failure returns false and retains the entry for safe retry.
    /// A crash during ACK may replay an already delivered event (at-least-once).
    pub fn acknowledge(&self, id: &str) -> bool {
        let mut state = self.state();
        let Some(index) = state
            .pending
            .iter()
            .position(|entry| entry.delivery.id == id)
        else {
            return false;
        };
        if let Some(spool) = &self.spool
            && spool.acknowledge(id).is_err()
        {
            return false;
        }
        let entry = state
            .pending
            .remove(index)
            .expect("index found while locked");
        state.bytes -= entry.delivery.payload.len();
        if self.limits.max_recent_ids > 0 {
            state.recent.insert(entry.delivery.id.clone(), entry.digest);
            state.recent_order.push_back(entry.delivery.id);
            while state.recent_order.len() > self.limits.max_recent_ids {
                if let Some(old) = state.recent_order.pop_front() {
                    state.recent.remove(&old);
                }
            }
        }
        true
    }

    pub fn len(&self) -> usize {
        self.state().pending.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn pending_bytes(&self) -> usize {
        self.state().bytes
    }
}

#[cfg(test)]
#[path = "reporting/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "reporting/durable_tests.rs"]
mod durable_tests;
