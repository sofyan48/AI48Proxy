//! Codex multi-agent v2 and orphan delegation compatibility for HTTP Responses.
//! Mirrors CPA internal/client/codex/optimize-multi-agent-v2 (request rewrites)
//! and helps.RestoreCodexMultiAgentV2Response (Codex response restore).
//! Every rewrite runs before provider preparation, so `payload.apply_context`
//! remains the single final barrier.
use axum::http::HeaderMap;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};

use crate::config::Config;
use crate::home::client::{HomeClient, Selection};

#[path = "codex_models.rs"]
pub(crate) mod models;

const COLLABORATION: &str = "collaboration";
const OPTIMIZED: &str = "collaboration-optimize";
const OPTIMIZED_NAME_PREFIX: &str = "collaboration-optimize__";
const OPTIMIZED_DOT_PREFIX: &str = "collaboration-optimize.";
const MESSAGE_TOOLS: &[&str] = &["spawn_agent", "send_message", "followup_task"];

/// Restore stops (and passes bytes through untouched) beyond this per-line/body size.
const RESTORE_LIMIT: usize = 32 * 1024 * 1024;

fn responses_path(path: &str) -> bool {
    matches!(path, "/v1/responses" | "/v1/responses/compact")
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::trim)
        .find(|v| !v.is_empty())
        .unwrap_or("")
}

/// Official Codex client identities (CPA IsCodexClientUserAgent).
pub(crate) fn is_codex_client(user_agent: &str) -> bool {
    let ua = user_agent.trim();
    ua.starts_with("Codex Desktop/")
        || ua.starts_with("codex-tui/")
        || ua == "codex_cli_rs"
        || ua.starts_with("codex_cli_rs/")
        || ua.starts_with("codex_exec/")
}

fn optimize_enabled(config: &Config, headers: &HeaderMap) -> bool {
    config.providers.codex_optimize_multi_agent_v2 && is_codex_client(header(headers, "user-agent"))
}

fn orphan_enabled(config: &Config, headers: &HeaderMap) -> bool {
    config.providers.codex_orphan_delegation
        && header(headers, "x-openai-subagent").eq_ignore_ascii_case("collab_spawn")
}

fn kind(value: &Value) -> &str {
    value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
}

fn name(value: &Value) -> &str {
    value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
}

/// Responses API boundary (CPA prepareCodexRequestPayload / Compact handler).
/// Applies to every provider, before dispatch-derived session keys are computed.
#[cfg(test)]
pub(crate) fn prepare_boundary(config: &Config, path: &str, headers: &HeaderMap, body: &mut Value) {
    prepare_boundary_with_models(config, path, headers, body, &[]);
}

/// Applies boundary rewrites and also renders the spawn_agent "Available model
/// overrides" list from `models` (CPA prepareCodexMultiAgentV2ToolsDoc). An
/// empty list leaves descriptions untouched, like CPA without catalog/models.
pub(crate) fn prepare_boundary_with_models(
    config: &Config,
    path: &str,
    headers: &HeaderMap,
    body: &mut Value,
    models: &[models::SpawnAgentModel],
) {
    if !responses_path(path) {
        return;
    }
    if path == "/v1/responses" && optimize_enabled(config, headers) {
        if !has_conflict(body) {
            rewrite_spawn_agent_descriptions(body, &models::format_models(models));
        }
        remove_message_encryption(body);
    }
    if orphan_enabled(config, headers) {
        rewrite_orphan_delegation(body);
    }
}

/// Whether the boundary would render a spawn_agent model list for this request,
/// i.e. whether fetching Home's models is worthwhile.
pub(crate) fn needs_spawn_agent_models(
    config: &Config,
    path: &str,
    headers: &HeaderMap,
    body: &Value,
) -> bool {
    fn spawns(tools: Option<&Value>) -> bool {
        tools.and_then(Value::as_array).is_some_and(|tools| {
            tools.iter().any(|tool| match kind(tool) {
                "function" => {
                    name(tool) == "spawn_agent"
                        && tool.get("description").is_some_and(Value::is_string)
                }
                "namespace" => spawns(tool.get("tools")),
                _ => false,
            })
        })
    }
    if path != "/v1/responses" || !optimize_enabled(config, headers) || has_conflict(body) {
        return false;
    }
    spawns(body.get("tools"))
        || body
            .get("input")
            .and_then(Value::as_array)
            .is_some_and(|input| {
                input
                    .iter()
                    .any(|i| kind(i) == "additional_tools" && spawns(i.get("tools")))
            })
}

fn spawn_agent_models_request(headers: &HeaderMap) -> Value {
    json!({"type":"models", "headers":super::super::auth_headers(headers),
        "query":{"client_version":""}})
}

/// Home-mode spawn_agent models (CPA codexHomeAvailableModels): Home's scoped
/// `GET models` view resolved against the runtime Codex client catalog.
/// Any Home failure yields no models, so descriptions stay untouched.
pub(crate) async fn fetch_spawn_agent_models(
    home: &HomeClient,
    headers: &HeaderMap,
) -> Vec<models::SpawnAgentModel> {
    // Always read the current config, not a potentially stale boundary snapshot.
    // A reload owns a new cache; old in-flight work cannot publish into it.
    let config = home.config();
    let cancel = home.cancellation();
    let Some(catalog) = config.providers.codex_catalog.snapshot(&cancel).await else {
        return Vec::new();
    };
    if !std::sync::Arc::ptr_eq(&config, &home.config()) || cancel.is_cancelled() {
        return Vec::new();
    }
    let Some(templates) = models::parse_templates(&catalog) else {
        return Vec::new();
    };
    let request = spawn_agent_models_request(headers);
    let Ok(payload) = serde_json::to_vec(&request) else {
        return Vec::new();
    };
    let raw = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Vec::new(),
        result = home.command(vec![b"GET".to_vec(), payload]) => result.and_then(|frame| frame.bytes()),
    };
    match raw
        .ok()
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
    {
        Some(raw) if std::sync::Arc::ptr_eq(&config, &home.config()) && !cancel.is_cancelled() => {
            models::models_from_home_templates(&raw, &templates)
        }
        Some(_) => Vec::new(),
        None => Vec::new(),
    }
}

/// HTTP Responses boundary with Home-provided spawn_agent models.
pub(crate) async fn prepare_boundary_home(
    home: &HomeClient,
    config: &Config,
    path: &str,
    headers: &HeaderMap,
    body: &mut Value,
) {
    let models = if needs_spawn_agent_models(config, path, headers, body) {
        fetch_spawn_agent_models(home, headers).await
    } else {
        Vec::new()
    };
    prepare_boundary_with_models(config, path, headers, body, &models);
}

/// Replace the model list in every spawn_agent string description.
pub(crate) fn rewrite_spawn_agent_descriptions(body: &mut Value, model_list: &str) -> bool {
    if model_list.is_empty() {
        return false;
    }
    let mut changed = false;
    visit_tools(body, &["spawn_agent"], &mut |tool| {
        let Some(Value::String(description)) = tool.get_mut("description") else {
            return;
        };
        let rewritten = models::replace_models(description, model_list);
        if rewritten != *description {
            *description = rewritten;
            changed = true;
        }
    });
    changed
}

/// Per-attempt executor stage. Returns whether the Codex collaboration
/// namespace was renamed, i.e. whether the response must be restored.
pub(crate) fn prepare_attempt(
    config: &Config,
    selection: &Selection,
    path: &str,
    headers: &HeaderMap,
    body: &mut Value,
) -> bool {
    if !responses_path(path) {
        return false;
    }
    let provider = selection.provider.to_ascii_lowercase();
    let compat = is_compat(selection, body);
    let optimize = optimize_enabled(config, headers);
    match provider.as_str() {
        // CPA OptimizeCodexMultiAgentV2RequestForAuthDoc.
        "codex" => {
            let mut optimized = false;
            if optimize {
                rewrite_agent_message_content(body);
                remove_message_encryption(body);
                optimized = !has_conflict(body) && rename_collaboration_namespace(body);
            }
            if compat {
                rewrite_agent_message_input(body, true, true);
            }
            optimized
        }
        // Responses-native targets keep agent_message items (CPA skips Codex/Responses targets).
        "xai" | "meta" => false,
        _ => {
            if path != "/v1/responses/compact" && (optimize || compat) {
                rewrite_agent_message_input(body, true, compat);
            }
            false
        }
    }
}

fn is_compat(selection: &Selection, body: &Value) -> bool {
    let route = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&selection.model);
    crate::providers::openai::model_options(selection, route)
        .and_then(|m| m.get("is-compat"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Visit function tools whose name is in `names`, in top-level `tools` and
/// `input[].additional_tools`, descending into namespace tools.
fn visit_tools(body: &mut Value, names: &[&str], f: &mut dyn FnMut(&mut Value)) {
    fn walk(tools: Option<&mut Value>, names: &[&str], f: &mut dyn FnMut(&mut Value)) {
        let Some(tools) = tools.and_then(Value::as_array_mut) else {
            return;
        };
        for tool in tools {
            match kind(tool) {
                "function" if names.contains(&name(tool)) => f(tool),
                "namespace" => walk(tool.get_mut("tools"), names, f),
                _ => {}
            }
        }
    }
    walk(body.get_mut("tools"), names, f);
    if let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) {
        for item in input.iter_mut().filter(|i| kind(i) == "additional_tools") {
            walk(item.get_mut("tools"), names, f);
        }
    }
}

pub(crate) fn remove_message_encryption(body: &mut Value) -> bool {
    let mut changed = false;
    visit_tools(body, MESSAGE_TOOLS, &mut |tool| {
        if let Some(message) = tool
            .pointer_mut("/parameters/properties/message")
            .and_then(Value::as_object_mut)
        {
            changed |= message.remove("encrypted").is_some();
        }
    });
    changed
}

/// The reserved optimized namespace is client-defined; never touch such requests.
pub(crate) fn has_conflict(body: &Value) -> bool {
    fn tools_conflict(tools: Option<&Value>) -> bool {
        tools.and_then(Value::as_array).is_some_and(|tools| {
            tools.iter().any(|tool| {
                let n = name(tool);
                n == OPTIMIZED
                    || n.starts_with(OPTIMIZED_NAME_PREFIX)
                    || n.starts_with(OPTIMIZED_DOT_PREFIX)
                    || (kind(tool) == "namespace" && tools_conflict(tool.get("tools")))
            })
        })
    }
    tools_conflict(body.get("tools"))
        || body
            .get("input")
            .and_then(Value::as_array)
            .is_some_and(|input| {
                input
                    .iter()
                    .any(|i| kind(i) == "additional_tools" && tools_conflict(i.get("tools")))
            })
}

/// Rename each `collaboration` namespace that directly contains a spawn_agent function.
pub(crate) fn rename_collaboration_namespace(body: &mut Value) -> bool {
    fn walk(tools: Option<&mut Value>) -> bool {
        let Some(tools) = tools.and_then(Value::as_array_mut) else {
            return false;
        };
        let mut renamed = false;
        for tool in tools.iter_mut().filter(|t| kind(t) == "namespace") {
            let spawns = tool
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(|c| {
                    c.iter()
                        .any(|t| kind(t) == "function" && name(t) == "spawn_agent")
                });
            if spawns && name(tool) == COLLABORATION {
                tool["name"] = OPTIMIZED.into();
                renamed = true;
            }
            renamed |= walk(tool.get_mut("tools"));
        }
        renamed
    }
    // Spawn tools at the root or directly in additional_tools have no namespace to rename.
    let mut renamed = walk(body.get_mut("tools"));
    if let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) {
        for item in input.iter_mut().filter(|i| kind(i) == "additional_tools") {
            renamed |= walk(item.get_mut("tools"));
        }
    }
    renamed
}

/// agent_message encrypted_content parts become readable input_text parts.
pub(crate) fn rewrite_agent_message_content(body: &mut Value) -> bool {
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for item in input.iter_mut().filter(|i| kind(i) == "agent_message") {
        let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for part in content
            .iter_mut()
            .filter(|p| kind(p) == "encrypted_content")
        {
            let Some(part) = part.as_object_mut() else {
                continue;
            };
            let Some(Value::String(text)) = part.get("encrypted_content").cloned() else {
                continue;
            };
            part.insert("type".into(), "input_text".into());
            part.insert("text".into(), text.into());
            part.remove("encrypted_content");
            changed = true;
        }
    }
    changed
}

/// CPA rewriteCodexAgentMessageInputDoc.
pub(crate) fn rewrite_agent_message_input(body: &mut Value, optimize: bool, compat: bool) -> bool {
    let mut changed = optimize && rewrite_agent_message_content(body);
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return changed;
    };
    for item in input.iter_mut() {
        let agent = kind(item) == "agent_message";
        let Some(object) = item.as_object_mut() else {
            continue;
        };
        if agent && optimize {
            object.insert("role".into(), "user".into());
            object.insert("type".into(), "message".into());
            changed = true;
        }
        if compat {
            for key in [
                "author",
                "recipient",
                "internal_chat_message_metadata_passthrough",
            ] {
                changed |= object.remove(key).is_some();
            }
        }
    }
    changed
}

/// Orphan codex_app delegation outputs (no paired function_call in this request)
/// become user messages that preserve the exact output.
pub(crate) fn rewrite_orphan_delegation(body: &mut Value) -> bool {
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return false;
    };
    let call_id = |item: &Value| {
        item.get("call_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let raw_kind = |item: &Value| item.get("type").and_then(Value::as_str) == Some("function_call");
    let mut available: std::collections::HashMap<String, usize> = Default::default();
    for item in input.iter().filter(|i| raw_kind(i)) {
        let id = call_id(item);
        if !id.trim().is_empty() {
            *available.entry(id).or_default() += 1;
        }
    }
    let mut changed = false;
    for item in input.iter_mut() {
        if item.get("type").and_then(Value::as_str) != Some("function_call_output") {
            continue;
        }
        let id = call_id(item);
        if !id.trim().is_empty()
            && let Some(count) = available.get_mut(&id).filter(|c| **c > 0)
        {
            *count -= 1;
            continue;
        }
        if item.get("namespace").and_then(Value::as_str) != Some("codex_app") {
            continue;
        }
        let label = match item.get("name").and_then(Value::as_str) {
            Some("create_thread") => "codex_app__create_thread",
            Some("send_message_to_thread") => "codex_app__send_message_to_thread",
            _ => continue,
        };
        let output = match item.get("output") {
            None => String::new(),
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
        };
        *item = json!({"type":"message","role":"user","content":[
            {"type":"input_text","text":format!("Tool output from {label}:\n{output}")}]});
        changed = true;
    }
    changed
}

/// Restore optimized collaboration names in one JSON payload; None when unchanged.
pub(crate) fn restore_payload(payload: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(payload).ok()?;
    restore_value(&mut value)
        .then(|| serde_json::to_vec(&value).ok())
        .flatten()
}

fn restore_value(value: &mut Value) -> bool {
    match value {
        Value::Array(items) => items.iter_mut().fold(false, |c, i| restore_value(i) | c),
        Value::Object(map) => restore_object(map),
        _ => false,
    }
}

fn restore_object(map: &mut Map<String, Value>) -> bool {
    let item_type = map
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let tool_call = matches!(item_type.as_str(), "function_call" | "custom_tool_call");
    let mut changed = false;
    if tool_call && map.get("namespace").and_then(Value::as_str) == Some(OPTIMIZED) {
        map.insert("namespace".into(), COLLABORATION.into());
        changed = true;
    }
    if let Some(name) = map.get("name").and_then(Value::as_str).map(str::to_owned) {
        if name == OPTIMIZED && item_type == "namespace" {
            map.insert("name".into(), COLLABORATION.into());
            changed = true;
        } else if let Some(tool) = name
            .strip_prefix(OPTIMIZED_DOT_PREFIX)
            .filter(|_| tool_call)
        {
            if !tool.is_empty() {
                map.insert("namespace".into(), COLLABORATION.into());
                map.insert("name".into(), tool.into());
                changed = true;
            }
        } else if let Some(tool) = name
            .strip_prefix(OPTIMIZED_NAME_PREFIX)
            .filter(|_| tool_call)
        {
            map.insert("name".into(), format!("{COLLABORATION}__{tool}").into());
            changed = true;
        }
    }
    let outputs = matches!(
        item_type.as_str(),
        "function_call_output" | "custom_tool_call_output"
    );
    for (key, child) in map.iter_mut() {
        // Opaque tool arguments, inputs and outputs are never rewritten.
        if key == "arguments" || key == "input" || (key == "output" && outputs) {
            continue;
        }
        changed |= restore_value(child);
    }
    changed
}

/// One SSE line (with its terminator): `data:` payloads are restored, others pass.
fn restore_sse_line(line: &[u8]) -> Option<Vec<u8>> {
    let body_len = line
        .strip_suffix(b"\r\n")
        .or_else(|| line.strip_suffix(b"\n"))
        .map_or(line.len(), <[u8]>::len);
    let (content, terminator) = line.split_at(body_len);
    let data = content.strip_prefix(b"data:")?.trim_ascii();
    let restored = restore_payload(data)?;
    let mut out = Vec::with_capacity(restored.len() + 8);
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(&restored);
    out.extend_from_slice(terminator);
    Some(out)
}

#[derive(Default)]
struct Restorer {
    json: bool,
    pending: Vec<u8>,
    passthrough: bool,
}

impl Restorer {
    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.passthrough {
            return chunk.to_vec();
        }
        self.pending.extend_from_slice(chunk);
        let mut out = Vec::new();
        if !self.json {
            while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=end).collect();
                out.extend(restore_sse_line(&line).unwrap_or(line));
            }
        }
        if self.pending.len() > RESTORE_LIMIT {
            self.passthrough = true;
            out.append(&mut self.pending);
        }
        out
    }

    fn finish(&mut self) -> Vec<u8> {
        let rest = std::mem::take(&mut self.pending);
        if self.passthrough || rest.is_empty() {
            return rest;
        }
        let restored = if self.json {
            restore_payload(&rest)
        } else {
            restore_sse_line(&rest)
        };
        restored.unwrap_or(rest)
    }
}

/// Wrap an upstream Codex response so optimized names are restored before any
/// translation, usage accounting or client output (CPA restores per `data:` line
/// in stream/execute and the whole body for compact JSON).
pub(crate) fn restore_response(response: reqwest::Response, json: bool) -> reqwest::Response {
    let status = response.status();
    let version = response.version();
    let mut headers = response.headers().clone();
    headers.remove(http::header::CONTENT_LENGTH);
    let state = (
        response.bytes_stream(),
        Restorer {
            json,
            ..Default::default()
        },
        false,
    );
    let body = futures_util::stream::unfold(state, |(mut source, mut restorer, done)| async move {
        if done {
            return None;
        }
        loop {
            match source.next().await {
                Some(Ok(chunk)) => {
                    let out = restorer.feed(&chunk);
                    if !out.is_empty() {
                        return Some((Ok(Bytes::from(out)), (source, restorer, false)));
                    }
                }
                Some(Err(err)) => return Some((Err(err), (source, restorer, true))),
                None => {
                    let out = restorer.finish();
                    return (!out.is_empty())
                        .then(|| (Ok(Bytes::from(out)), (source, restorer, true)));
                }
            }
        }
    });
    let mut restored = http::Response::new(reqwest::Body::wrap_stream(body));
    *restored.status_mut() = status;
    *restored.version_mut() = version;
    *restored.headers_mut() = headers;
    restored.into()
}

#[cfg(test)]
#[path = "codex_multiagent_tests.rs"]
mod tests;
