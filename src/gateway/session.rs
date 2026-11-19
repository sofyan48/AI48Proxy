//! Home owns affinity and pin authorization; never create a local credential binding.
use axum::http::HeaderMap;
use serde_json::{Value, json};

pub(super) fn dispatch_fields(headers: &HeaderMap, body: &Value, request: &mut Value) {
    for (field, names) in [
        (
            "session_id",
            &[
                "x-session-id",
                "session_id",
                "session-id",
                "x-slot-session-id",
                "x-http-session-id",
                "x-claude-code-session-id",
                "conversation-id",
                "x-conversation-id",
            ][..],
        ),
        (
            "parent_session_id",
            &["x-parent-session-id", "x-codex-parent-session-id"][..],
        ),
    ] {
        let value = names
            .iter()
            .find_map(|name| {
                headers
                    .get(*name)
                    .and_then(|v| v.to_str().ok())
                    .and_then(valid)
            })
            .or_else(|| body.get(field).and_then(Value::as_str).and_then(valid));
        if let Some(value) = value {
            request[field] = json!(value);
        }
    }
    // Caller-supplied auth IDs are not trusted routing policy. Affinity and any
    // pinned_auth_id must originate from Home/internal execution, not JSON/headers.
}
fn valid(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty() && trimmed.len() <= 256 && !value.chars().any(char::is_control))
        .then_some(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn header_precedence_and_no_caller_credential_pinning() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "header-session".parse().unwrap());
        let body = json!({"session_id":"body-session","parent_session_id":"parent","pinned_auth_id":"attacker"});
        let mut request = json!({});
        dispatch_fields(&headers, &body, &mut request);
        assert_eq!(
            request,
            json!({"session_id":"header-session","parent_session_id":"parent"})
        );
        assert!(valid("a\nb").is_none());
        assert!(valid(&"a".repeat(257)).is_none());
    }
}
