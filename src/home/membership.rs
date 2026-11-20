//! Membership identity and recovery policy survive individual transport lifetimes.
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
pub struct Membership {
    pub(super) managed: AtomicBool,
    pub(super) pending_result: Mutex<Option<(String, super::cluster::Outcome)>>,
    eligible: AtomicBool,
    legacy: AtomicBool,
    timeout: AtomicBool,
    migration: Mutex<Option<super::cluster::Instruction>>,
    pub(super) usage_owners: Mutex<std::collections::BTreeMap<String, super::transport::Connector>>,
    pub(super) latency: Mutex<super::client::latency::History>,
}
impl Membership {
    pub(super) fn managed() -> Self {
        Self {
            managed: AtomicBool::new(true),
            ..Self::default()
        }
    }
    pub fn mark_eligible(&self) {
        if !self.legacy() {
            self.eligible.store(true, Ordering::Release);
        }
    }
    pub fn suppress_takeover(&self) {
        self.eligible.store(false, Ordering::Release);
    }
    pub fn takeover(&self) -> bool {
        self.eligible.load(Ordering::Acquire) && !self.legacy()
    }
    pub fn legacy(&self) -> bool {
        self.legacy.load(Ordering::Acquire)
    }
    pub fn subscription(&self, revision: i64, instance: &str) -> Vec<Vec<u8>> {
        let mut args = vec![b"SUBSCRIBE".to_vec(), b"config".to_vec()];
        if revision > 0 {
            args.push(revision.to_string().into_bytes());
            if !self.legacy() {
                if self.takeover() {
                    args.push(b"takeover".to_vec());
                }
                args.push(instance.as_bytes().to_vec());
            }
        }
        args
    }
    /// Match only known protocol errors; never expose arbitrary RESP text.
    pub fn rejection(&self, error: &str) {
        let error = error.trim().to_ascii_lowercase();
        let error = error.strip_prefix("err ").unwrap_or(&error);
        if error == "wrong number of arguments for 'subscribe' command" {
            self.legacy.store(true, Ordering::Release);
            self.suppress_takeover();
        } else if error == "membership_takeover_unavailable" {
            self.suppress_takeover();
        }
    }
    pub fn mark_timeout(&self) {
        self.timeout.store(true, Ordering::Release);
    }
    pub fn take_timeout(&self) -> bool {
        self.timeout.swap(false, Ordering::AcqRel)
    }
    pub fn migrate(&self, instruction: super::cluster::Instruction) {
        *self.migration.lock().unwrap_or_else(|e| e.into_inner()) = Some(instruction);
    }
    pub fn take_migration(&self) -> Option<super::cluster::Instruction> {
        self.migration
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn acknowledgement_eligibility_suppression_and_permanent_legacy() {
        let state = Membership::default();
        assert_eq!(state.subscription(7, "id").len(), 4);
        state.mark_eligible();
        assert_eq!(state.subscription(7, "id")[3], b"takeover");
        state.rejection("ERR membership_takeover_unavailable");
        assert!(!state.takeover());
        state.mark_eligible();
        state.rejection("ERR wrong number of arguments for 'subscribe' command");
        state.mark_eligible();
        assert!(state.legacy());
        assert!(!state.takeover());
        assert_eq!(state.subscription(7, "id").len(), 3);
        assert_eq!(state.subscription(0, "id").len(), 2);
    }
}
