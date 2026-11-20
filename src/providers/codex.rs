//! Codex HTTP Responses subset, derived from codex_executor_{request,execute}.go.
use super::{insert, text};
use anyhow::{Result, ensure};
use http::HeaderMap;
use serde_json::{Value, json};

#[path = "codex_cache.rs"]
pub mod cache;
#[path = "codex_ws.rs"]
mod websocket_impl;
pub mod websocket {
    pub use super::websocket_impl::enabled;

    pub fn prepare(
        config: &crate::config::Config,
        selection: &crate::home::client::Selection,
        incoming: &http::HeaderMap,
        mut body: serde_json::Value,
    ) -> anyhow::Result<reqwest::Request> {
        config.disable_image_generation.codex_tools(
            &mut body,
            &selection.model,
            &selection.auth,
            incoming,
        );
        config.disable_image_generation.filter(&mut body);
        super::websocket_impl::prepare(config, selection, incoming, body)
    }
}

pub(super) fn headers(headers: &mut HeaderMap, auth: &Value, oauth: bool) -> Result<()> {
    insert(
        headers,
        "user-agent",
        "codex-tui/0.154.0 (Mac OS 26.5.2; arm64) iTerm.app/3.6.11 (codex-tui; 0.154.0)",
    )?;
    if oauth {
        insert(headers, "originator", "codex-tui")?;
        let account = text(auth, "/metadata/account_id");
        if !account.is_empty() {
            insert(headers, "chatgpt-account-id", account)?;
        }
    }
    Ok(())
}

pub(super) fn forward_headers(headers: &mut HeaderMap, incoming: &HeaderMap) {
    for name in [
        "version",
        "x-codex-beta-features",
        "x-codex-turn-metadata",
        "x-codex-turn-state",
        "x-client-request-id",
        "x-codex-window-id",
        "thread-id",
        "session-id",
        "x-openai-internal-codex-responses-lite",
    ] {
        if let Some(value) = incoming.get(name) {
            // misc.EnsureHeader ignores blank sources, preserving a seeded session.
            if name == "session-id" {
                if let Ok(value) = value.to_str()
                    && !value.trim().is_empty()
                    && let Ok(value) = http::HeaderValue::from_str(value.trim())
                {
                    headers.insert(name, value);
                }
            } else {
                headers.insert(name, value.clone());
            }
        }
    }
}

/// Explicit Responses keys only: codex_executor_request.go:cacheHelperDoc reads
/// req.Payload before FinalizePayloadDoc and sets Session-Id without inventing an ID.
pub(crate) fn prompt_cache_header(headers: &mut HeaderMap, request: &Value) -> Result<()> {
    if let Some(key) = request.get("prompt_cache_key").and_then(Value::as_str)
        && !key.is_empty()
    {
        insert(headers, "session-id", key)?;
    }
    Ok(())
}

/// Config defaults apply to OAuth only; explicit credential headers win last.
pub(crate) fn request_headers(
    headers: &mut HeaderMap,
    incoming: &HeaderMap,
    config: &crate::config::Config,
    auth: &Value,
    oauth: bool,
) -> Result<()> {
    forward_headers(headers, incoming);
    let configured = if oauth {
        config
            .raw
            .get("codex-header-defaults")
            .and_then(|v| v.get("user-agent"))
            .and_then(serde_yaml::Value::as_str)
            .unwrap_or("")
            .trim()
    } else {
        ""
    };
    if !configured.is_empty() {
        insert(headers, "user-agent", configured)?;
    } else if let Some(agent) = incoming.get("user-agent") {
        headers.insert("user-agent", agent.clone());
    }
    if let Some(originator) = incoming.get("originator") {
        headers.insert("originator", originator.clone());
    }
    insert(headers, "connection", "Keep-Alive")?;
    super::custom_headers(headers, auth)?;
    Ok(())
}

/// Normalize native Responses input *before* user payload rules. Not a Chat translator.
/// HTTP drops previous_response_id like Go; native WS preserves it before payload rules.
/// Hosted-tool policy is applied by the gateway before this normalization.
pub fn normalize_request(body: &mut Value, model: &str) -> Result<()> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Codex request must be an object"))?;
    ensure!(!model.is_empty(), "Codex model missing");
    ensure!(
        !object.contains_key("messages"),
        "Codex requires translated Responses input"
    );

    ensure!(
        !object
            .get("previous_response_id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.trim().starts_with("resp_ag_compact_")),
        "local compaction continuation requires full input"
    );
    ensure!(
        !object.contains_key("context_management"),
        "Codex local compaction unsupported"
    );
    ensure!(
        !object.contains_key("generate"),
        "Codex generate extension unsupported"
    );
    let input = object
        .get("input")
        .ok_or_else(|| anyhow::anyhow!("Codex input missing"))?;
    ensure!(
        input.is_string() || input.is_array(),
        "Codex input must be text or an array"
    );
    if let Some(tools) = object.get("tools") {
        let tools = tools
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Codex tools must be an array"))?;
        ensure!(
            tools.iter().all(Value::is_object),
            "Codex tools must be objects"
        );
    }
    if let Some(input) = object.get("input").and_then(Value::as_str) {
        object.insert(
            "input".into(),
            json!([{"type":"message", "role":"user",
            "content":[{"type":"input_text","text":input}]}]),
        );
    }
    if let Some(items) = object.get_mut("input").and_then(Value::as_array_mut) {
        for item in items {
            if let Some(object) = item.as_object_mut() {
                object.remove("prompt_cache_breakpoint");
                for field in ["content", "output"] {
                    if let Some(parts) = object.get_mut(field).and_then(Value::as_array_mut) {
                        for part in parts {
                            if let Some(part) = part.as_object_mut() {
                                part.remove("prompt_cache_breakpoint");
                            }
                        }
                    }
                }
            }
            if item.get("role").and_then(Value::as_str) == Some("system") {
                item["role"] = "developer".into();
            }
            if item.get("type").and_then(Value::as_str) == Some("function_call")
                && item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.trim().is_empty())
            {
                item["arguments"] = "{}".into();
            }
        }
    }
    if object.get("max_output_tokens").is_none_or(Value::is_null) {
        for field in ["max_completion_tokens", "max_tokens"] {
            if let Some(value) = object.get(field).filter(|v| v.is_number()).cloned() {
                object.insert("max_output_tokens".into(), value);
                break;
            }
        }
    }
    if object.get("parallel_tool_calls") != Some(&Value::Bool(false)) {
        object.insert("parallel_tool_calls".into(), true.into());
    }
    let sources = object
        .get("include")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|v| v == "web_search_call.action.sources"));
    object.insert(
        "include".into(),
        if sources {
            json!([
                "reasoning.encrypted_content",
                "web_search_call.action.sources"
            ])
        } else {
            json!(["reasoning.encrypted_content"])
        },
    );
    if let Some(tier) = object.get("service_tier") {
        match tier
            .as_str()
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("priority" | "fast") => {
                object.insert("service_tier".into(), "priority".into());
            }
            Some("ultrafast") => {
                object.insert("service_tier".into(), "ultrafast".into());
            }
            _ => {
                object.remove("service_tier");
            }
        }
    }
    for key in [
        "max_completion_tokens",
        "max_tokens",
        "temperature",
        "top_p",
        "truncation",
        "prompt_cache_options",
        "user",
    ] {
        object.remove(key);
    }
    object.insert("model".into(), model.into());
    object.insert("stream".into(), true.into());
    object.insert("store".into(), false.into());
    if object.get("instructions").is_none_or(Value::is_null) {
        object.insert("instructions".into(), "".into());
    }
    if let Some(budget) = object.get("max_output_tokens").and_then(Value::as_f64)
        && (1.0..16.0).contains(&budget)
        && budget.fract() == 0.0
    {
        object.insert("max_output_tokens".into(), json!(16));
    }
    for key in [
        "previous_response_id",
        "prompt_cache_retention",
        "safety_identifier",
        "stream_options",
    ] {
        object.remove(key);
    }
    if object
        .get("tools")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        object.remove("parallel_tool_calls");
    }
    for path in ["/tools", "/tool_choice/tools"] {
        if let Some(tools) = body.pointer_mut(path).and_then(Value::as_array_mut) {
            for tool in tools {
                normalize_builtin_tool(tool);
            }
        }
    }
    if let Some(choice) = body.get_mut("tool_choice") {
        normalize_builtin_tool(choice);
    }
    sanitize_input_ids(body);
    Ok(())
}
fn normalize_builtin_tool(tool: &mut Value) {
    if matches!(
        text(tool, "/type"),
        "web_search_preview" | "web_search_preview_2025_03_11"
    ) {
        tool["type"] = "web_search".into();
    }
}

/// Compact uses native Responses, not the normal Codex streaming translator.
pub fn normalize_compact(body: &mut Value, model: &str) -> Result<()> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Codex compact request must be an object"))?;
    ensure!(!model.is_empty(), "Codex model missing");
    ensure!(
        !object.contains_key("messages"),
        "Codex compact requires Responses input"
    );
    ensure!(
        object
            .get("previous_response_id")
            .is_none_or(|v| v.is_null() || v.as_str() == Some("")),
        "Codex compact requires full conversation input"
    );
    object.insert("model".into(), model.into());
    object.remove("stream");
    if object.get("instructions").is_none_or(Value::is_null) {
        object.insert("instructions".into(), "".into());
    }
    sanitize_input_ids(body);
    Ok(())
}

/// Go helps/codex_input_ids.go: only item IDs change; call_id links remain untouched.
pub fn sanitize_input_ids(body: &mut Value) {
    use std::collections::{HashMap, HashSet};
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    items.retain(|item| {
        !(text(item, "/type") == "reasoning"
            && text(item, "/id").chars().count() > 64
            && !text(item, "/encrypted_content").is_empty())
    });
    let normalized: Vec<_> = items
        .iter()
        .map(|item| {
            let id = item.get("id").and_then(Value::as_str)?;
            let prefix = match text(item, "/type") {
                "message" => "msg",
                "reasoning" => "rs",
                "function_call" => "fc",
                "custom_tool_call" => "ctc",
                "custom_tool_call_output" => "ctco",
                "tool_search_call" => "tsc",
                _ => "",
            };
            let raw = if prefix == "tsc" && id.starts_with("ts_") && id.len() > 3 {
                format!("tsc_{}", &id[3..])
            } else if id.is_empty() || prefix.is_empty() || id.starts_with(prefix) {
                id.to_owned()
            } else {
                format!("{prefix}_{id}")
            };
            let normalized = if raw.chars().count() > 64 {
                raw.clone()
            } else {
                portable_id(&raw)
            };
            Some((id.to_owned(), raw, normalized))
        })
        .collect();
    let mut occupied: HashSet<String> = normalized
        .iter()
        .flatten()
        .filter(|(_, _, id)| id.chars().count() <= 64)
        .map(|(_, _, id)| id.clone())
        .collect();
    let mut preserved: HashSet<String> = normalized
        .iter()
        .flatten()
        .filter(|(original, _, id)| original == id)
        .map(|(_, _, id)| id.clone())
        .collect();
    let mut mapped: HashMap<(String, String), String> = HashMap::new();
    for (item, entry) in items.iter_mut().zip(normalized) {
        let Some((original, raw, normalized)) = entry else {
            continue;
        };
        let key = (normalized.clone(), original.clone());
        let id = if let Some(id) = mapped.get(&key) {
            id.clone()
        } else {
            let collision = normalized != original && preserved.contains(&normalized);
            let id = if normalized.chars().count() > 64 || collision {
                let source = if collision { &raw } else { &normalized };
                let mut attempt = 0;
                loop {
                    let candidate = hashed_id(source, attempt);
                    if !occupied.contains(&candidate) {
                        break candidate;
                    }
                    attempt += 1;
                }
            } else {
                normalized
            };
            occupied.insert(id.clone());
            preserved.insert(id.clone());
            mapped.insert(key, id.clone());
            id
        };
        item["id"] = id.into();
    }
}
fn portable_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}
fn hashed_id(id: &str, attempt: usize) -> String {
    use sha2::{Digest, Sha256};
    let input = if attempt == 0 {
        id.to_owned()
    } else {
        format!("{id}\0{attempt}")
    };
    let digest = Sha256::digest(input.as_bytes());
    format!(
        "{}_{}",
        portable_id(&id.chars().take(47).collect::<String>()),
        hex::encode(&digest[..8])
    )
}

/// Extract the terminal Responses object for a nonstream downstream request.
/// The gateway must feed parsed SSE data, preserve error status, and reject EOF
/// without a terminal event. This is not an unbounded SSE buffering implementation.
pub fn completed_response(event: &Value) -> Result<Option<Value>> {
    match event.get("type").and_then(Value::as_str) {
        Some("response.completed" | "response.done") => {
            let response = event
                .get("response")
                .filter(|v| v.is_object())
                .ok_or_else(|| anyhow::anyhow!("Codex completion missing response"))?;
            Ok(Some(response.clone()))
        }
        Some("error" | "response.failed" | "response.incomplete") => {
            anyhow::bail!("Codex response did not complete successfully")
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::*;

    #[test]
    fn terminal_alias_has_identical_response_validation() {
        for kind in ["response.completed", "response.done"] {
            for response in [json!({"id":"resp_test","output":[]}), json!({})] {
                let event = json!({"type":kind,"response":response});
                assert_eq!(completed_response(&event).unwrap(), Some(response));
            }
            for response in [Value::Null, json!([]), json!("invalid"), json!(true)] {
                assert!(completed_response(&json!({"type":kind,"response":response})).is_err());
            }
            assert!(completed_response(&json!({"type":kind})).is_err());
        }
        for kind in ["error", "response.failed", "response.incomplete"] {
            assert!(completed_response(&json!({"type":kind,"response":{}})).is_err());
        }
        assert!(
            completed_response(&json!({"type":"response.created"}))
                .unwrap()
                .is_none()
        );
    }
}
