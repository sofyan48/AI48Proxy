//! Bounded, session-local replay state for native Responses WebSockets.
//! Replay is advisory input state only: it never triggers redispatch or credential rotation.
use serde_json::Value;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const MAX_ENTRIES: usize = 32;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
pub(super) struct ReplayCache {
    entries: VecDeque<Entry>,
    bytes: usize,
}
struct Entry {
    response_id: String,
    request: Value,
    response: Value,
    size: usize,
    committed: Instant,
}
impl ReplayCache {
    pub fn commit(&mut self, response_id: String, request: Value, response: Value) {
        self.invalidate_expired();
        let size = serde_json::to_vec(&request)
            .map_or(usize::MAX, |v| v.len())
            .saturating_add(serde_json::to_vec(&response).map_or(usize::MAX, |v| v.len()));
        if size > MAX_BYTES {
            return;
        }
        self.entries
            .retain(|entry| entry.response_id != response_id);
        self.bytes = self.bytes.saturating_sub(
            self.entries
                .iter()
                .filter(|entry| entry.response_id == response_id)
                .map(|entry| entry.size)
                .sum(),
        );
        self.entries.push_back(Entry {
            response_id,
            request,
            response,
            size,
            committed: Instant::now(),
        });
        self.bytes = self.bytes.saturating_add(size);
        while self.entries.len() > MAX_ENTRIES || self.bytes > MAX_BYTES {
            if let Some(entry) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(entry.size);
            } else {
                break;
            }
        }
    }
    pub fn invalidate(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
    pub fn invalidate_expired(&mut self) {
        let now = Instant::now();
        self.entries
            .retain(|entry| now.duration_since(entry.committed) <= TTL);
        self.bytes = self.entries.iter().map(|entry| entry.size).sum();
    }
    pub fn get(&mut self, response_id: &str) -> Option<(&Value, &Value)> {
        self.invalidate_expired();
        self.entries
            .iter()
            .find(|entry| entry.response_id == response_id)
            .map(|entry| (&entry.request, &entry.response))
    }
    #[cfg(test)]
    fn len(&mut self) -> usize {
        self.invalidate_expired();
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn only_committed_entries_are_visible_and_invalidation_is_total() {
        let mut cache = ReplayCache::default();
        assert!(cache.get("r1").is_none());
        cache.commit("r1".into(), json!({"input":[1]}), json!({"output":[2]}));
        assert!(cache.get("r1").is_some());
        cache.invalidate();
        assert_eq!(cache.len(), 0);
    }
    #[test]
    fn cache_is_bounded() {
        let mut cache = ReplayCache::default();
        for i in 0..64 {
            cache.commit(format!("r{i}"), json!({"i":i}), json!({"i":i}));
        }
        assert!(cache.len() <= MAX_ENTRIES);
    }
}
