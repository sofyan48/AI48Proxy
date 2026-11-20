//! Go's standard four-signal native detection (not markerless helper profiles).
use http::HeaderMap;
use serde_json::Value;

pub(super) fn confirmed(body: &Value, headers: &HeaderMap) -> bool {
    let header = |name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    };
    if !matches!(header("x-app"), "cli" | "cli-bg") || !native_user_agent(header("user-agent")) {
        return false;
    }
    if !headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|b| b.trim() == "claude-code-20250219")
    {
        return false;
    }
    let Some(identity) = body
        .pointer("/metadata/user_id")
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
    else {
        return false;
    };
    let device = identity["device_id"].as_str().unwrap_or("");
    let account = identity["account_uuid"].as_str();
    device.len() == 64
        && device
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && identity["session_id"]
            .as_str()
            .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
        && account.is_none_or(|s| s.is_empty() || uuid::Uuid::parse_str(s).is_ok())
        && identity.get("account_uuid").is_none_or(Value::is_string)
}

fn version(value: &str) -> Option<[u64; 3]> {
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts
            .iter()
            .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    Some([
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ])
}

fn native_user_agent(value: &str) -> bool {
    if value
        .chars()
        .any(|c| (c < ' ' && c != '\t') || c == '\u{7f}')
    {
        return false;
    }
    let value = value.trim();
    let Some((prefix, comment)) = value.split_once('(') else {
        return false;
    };
    if prefix == prefix.trim() {
        return false;
    }
    let Some(v) = prefix.trim().strip_prefix("claude-cli/").and_then(version) else {
        return false;
    };
    if v[0] != 2 || v[1] != 1 || v[2] < 321 {
        return false;
    }
    let Some(comment) = comment.strip_suffix(')') else {
        return false;
    };
    if comment.contains(['(', ')']) {
        return false;
    }
    let mut fields = comment.split(',');
    if !fields
        .next()
        .is_some_and(|s| s.eq_ignore_ascii_case("external"))
    {
        return false;
    }
    if !fields.next().is_some_and(|s| {
        matches!(
            s.trim().to_lowercase().as_str(),
            "cli" | "sdk-cli" | "sdk-ts" | "claude-vscode"
        )
    }) {
        return false;
    }
    let mut seen = std::collections::HashSet::new();
    for field in fields {
        let Some((name, value)) = field.trim().split_once('/') else {
            return false;
        };
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
            || value.trim().is_empty()
        {
            return false;
        }
        let name = name.to_lowercase();
        if !seen.insert(name.clone()) || (name == "agent-sdk" && version(value).is_none()) {
            return false;
        }
    }
    true
}
