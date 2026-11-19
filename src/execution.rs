//! One registry belongs to exactly one accepted Home membership; never reuse it on reconnect.
//! Completion is synchronous, including Drop. Cancellation requests do not imply completion.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[path = "execution/snapshot.rs"]
mod snapshot;
pub use snapshot::{Snapshot, SnapshotError, SnapshotFrame};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseFrame {
    pub credential_id: String,
    pub model: String,
    pub release_seq: i64,
}

type Key = (String, String);

#[derive(Default)]
struct Ledger {
    completed: i64,
    acknowledged: i64,
}

struct Active {
    credential_id: String,
    model: String,
    accounted: bool,
    request_id: String,
    request_kind: String,
    started_at: DateTime<Utc>,
}

#[derive(Default)]
struct State {
    next_id: u64,
    active: BTreeMap<u64, Active>,
    releases: BTreeMap<Key, Ledger>,
    admissions: usize,
    revision: i64,
    release_exhausted: bool,
}

pub struct Registry {
    lifetime_cancel: CancellationToken,
    state: Mutex<State>,
}

impl Registry {
    pub fn new(lifetime_cancel: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            lifetime_cancel,
            state: Mutex::new(State::default()),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // No user code runs while locked. Preserve completion even during stack unwinding.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The caller must not start upstream work if the returned cancellation is already set.
    /// An accounted dispatch still needs a guard even when cancellation raced its response.
    pub fn begin(
        self: &Arc<Self>,
        credential_id: String,
        model: String,
        accounted: bool,
        request_id: String,
        request_kind: String,
    ) -> ExecutionGuard {
        let mut state = self.state();
        let id = state.next_id;
        state.next_id = state
            .next_id
            .checked_add(1)
            .expect("execution identity exhausted");
        if accounted {
            state
                .releases
                .entry((credential_id.clone(), model.clone()))
                .or_default();
        }
        state.active.insert(
            id,
            Active {
                credential_id,
                model,
                accounted,
                request_id,
                request_kind,
                started_at: Utc::now(),
            },
        );
        ExecutionGuard {
            registry: Arc::clone(self),
            id: Some(id),
            cancellation: self.lifetime_cancel.child_token(),
        }
    }

    /// Track a dispatch command until its result is either guarded or known not to be admitted.
    /// Ambiguous post-send failures must fence the membership, not silently retry dispatch.
    pub fn admission(self: &Arc<Self>) -> AdmissionGuard {
        self.state().admissions += 1;
        AdmissionGuard {
            registry: Arc::clone(self),
            pending: true,
        }
    }

    pub fn pending_admissions(&self) -> usize {
        self.state().admissions
    }

    pub fn pending_releases(&self) -> Vec<ReleaseFrame> {
        self.state()
            .releases
            .iter()
            .filter(|&((_credential_id, _model), ledger)| ledger.completed > ledger.acknowledged)
            .map(|((credential_id, model), ledger)| ReleaseFrame {
                credential_id: credential_id.clone(),
                model: model.clone(),
                release_seq: ledger.completed,
            })
            .collect()
    }

    /// Call only after a successful Home reply, on the registry that produced this frame.
    /// A late ACK for N never clears a subsequently completed N+1.
    pub fn acknowledge(&self, frame: &ReleaseFrame) {
        let mut state = self.state();
        if let Some(ledger) = state
            .releases
            .get_mut(&(frame.credential_id.clone(), frame.model.clone()))
            && frame.release_seq > ledger.acknowledged
            && frame.release_seq <= ledger.completed
        {
            ledger.acknowledged = frame.release_seq;
        }
    }

    pub fn is_drained(&self) -> bool {
        let state = self.state();
        !state.release_exhausted
            && state.active.is_empty()
            && state.admissions == 0
            && state
                .releases
                .values()
                .all(|ledger| ledger.completed == ledger.acknowledged)
    }

    pub fn active_count(&self) -> usize {
        self.state().active.len()
    }

    fn complete(&self, id: u64) -> Option<ReleaseFrame> {
        let mut state = self.state();
        let active = state.active.remove(&id)?;
        if !active.accounted {
            return None;
        }
        let key = (active.credential_id, active.model);
        let ledger = state.releases.entry(key.clone()).or_default();
        // Home uses signed int64; exhausting it requires retiring this membership.
        let Some(next) = ledger.completed.checked_add(1) else {
            state.release_exhausted = true;
            self.lifetime_cancel.cancel();
            return None;
        };
        ledger.completed = next;
        Some(ReleaseFrame {
            credential_id: key.0,
            model: key.1,
            release_seq: next,
        })
    }
}

#[must_use = "keep the guard alive until upstream execution and streaming have stopped"]
pub struct ExecutionGuard {
    registry: Arc<Registry>,
    id: Option<u64>,
    cancellation: CancellationToken,
}

impl ExecutionGuard {
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub fn finish(&mut self) -> Option<ReleaseFrame> {
        let frame = self.id.take().and_then(|id| self.registry.complete(id));
        self.cancellation.cancel();
        frame
    }
}

impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        self.finish();
    }
}

#[must_use = "hold until the dispatch result is resolved"]
pub struct AdmissionGuard {
    registry: Arc<Registry>,
    pending: bool,
}

impl AdmissionGuard {
    /// Establish active ownership before clearing pending admission, without a drain gap.
    pub fn begin(
        mut self,
        credential_id: String,
        model: String,
        accounted: bool,
        request_id: String,
        request_kind: String,
    ) -> ExecutionGuard {
        let guard = self
            .registry
            .begin(credential_id, model, accounted, request_id, request_kind);
        self.finish();
        guard
    }

    /// Explicitly resolve a known no-admission result. An unresolved Drop fences the lifetime.
    pub fn finish(&mut self) {
        if self.pending {
            self.registry.state().admissions -= 1;
            self.pending = false;
        }
    }
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        if self.pending {
            // A canceled dispatch future may already have acquired a remote slot.
            self.registry.lifetime_cancel.cancel();
        }
        self.finish();
    }
}

#[cfg(test)]
#[path = "execution/tests.rs"]
mod tests;
