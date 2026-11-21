//! Codex WS failure classification (Go codex_websockets_errors.go and
//! codex_executor_terminal.go). Only statuses and canonical error bodies are
//! derived; upstream text is never logged.
use serde_json::{Value, json};

fn integer(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}

fn at(value: &Value, path: &str) -> i64 {
    value.pointer(path).and_then(integer).unwrap_or(0)
}

fn lower(value: &Value, path: &str) -> String {
    value
        .pointer(path)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn kind(event: &Value) -> &str {
    event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
}

fn has_headers(event: &Value) -> bool {
    [
        "/headers",
        "/body/headers",
        "/error/headers",
        "/body/error/headers",
        "/response/error/headers",
    ]
    .iter()
    .any(|p| event.pointer(p).and_then(Value::as_object).is_some())
}

fn connection_limit(event: &Value) -> bool {
    [
        "/error/code",
        "/error/type",
        "/body/error/code",
        "/body/error/type",
        "/code",
        "/error",
    ]
    .iter()
    .any(|p| {
        event.pointer(p).and_then(Value::as_str).map(str::trim)
            == Some("websocket_connection_limit_reached")
    })
}

fn usage_limit(body: &Value) -> bool {
    ["/error/type", "/type", "/body/error/type"]
        .iter()
        .any(|p| lower(body, p) == "usage_limit_reached")
}

fn capacity(body: &Value) -> bool {
    let code = lower(body, "/error/code");
    if !matches!(
        code.as_str(),
        "" | "turn_rate_limit_exceeded" | "model_at_capacity" | "model_is_at_capacity"
    ) {
        return false;
    }
    let raw = body.to_string().to_ascii_lowercase();
    code == "turn_rate_limit_exceeded"
        || raw.contains("model_at_capacity")
        || raw.contains("model_is_at_capacity")
        || (raw.contains("model") && raw.contains("at capacity"))
}

fn context_length(body: &Value) -> bool {
    lower(body, "/error/code") == "context_length_exceeded"
}

/// Go codexTerminalFailureBody: the error object of an `error`/`response.failed` event.
pub(super) fn body(event: &Value) -> Value {
    let paths: &[&str] = if kind(event) == "response.failed" {
        &["/response/error", "/error", "/body/error"]
    } else {
        &["/error", "/body/error", "/response/error"]
    };
    let mut body = paths.iter().find_map(|p| match event.pointer(p) {
        Some(Value::Object(error)) => Some(json!({"error": error})),
        Some(Value::String(message)) if !message.trim().is_empty() => {
            Some(json!({"error":{"message": message.trim()}}))
        }
        _ => None,
    });
    if body.is_none() && kind(event) == "error" {
        let mut error = serde_json::Map::new();
        for (from, to) in [
            ("message", "message"),
            ("code", "code"),
            ("error_type", "type"),
            ("param", "param"),
        ] {
            if let Some(text) = event
                .get(from)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                error.insert(to.into(), text.trim().into());
            }
        }
        if !error.is_empty() {
            body = Some(json!({ "error": error }));
        }
    }
    let mut body = body.unwrap_or_else(
        || json!({"error":{"message":"upstream stream failed without error details"}}),
    );
    if let Some(status) = [
        "/status_code",
        "/status",
        "/body/status_code",
        "/body/status",
    ]
    .iter()
    .map(|p| at(event, p))
    .find(|s| (400..=599).contains(s))
    {
        body["status"] = status.into();
    }
    body
}

/// Go codexTerminalFailureStatus.
fn terminal_status(body: &Value) -> u16 {
    if let Some(status) = [
        "/error/status_code",
        "/error/status",
        "/status_code",
        "/status",
    ]
    .iter()
    .map(|p| at(body, p))
    .find(|s| (400..=599).contains(s))
    {
        return status as u16;
    }
    let kind = lower(body, "/error/type");
    let code = lower(body, "/error/code");
    match (kind.as_str(), code.as_str()) {
        (
            _,
            "cyber_policy"
            | "cyber_policy_violation"
            | "bio_policy"
            | "misalignment_policy_violation"
            | "invalid_prompt"
            | "content_policy_violation"
            | "image_content_policy_violation",
        ) => 400,
        ("not_found_error", _) | (_, "not_found" | "model_not_found") => 404,
        ("authentication_error", _) | (_, "invalid_api_key" | "unauthorized") => 401,
        ("permission_error", _) | (_, "forbidden" | "permission_denied") => 403,
        ("rate_limit_error", _) | (_, "rate_limit_exceeded") => 429,
        ("invalid_request_error" | "bad_request_error" | "image_generation_user_error", _)
        | (_, "invalid_value" | "invalid_request_error") => 400,
        _ => 502,
    }
}

/// HTTP-equivalent status of an upstream `error` or `response.failed` event.
pub(super) fn status(event: &Value) -> u16 {
    if kind(event) == "error" {
        let mut status = ["/status", "/status_code"]
            .iter()
            .map(|p| at(event, p))
            .find(|s| *s > 0)
            .or_else(|| {
                [
                    "/body/status",
                    "/body/status_code",
                    "/error/status",
                    "/error/status_code",
                    "/body/error/status",
                    "/body/error/status_code",
                    "/response/error/status",
                    "/response/error/status_code",
                ]
                .iter()
                .map(|p| at(event, p))
                .find(|s| (400..=599).contains(s))
            })
            .unwrap_or(0);
        if status == 0 && has_headers(event) {
            status = infer(event);
        }
        if status > 0 {
            return status.clamp(0, u16::MAX as i64) as u16;
        }
    }
    let body = body(event);
    if usage_limit(&body) || capacity(&body) {
        429
    } else if context_length(&body) {
        400
    } else {
        terminal_status(&body)
    }
}

fn infer(event: &Value) -> i64 {
    if connection_limit(event) {
        return 429;
    }
    let body = body(event);
    if usage_limit(&body) || capacity(&body) {
        return 429;
    }
    if context_length(&body) {
        return 400;
    }
    if lower(&body, "/error/type") == "service_unavailable_error"
        || lower(&body, "/error/code") == "server_is_overloaded"
    {
        return 503;
    }
    terminal_status(&body) as i64
}

/// Go: account health failures end the duplex socket regardless of which request failed.
pub(super) fn credential(status: u16) -> bool {
    matches!(status, 401 | 403 | 429)
}

/// Never-sent creates are safe to resubmit; sent operations have unknown outcomes.
pub(super) fn lost(event_id: &str, not_sent: bool) -> Value {
    let mut event = json!({"type":"error", "status": if not_sent { 409 } else { 502 },
        "error":{"type":"server_error", "code": if not_sent { "upstream_websocket_not_sent" } else { "upstream_websocket_lost" },
        "message": if not_sent { "Request was not sent upstream; resend with full input" }
            else { "Upstream WebSocket lost; request outcome is unknown; do not automatically retry" }}});
    if !event_id.is_empty() {
        event["error"]["event_id"] = event_id.into();
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_credential_and_business_failures_like_go() {
        for (event, expected) in [
            (
                json!({"type":"error","status":401,"error":{"message":"x"}}),
                401,
            ),
            (
                json!({"type":"error","error":{"type":"usage_limit_reached"}}),
                429,
            ),
            (
                json!({"type":"error","error":{"code":"websocket_connection_limit_reached"},"headers":{}}),
                429,
            ),
            (
                json!({"type":"error","error":{"type":"permission_error"}}),
                403,
            ),
            (
                json!({"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded"}}}),
                429,
            ),
            (
                json!({"type":"response.failed","response":{"error":{"code":"invalid_prompt"}}}),
                400,
            ),
            (
                json!({"type":"error","error":{"code":"context_length_exceeded"}}),
                400,
            ),
            (json!({"type":"error","message":"boom"}), 502),
        ] {
            assert_eq!(status(&event), expected, "{event}");
        }
        assert!(credential(401) && credential(403) && credential(429) && !credential(400));
        assert_eq!(
            body(&json!({"type":"error","code":"c","message":"m","status":429})),
            json!({"error":{"code":"c","message":"m"},"status":429})
        );
    }
}
