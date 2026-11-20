//! Basic main-thread cache placement, limit priority and TTL ordering from Go.
use serde_json::{Value, json};

fn valid(value: &Value) -> bool {
    value["type"] == "ephemeral"
}
fn cacheable(b: &Value) -> bool {
    match b["type"].as_str().unwrap_or("") {
        "" | "thinking" | "redacted_thinking" => false,
        "text" => b["text"].as_str().is_some_and(|s| !s.trim().is_empty()),
        _ => b.is_object(),
    }
}
fn target(message: &Value) -> Option<usize> {
    let blocks = message["content"].as_array()?;
    for (i, b) in blocks.iter().enumerate().rev() {
        if b["type"] == "text" && !b["text"].as_str().is_some_and(|s| !s.trim().is_empty()) {
            continue;
        }
        if message["role"] == "assistant"
            && matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking"))
        {
            return None;
        }
        if cacheable(b) {
            return Some(i);
        }
    }
    None
}

pub(super) fn normalize(body: &mut Value) {
    let messages = body["messages"].as_array_mut().unwrap();
    let last = messages.iter().rposition(|m| {
        m["role"] != "system"
            && (m["content"].as_str().is_some_and(|s| !s.trim().is_empty()) || target(m).is_some())
    });
    if let Some(mut i) = last {
        if messages.last().is_some_and(|m| {
            m["role"] == "system" && m["content"].as_str().is_some_and(|s| !s.trim().is_empty())
        }) {
            i = messages.len() - 1;
        }
        let m = &mut messages[i];
        if let Some(text) = m["content"].as_str() {
            m["content"] =
                json!([{"type":"text","text":text,"cache_control":{"type":"ephemeral"}}]);
        } else if !m["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| valid(&b["cache_control"]))
            && let Some(j) = target(m)
        {
            m["content"][j]["cache_control"] = json!({"type":"ephemeral"});
        }
    }
    let (tools, system, messages) = paths(body);
    let total = tools.len() + system.len() + messages.len();
    let removal = system
        .iter()
        .take(system.len().saturating_sub(1))
        .chain(tools.iter().take(tools.len().saturating_sub(1)))
        .chain(messages.iter())
        .chain(system.last())
        .chain(tools.last());
    for path in removal.take(total.saturating_sub(4)) {
        body.pointer_mut(path)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
    }
    let (tools, system, messages) = paths(body);
    let mut seen5m = false;
    for path in tools.iter().chain(&system).chain(&messages) {
        let cc = &mut body.pointer_mut(path).unwrap()["cache_control"];
        if cc.get("ttl").is_none() {
            cc["ttl"] = "1h".into();
        }
        if cc["ttl"] != "1h" {
            seen5m = true;
        } else if seen5m {
            cc.as_object_mut().unwrap().remove("ttl");
        }
    }
}

fn paths(body: &Value) -> (Vec<String>, Vec<String>, Vec<String>) {
    let section = |name: &str| {
        body[name]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .filter(|(_, b)| valid(&b["cache_control"]))
            .map(|(i, _)| format!("/{name}/{i}"))
            .collect::<Vec<_>>()
    };
    let mut messages = Vec::new();
    for (i, m) in body["messages"].as_array().unwrap().iter().enumerate() {
        for (j, b) in m["content"].as_array().into_iter().flatten().enumerate() {
            if valid(&b["cache_control"]) {
                messages.push(format!("/messages/{i}/content/{j}"));
            }
        }
    }
    (section("tools"), section("system"), messages)
}
