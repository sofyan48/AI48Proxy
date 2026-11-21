//! Transport-only recovery. A lost epoch is never replayed or reselected.
use super::{ws_codex, ws_failure, ws_steering};
use serde_json::Value;
use std::{error::Error, fmt, io};

#[derive(Debug)]
pub(super) struct Lost;
impl fmt::Display for Lost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("upstream transport lost")
    }
}
impl Error for Lost {}

/// Match Go's EOF/reset/broken-pipe classification, not policy/protocol errors.
pub(super) fn transport(error: axum::Error) -> anyhow::Error {
    let inner = error.into_inner();
    let root: &(dyn Error + 'static) = inner.as_ref();
    // The adapter boxes anyhow's ErrorImpl, hiding yawc's concrete EOF type.
    // Only its exact static sentinel is accepted; never match arbitrary text.
    if root.source().is_none()
        && root.to_string() == yawc::WebSocketError::ConnectionClosed.to_string()
    {
        return Lost.into();
    }
    let mut source: Option<&(dyn Error + 'static)> = Some(root);
    while let Some(cause) = source {
        if let Some(wire) = cause.downcast_ref::<yawc::WebSocketError>() {
            if matches!(wire, yawc::WebSocketError::ConnectionClosed) {
                return Lost.into();
            }
        }
        if let Some(io) = cause.downcast_ref::<io::Error>() {
            if matches!(
                io.kind(),
                io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::NotConnected
            ) {
                return Lost.into();
            }
        }
        source = cause.source();
    }
    anyhow::anyhow!(inner)
}

pub(super) fn continuation(event_id: &str) -> Value {
    let mut event = serde_json::json!({"type":"error", "status":409,
        "error":{"type":"invalid_request_error", "code":"previous_response_not_found",
        "param":"previous_response_id", "message":"Upstream history was lost; send response.create with full input"}});
    if !event_id.is_empty() {
        event["error"]["event_id"] = event_id.into();
    }
    event
}

/// Taking the ledger settles every reporter once before a new epoch can start.
pub(super) fn abandon<T>(
    owners: &mut ws_codex::Owners<T>,
    steering: &mut ws_steering::Steering,
    consumed: Option<&str>,
) -> Vec<Value> {
    let mut losses = Vec::new();
    for owner in owners.active.values().chain(owners.pending.iter()) {
        losses.push(ws_failure::lost(owner.event_id(), false));
    }
    if let Some(id) = consumed {
        losses.push(ws_failure::lost(id, true));
    }
    let (sent, queued) = steering.abandoned();
    losses.extend(sent.iter().map(|id| ws_failure::lost(id, false)));
    losses.extend(queued.iter().map(|id| ws_failure::lost(id, true)));
    *owners = ws_codex::Owners::default();
    *steering = ws_steering::Steering::new(steering.enabled);
    losses
}

#[cfg(test)]
#[path = "ws_recovery_home_tests.rs"]
mod home_tests;
#[cfg(test)]
#[path = "ws_recovery_tests.rs"]
mod tests;
