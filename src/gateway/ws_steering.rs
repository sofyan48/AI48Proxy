//! Full-duplex Codex steering bookkeeping (Go codex_websockets_duplex.go).
//!
//! One socket, one Home selection: steering frames and automatic successor
//! responses never redispatch, redial, or replay on another credential. Usage
//! is still reported once per upstream response by the caller's `Turn`.
use crate::{config::Config, home::client::Selection, payload::ApplyContext};
use anyhow::{Result, ensure};
use axum::http::HeaderMap;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};

/// Go bounds outstanding `response.create` frames at 16 per duplex socket.
pub(super) const MAX_QUEUED: usize = 16;

#[derive(Default)]
pub(super) struct Steering {
    pub enabled: bool,
    /// (parent, client event_id) of forwarded `response.steer` frames awaiting an ack.
    unacknowledged: Vec<(String, String)>,
    /// Accepted steer id -> (parent, successor response id once created, client event_id).
    accepted: HashMap<String, (String, String, String)>,
    /// Parent that upstream reported as waiting for client input (tool results).
    waiting_parent: String,
    /// Most recent `response.created` id; Go's fallback parent for id-less acks.
    last_response: String,
    queue: VecDeque<Value>,
}

fn text<'a>(event: &'a Value, pointer: &str) -> &'a str {
    event.pointer(pointer).and_then(Value::as_str).unwrap_or("")
}

impl Steering {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Default::default()
        }
    }

    pub fn submitted(&mut self, parent: &str, event_id: &str) {
        self.unacknowledged
            .push((parent.to_owned(), event_id.to_owned()));
    }

    pub fn has_unacknowledged(&self) -> bool {
        !self.unacknowledged.is_empty()
    }

    /// Nothing outstanding: no unacked or accepted steer and no queued create.
    pub fn settled(&self) -> bool {
        self.unacknowledged.is_empty() && self.accepted.is_empty() && self.queue.is_empty()
    }

    /// Go loss order: unacknowledged steers, accepted steers sorted by steer id,
    /// then queued (never sent) creates. Operations without an event_id are omitted.
    pub fn abandoned(&self) -> (Vec<String>, Vec<String>) {
        let mut sent: Vec<String> = self
            .unacknowledged
            .iter()
            .map(|(_, event)| event.clone())
            .collect();
        let mut ids: Vec<&String> = self.accepted.keys().collect();
        ids.sort();
        sent.extend(ids.into_iter().map(|id| self.accepted[id].2.clone()));
        sent.retain(|event| !event.is_empty());
        let queued = self
            .queue
            .iter()
            .filter_map(|frame| frame.get("event_id").and_then(Value::as_str))
            .filter(|event| !event.is_empty())
            .map(str::to_owned)
            .collect();
        (sent, queued)
    }

    /// Track an upstream `response.steer.*` event. The event itself is opaque
    /// and must be forwarded byte-for-byte by the caller.
    pub fn observe(&mut self, event: &Value) {
        let id = text(event, "/steer/id").to_owned();
        let mut parent = text(event, "/steer/previous_response_id").to_owned();
        if parent.is_empty() {
            parent = self.last_response.clone();
        }
        match text(event, "/type") {
            "response.steer.accepted" => {
                let event_id = self.consume(&parent);
                self.accepted.insert(id, (parent, String::new(), event_id));
            }
            "response.steer.failed" => {
                if self.accepted.remove(&id).is_none() {
                    self.consume(&parent);
                }
            }
            "response.steer.pending" => self.waiting_parent = parent,
            _ => {}
        }
    }

    fn consume(&mut self, parent: &str) -> String {
        self.unacknowledged
            .iter()
            .position(|(p, _)| p == parent || p.is_empty())
            .map(|index| self.unacknowledged.remove(index).1)
            .unwrap_or_default()
    }

    /// Record `response.created`. Without an explicit create in flight the
    /// response is an automatic successor and must consume an accepted steer.
    pub fn created(&mut self, event: &Value, explicit: bool) -> Result<()> {
        let id = text(event, "/response/id");
        ensure!(!id.is_empty(), "response id missing");
        let mut parent = text(event, "/response/previous_response_id");
        if parent.is_empty() {
            parent = self.last_response.as_str();
        }
        let parent = parent.to_owned();
        let mut owned = false;
        for (steer_parent, successor, _) in self.accepted.values_mut() {
            if *steer_parent == parent && successor.is_empty() {
                *successor = id.to_owned();
                owned = true;
            }
        }
        ensure!(
            explicit || owned,
            "automatic successor has no accepted steer"
        );
        self.waiting_parent.clear();
        self.last_response = id.to_owned();
        Ok(())
    }

    /// A steer is complete once the response that consumed it has ended.
    pub fn ended(&mut self, id: &str) {
        if !id.is_empty() {
            self.accepted.retain(|_, (_, successor, _)| successor != id);
        }
    }

    /// Go readyForCreate: creates pipeline behind a running explicit response,
    /// but never behind an unacked steer or an automatic successor.
    pub fn ready(&self, automatic_active: bool) -> bool {
        !automatic_active
            && self.unacknowledged.is_empty()
            && self.accepted.values().all(|(parent, successor, _)| {
                !successor.is_empty() || *parent == self.waiting_parent
            })
    }

    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }

    /// Go wrongParent: while an accepted steer awaits input, only a create that
    /// continues the waiting parent may start.
    pub fn create_parent_ok(&self, previous: &str) -> bool {
        self.accepted
            .values()
            .all(|(_, successor, _)| !successor.is_empty() || previous == self.waiting_parent)
    }

    pub fn enqueue(&mut self, frame: Value) -> Result<()> {
        ensure!(
            self.queue.len() < MAX_QUEUED,
            "too many outstanding response.create requests"
        );
        self.queue.push_back(frame);
        Ok(())
    }

    pub fn pop_ready(&mut self, automatic_active: bool) -> Option<Value> {
        if self.enabled && self.ready(automatic_active) {
            self.queue.pop_front()
        } else {
            None
        }
    }
}

/// Steering frames bypass response.create built-ins (Go: control frames keep
/// unknown fields for upstream validation) but still pass the final payload
/// rules exactly once. Returns the encoded frame and its parent response id.
pub(super) fn prepare(
    config: &Config,
    selection: &Selection,
    headers: &HeaderMap,
    alias: &str,
    frame: Value,
    limit: usize,
) -> Result<(String, String)> {
    ensure!(
        frame.get("type").and_then(Value::as_str) == Some("response.steer"),
        "expected response.steer"
    );
    let mut body = frame.clone();
    config.payload.apply_context(
        &mut body,
        ApplyContext {
            model: &selection.model,
            protocol: "codex",
            from_protocol: "openai-response",
            headers: Some(headers),
            requested_model: alias,
            original: Some(&frame),
            root: "",
        },
    )?;
    let object = body
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("final steer payload must be an object"))?;
    ensure!(
        object
            .get("type")
            .is_none_or(|v| v.as_str() == Some("response.steer")),
        "conflicting websocket event type"
    );
    object.insert("type".into(), "response.steer".into());
    let parent = match object.get("previous_response_id") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(id)) => id.clone(),
        _ => anyhow::bail!("invalid previous_response_id"),
    };
    let message = serde_json::to_string(&body)?;
    ensure!(message.len() <= limit, "steer payload exceeds limit");
    Ok((message, parent))
}

#[cfg(test)]
#[path = "ws_steering_tests.rs"]
mod tests;
