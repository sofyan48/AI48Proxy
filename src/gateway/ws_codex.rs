//! Codex multi-agent v2 / orphan delegation on the native WS path (Go
//! openai_responses_websocket.go + codex_websockets_stream.go). Rewrites run
//! before `ws_transport::prepare`, whose payload rules stay the final barrier.
use super::super::codex::multiagent;
use super::ws_transport;
use crate::{config::Config, home::client::Selection};
use anyhow::Result;
use axum::http::HeaderMap;
use serde_json::Value;

/// Per-request multi-agent state (Go optimizeMultiAgentV2 / multiAgentV2Conflict).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Flags {
    pub optimized: bool,
    pub conflict: bool,
}

/// Handler boundary: applied to every hydrated request before dispatch/attempt.
#[cfg(test)]
pub(super) fn boundary(config: &Config, headers: &HeaderMap, body: &mut Value) {
    multiagent::prepare_boundary(config, "/v1/responses", headers, body);
}

/// Executor stage: conflict is detected before the optimizer runs, as in Go.
pub(super) fn attempt(
    config: &Config,
    selection: &Selection,
    headers: &HeaderMap,
    mut body: Value,
) -> Result<(ws_transport::Prepared, Flags)> {
    let conflict = multiagent::has_conflict(&body);
    let optimized =
        multiagent::prepare_attempt(config, selection, "/v1/responses", headers, &mut body);
    let prepared = ws_transport::prepare(config, selection, headers, body)?;
    Ok((
        prepared,
        Flags {
            optimized,
            conflict,
        },
    ))
}

/// Go setMultiAgentV2Optimized: only a request that optimized or conflicted
/// changes the connection's state.
pub(super) fn connection_state(current: bool, flags: Flags) -> bool {
    if flags.optimized || flags.conflict {
        flags.optimized && !flags.conflict
    } else {
        current
    }
}

/// Go RestoreCodexMultiAgentV2Response, gated per event owner and connection.
pub(super) fn restore(text: String, flags: Flags, connection: bool) -> String {
    if flags.conflict || !(flags.optimized || connection) {
        return text;
    }
    multiagent::restore_payload(text.as_bytes())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or(text)
}

/// Settings and accounting follow the request, never whichever response happened
/// to be most recently created on the connection.
pub(super) struct Owner<T> {
    pub request: Value,
    pub flags: Flags,
    pub automatic: bool,
    pub turn: T,
}

impl<T> Owner<T> {
    pub fn event_id(&self) -> &str {
        self.request
            .get("event_id")
            .and_then(Value::as_str)
            .unwrap_or("")
    }
}

pub(super) struct Owners<T> {
    pub pending: std::collections::VecDeque<Owner<T>>,
    pub active: std::collections::BTreeMap<String, Owner<T>>,
}

impl<T> Default for Owners<T> {
    fn default() -> Self {
        Self {
            pending: Default::default(),
            active: Default::default(),
        }
    }
}

impl<T> Owners<T> {
    pub fn automatic_active(&self) -> bool {
        self.active.values().any(|owner| owner.automatic)
    }

    pub fn outstanding(&self) -> usize {
        self.pending.len() + self.active.len()
    }

    /// An id-less failure cannot choose between a running response, a pending
    /// create, or an unacknowledged steer. Never guess or retry that input.
    pub fn failure_owner(&self, event: &Value, unacknowledged: bool) -> Result<Option<String>> {
        let id = event
            .pointer("/response/id")
            .or_else(|| event.get("response_id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        if let Some(id) = id {
            return Ok(self.active.contains_key(id).then(|| id.to_owned()));
        }
        anyhow::ensure!(
            !unacknowledged
                && !(self.active.len() > 1
                    || (!self.pending.is_empty() && !self.active.is_empty())),
            "cannot associate websocket failure with an operation"
        );
        Ok(self.active.keys().next().cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> Value {
        json!({"model":"m","input":[],"tools":[{"type":"namespace","name":"collaboration","tools":[
            {"type":"function","name":"spawn_agent","parameters":{"properties":{"message":{"type":"string","encrypted":true}}}}]}]})
    }

    fn selection() -> Selection {
        serde_json::from_value(json!({"model":"m","provider":"codex","auth_index":"idx","user_api_key":"user",
            "auth":{"id":"a","attributes":{"api_key":"upstream","base_url":"http://127.0.0.1:9","websockets":true}}}))
        .unwrap()
    }

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", "codex_cli_rs/1.0".parse().unwrap());
        headers.insert("x-openai-subagent", "collab_spawn".parse().unwrap());
        headers
    }

    #[test]
    fn rewrite_precedes_final_payload_rules() {
        let config = Config::parse(br#"{"credential-concurrency":{"lifecycle-config-revision":1},
            "client":{"codex":{"optimize-multi-agent-v2":true}},"codex":{"orphan-delegation-compatibility":true},
            "payload":{"override":[{"models":[{"name":"*","protocol":"codex"}],"params":{"custom_final":1}}]}}"#).unwrap();
        let mut body = request();
        boundary(&config, &headers(), &mut body);
        let (prepared, flags) = attempt(&config, &selection(), &headers(), body).unwrap();
        assert_eq!(
            flags,
            Flags {
                optimized: true,
                conflict: false
            }
        );
        let sent: Value = serde_json::from_str(&prepared.message).unwrap();
        assert_eq!(sent["tools"][0]["name"], "collaboration-optimize");
        assert!(
            sent.pointer("/tools/0/tools/0/parameters/properties/message/encrypted")
                .is_none()
        );
        assert_eq!(sent["custom_final"], 1);
        assert_eq!(sent["type"], "response.create");

        // A rule may still rewrite the optimized namespace; nothing runs after it.
        let config = Config::parse(br#"{"credential-concurrency":{"lifecycle-config-revision":1},
            "client":{"codex":{"optimize-multi-agent-v2":true}},
            "payload":{"override":[{"models":[{"name":"*","protocol":"codex"}],"params":{"tools":[]}}]}}"#).unwrap();
        let (prepared, _) = attempt(&config, &selection(), &headers(), request()).unwrap();
        let sent: Value = serde_json::from_str(&prepared.message).unwrap();
        assert_eq!(sent["tools"], json!([]));
    }

    #[test]
    fn conflict_and_connection_state_gate_restore() {
        let config = Config::parse(
            br#"{"credential-concurrency":{"lifecycle-config-revision":1},
            "client":{"codex":{"optimize-multi-agent-v2":true}}}"#,
        )
        .unwrap();
        let mut body = request();
        body["tools"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"function","name":"collaboration-optimize__x"}));
        let (_, flags) = attempt(&config, &selection(), &headers(), body).unwrap();
        assert!(flags.conflict && !flags.optimized);
        let event = json!({"type":"response.output_item.done","item":{"type":"function_call",
            "namespace":"collaboration-optimize","name":"spawn_agent","arguments":"{}"}})
        .to_string();
        let restored: Value =
            serde_json::from_str(&restore(event.clone(), Flags::default(), true)).unwrap();
        assert_eq!(restored["item"]["namespace"], "collaboration");
        assert_eq!(restore(event.clone(), flags, true), event);
        assert_eq!(restore(event.clone(), Flags::default(), false), event);
        assert!(!connection_state(true, flags));
        assert!(connection_state(true, Flags::default()));
        assert!(connection_state(
            false,
            Flags {
                optimized: true,
                conflict: false
            }
        ));
    }
}
