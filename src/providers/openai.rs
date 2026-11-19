//! Native Chat/compact subset of openai_compat_executor.go; translation belongs to the gateway.
use super::{insert, text};
use crate::home::client::Selection;
use anyhow::{Result, ensure};
use http::HeaderMap;
use serde_json::{Value, json};

pub(super) fn is_compat(selection: &Selection) -> bool {
    matches!(
        selection.provider.as_str(),
        "openai" | "openai-compatibility"
    ) || !text(&selection.auth, "/attributes/compat_name").is_empty()
        || !text(&selection.auth, "/attributes/provider_key").is_empty()
}

pub(super) fn headers(headers: &mut HeaderMap, incoming: &HeaderMap) -> Result<()> {
    if !headers.contains_key("user-agent") {
        if let Some(value) = incoming.get("user-agent") {
            headers.insert("user-agent", value.clone());
        } else {
            insert(headers, "user-agent", "cli-proxy-openai-compat")?;
        }
    }
    Ok(())
}

fn base_model(model: &str) -> &str {
    model.trim().split('(').next().unwrap_or("").trim()
}

/// Select against the dispatched upstream and route alias, never an unrelated client's alias.
/// Mirrors sdk/cliproxy/auth/api_key_model_capabilities.go:homeAPIKeyModelOptions.
pub fn model_options<'a>(selection: &'a Selection, route: &str) -> Option<&'a Value> {
    let auth = &selection.auth;
    let models = auth
        .pointer("/metadata/credential_options/models")?
        .as_array()?;
    let upstream = text(auth, "/attributes/home_upstream_model");
    let upstream = if upstream.is_empty() {
        selection.model.trim()
    } else {
        upstream
    };
    let prefix = text(auth, "/prefix");
    let route = route
        .trim()
        .strip_prefix(&format!("{prefix}/"))
        .unwrap_or(route.trim());
    let routes = [route, base_model(route)];
    let candidates = [upstream, base_model(upstream)];
    let name = |m: &'a Value| {
        let name = text(m, "/name");
        if name.is_empty() {
            text(m, "/alias")
        } else {
            name
        }
    };
    for route in routes {
        for candidate in candidates {
            if let Some(model) = models.iter().find(|m| {
                name(m).eq_ignore_ascii_case(candidate)
                    && (text(m, "/alias").eq_ignore_ascii_case(route)
                        || name(m).eq_ignore_ascii_case(route))
            }) {
                return Some(model);
            }
        }
    }
    for alias in [false, true] {
        for candidate in candidates {
            if let Some(model) = models.iter().find(|m| {
                let value = if alias { text(m, "/alias") } else { name(m) };
                !candidate.is_empty() && value.eq_ignore_ascii_case(candidate)
            }) {
                return Some(model);
            }
        }
    }
    None
}

/// Built-ins only. Caller applies payload rules after this function, exactly once.
pub fn normalize_request(
    body: &mut Value,
    selection: &Selection,
    route: &str,
    compact: bool,
    stream: bool,
) -> Result<()> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("OpenAI request must be an object"))?;
    object.insert("model".into(), selection.model.clone().into());
    if compact {
        object.remove("stream");
        return Ok(());
    }
    ensure!(
        !object.contains_key("input"),
        "OpenAI compatibility requires translated Chat input"
    );
    let options = model_options(selection, route);
    let use_mct = options
        .and_then(|m| m.get("use-max-completion-tokens"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (keep, remove) = if use_mct {
        ("max_completion_tokens", "max_tokens")
    } else {
        ("max_tokens", "max_completion_tokens")
    };
    if let Some(value) = object.remove(remove) {
        object.entry(keep).or_insert(value);
    }
    object.insert("stream".into(), stream.into());
    if stream {
        let options = object.entry("stream_options").or_insert_with(|| json!({}));
        if !options.is_object() {
            *options = json!({});
        }
        options["include_usage"] = true.into();
    }
    // Existing explicit keys pass through even when support is false (Go does not delete them).
    let support = match text(&selection.auth, "/attributes/support_prompt_cache_key") {
        "true" | "TRUE" | "True" | "1" | "t" | "T" => Some(true),
        "false" | "FALSE" | "False" | "0" | "f" | "F" => Some(false),
        _ => selection
            .auth
            .pointer("/metadata/credential_options/support-prompt-cache-key")
            .and_then(Value::as_bool),
    };
    if support == Some(true)
        && let Some(key) = object
            .get("prompt_cache_key")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    {
        object.insert("prompt_cache_key".into(), key.into());
    }
    if options
        .and_then(|m| m.get("input-modalities"))
        .and_then(Value::as_array)
        .is_some_and(|m| {
            m.iter().any(|v| {
                v.as_str()
                    .is_some_and(|s| s.trim().eq_ignore_ascii_case("text"))
            }) && !m.iter().any(|v| {
                v.as_str()
                    .is_some_and(|s| s.trim().eq_ignore_ascii_case("image"))
            })
        })
        && let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut)
    {
        normalize_tool_results(messages);
    }
    Ok(())
}

const OMITTED: &str = "[image omitted: unsupported by upstream]";
const PLACEHOLDER: &str =
    "[Tool returned image content; the images follow in the next user message.]";
const RELAY: &str = "Images returned by the preceding tool call(s):";
fn image(part: &Value) -> bool {
    matches!(
        text(part, "/type").to_ascii_lowercase().as_str(),
        "image" | "image_url" | "input_image"
    ) || part.get("image_url").is_some()
        || part.get("input_image").is_some()
}
fn flatten(part: &Value) -> String {
    if image(part) {
        OMITTED.into()
    } else if let Some(text) = part
        .as_str()
        .or_else(|| part.get("text").and_then(Value::as_str))
    {
        text.into()
    } else {
        part.to_string()
    }
}
fn normalize_tool_results(messages: &mut Vec<Value>) {
    let mut replaced = false;
    let mut drop = Vec::new();
    for i in 0..messages.len() {
        match text(&messages[i], "/role") {
            "tool" => {
                if let Some(content) = messages[i].get("content").cloned() {
                    if content.as_str() == Some(PLACEHOLDER) {
                        messages[i]["content"] = OMITTED.into();
                        replaced = true;
                    } else if !content.is_string() {
                        messages[i]["content"] = content
                            .as_array()
                            .map(|parts| parts.iter().map(flatten).collect::<Vec<_>>().join("\n\n"))
                            .unwrap_or_else(|| flatten(&content))
                            .into();
                    }
                }
            }
            "user" => {
                if let Some(parts) = messages[i].get("content").and_then(Value::as_array)
                    && parts
                        .iter()
                        .any(|p| p.get("text").and_then(Value::as_str) == Some(RELAY))
                    && parts.iter().any(image)
                {
                    let kept: Vec<_> = parts
                        .iter()
                        .filter(|p| {
                            !image(p) && p.get("text").and_then(Value::as_str) != Some(RELAY)
                        })
                        .cloned()
                        .collect();
                    if !replaced && i > 0 && text(&messages[i - 1], "/role") == "tool" {
                        let previous = text(&messages[i - 1], "/content");
                        if !previous.contains(OMITTED) {
                            messages[i - 1]["content"] = if previous.is_empty() {
                                OMITTED.into()
                            } else {
                                format!("{previous}\n\n{OMITTED}").into()
                            };
                        }
                    }
                    replaced = false;
                    if kept.is_empty() {
                        drop.push(i);
                    } else {
                        messages[i]["content"] = kept.into();
                    }
                }
            }
            _ => replaced = false,
        }
    }
    for i in drop.into_iter().rev() {
        messages.remove(i);
    }
}
