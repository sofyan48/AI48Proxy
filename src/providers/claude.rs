//! Claude Messages bearer/API-key execution. This is a native-input subset,
//! not CPA's full cloaking, signing, tool aliasing or per-model beta engine.
use super::{insert, text};
use anyhow::{Result, ensure};
use http::HeaderMap;
use serde_json::{Value, json};

#[path = "claude_betas.rs"]
mod betas;

#[cfg(test)]
#[path = "claude_contract_tests.rs"]
mod contract_tests;

pub(super) fn headers(headers: &mut HeaderMap) -> Result<()> {
    for (name, value) in [
        ("user-agent", "claude-cli/2.1.321 (external, cli)"),
        ("anthropic-dangerous-direct-browser-access", "true"),
        ("x-app", "cli"),
        ("x-stainless-retry-count", "0"),
        ("x-stainless-runtime", "node"),
        ("x-stainless-lang", "js"),
        ("x-stainless-timeout", "600"),
        ("x-stainless-package-version", "0.128.0"),
        ("x-stainless-runtime-version", "v26.12.1"),
        ("x-stainless-os", "MacOS"),
        ("x-stainless-arch", "arm64"),
    ] {
        insert(headers, name, value)?;
    }
    Ok(())
}

pub(super) fn prepare_body(
    body: &mut Value,
    headers: &mut HeaderMap,
    auth: &Value,
    incoming: &HeaderMap,
    oauth: bool,
) -> Result<()> {
    ensure!(
        body.get("messages").is_some_and(Value::is_array),
        "Claude Messages input missing"
    );
    normalize_native_body(body);
    if let Some(version) = incoming.get("anthropic-version") {
        headers.insert("anthropic-version", version.clone());
    }
    if !oauth {
        return request_betas(body, headers, incoming, Vec::new());
    }
    // Home owns persistent credential identity. Never generate a device/account
    // locally or trust a caller's credential identity from metadata.user_id.
    let account = [
        text(auth, "/metadata/account_uuid"),
        text(auth, "/metadata/accountUuid"),
    ]
    .into_iter()
    .find(|s| !s.is_empty())
    .unwrap_or("");
    ensure!(
        !account.is_empty(),
        "Claude OAuth requires Home account_uuid"
    );
    let device = auth
        .pointer("/metadata/claude_device_ids")
        .and_then(Value::as_array)
        .filter(|pool| pool.len() == 1)
        .and_then(|pool| pool[0].as_str())
        .unwrap_or("");
    ensure!(
        device.len() == 64
            && device
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "Claude OAuth requires canonical Home claude_device_ids"
    );
    for field in [
        "thread",
        "fallbacks",
        "safeguards",
        "diagnostics",
        "context_management",
    ] {
        ensure!(
            body.get(field).is_none(),
            "Claude extended feature requires unsupported native handling"
        );
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        ensure!(
            tools.iter().all(|t| {
                t.get("type").is_none() || t.get("type").and_then(Value::as_str) == Some("custom")
            }),
            "Claude server/advanced tools unsupported"
        );
    }
    // Go rebuilds typed client declarations without `type`. Names remain native:
    // aliasing requires a per-request reverse map in both JSON and SSE responses.
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if let Some(tool) = tool.as_object_mut() {
                tool.remove("type");
            }
        }
    }
    let existing_identity = body
        .pointer("/metadata/user_id")
        .and_then(Value::as_str)
        .and_then(|value| serde_json::from_str::<Value>(value).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let session = incoming
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| uuid::Uuid::parse_str(v).is_ok())
        .or_else(|| {
            existing_identity
                .get("session_id")
                .and_then(Value::as_str)
                .filter(|value| uuid::Uuid::parse_str(value).is_ok())
        })
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if body.get("metadata").is_none_or(Value::is_null) {
        body["metadata"] = json!({});
    }
    ensure!(
        body["metadata"].is_object(),
        "Claude metadata must be an object"
    );
    let mut identity = existing_identity;
    identity["device_id"] = device.into();
    identity["account_uuid"] = account.into();
    identity["session_id"] = session.clone().into();
    body["metadata"]["user_id"] = identity.to_string().into();
    insert(headers, "x-claude-code-session-id", &session)?;
    insert(
        headers,
        "x-client-request-id",
        &uuid::Uuid::new_v4().to_string(),
    )?;
    // Basic OAuth baseline follows Go's model capability ordering. Explicit
    // caller/operator extensions still pass through the existing merge policy.
    let baseline = betas::baseline(body);
    request_betas(body, headers, incoming, baseline)
}

// Body betas are transport metadata for both credential kinds, never a Messages field.
fn request_betas(
    body: &mut Value,
    headers: &mut HeaderMap,
    incoming: &HeaderMap,
    mut betas: Vec<String>,
) -> Result<()> {
    // Preserve operator betas when there is no caller header, as in Go's custom
    // gateway escape hatch. A caller header replaces the configured starting set.
    if !incoming.contains_key("anthropic-beta") {
        for value in headers.get_all("anthropic-beta") {
            append_betas(
                &mut betas,
                value
                    .to_str()
                    .map_err(|_| anyhow::anyhow!("invalid Anthropic beta header"))?,
            );
        }
    }
    for value in incoming.get_all("anthropic-beta") {
        let value = value
            .to_str()
            .map_err(|_| anyhow::anyhow!("invalid Anthropic beta header"))?;
        append_betas(&mut betas, value);
    }
    if let Some(extra) = body.get("betas") {
        if let Some(beta) = extra.as_str() {
            append_betas(&mut betas, beta);
        } else {
            let values = extra
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("Claude betas must be a string or array"))?;
            for beta in values {
                append_betas(
                    &mut betas,
                    beta.as_str()
                        .ok_or_else(|| anyhow::anyhow!("Claude beta must be a string"))?,
                );
            }
        }
    }
    body.as_object_mut()
        .expect("validated object")
        .remove("betas");
    if body
        .get("speed")
        .and_then(Value::as_str)
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("fast"))
    {
        append_betas(&mut betas, "fast-mode-2026-02-01");
    }
    if body.pointer("/thinking/block_binding").is_some() {
        append_betas(&mut betas, "thinking-binding-controls-2026-08-01");
    }
    if betas.is_empty() {
        headers.remove("anthropic-beta");
    } else {
        insert(headers, "anthropic-beta", &betas.join(","))?;
    }
    Ok(())
}

fn append_betas(betas: &mut Vec<String>, value: &str) {
    for beta in value.split(',').map(str::trim).filter(|v| !v.is_empty()) {
        if !betas.iter().any(|v| v == beta) {
            betas.push(beta.into());
        }
    }
}

fn normalize_native_body(body: &mut Value) {
    // Native input owns sampling; remove only combinations Anthropic rejects.
    // This runs before the gateway's final user payload-rule barrier.
    if matches!(
        body.pointer("/tool_choice/type").and_then(Value::as_str),
        Some("any" | "tool")
    ) {
        body.as_object_mut()
            .expect("Messages object")
            .remove("thinking");
        if let Some(output) = body.get_mut("output_config").and_then(Value::as_object_mut) {
            output.remove("effort");
            if output.is_empty() {
                body.as_object_mut()
                    .expect("Messages object")
                    .remove("output_config");
            }
        }
    }
    let thinking = body
        .pointer("/thinking/type")
        .and_then(Value::as_str)
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "enabled" | "adaptive" | "auto"
            )
        });
    let object = body.as_object_mut().expect("Messages object");
    if thinking {
        if object
            .get("temperature")
            .is_some_and(|v| v.as_f64() != Some(1.0))
        {
            object.remove("temperature");
        }
        if object
            .get("top_p")
            .is_some_and(|v| v.as_f64().is_none_or(|v| v < 0.95))
        {
            object.remove("top_p");
        }
        object.remove("top_k");
    } else if object.contains_key("temperature") && object.contains_key("top_p") {
        object.remove("top_p");
    }
    // Tool provenance from other providers is not Anthropic tool_use input.
    // Do not manufacture or guess compatibility of opaque thinking signatures.
    for message in body["messages"].as_array_mut().expect("validated messages") {
        if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
            for part in content {
                if part.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                if let Some(part) = part.as_object_mut() {
                    for field in [
                        "signature",
                        "thoughtSignature",
                        "thought_signature",
                        "model",
                    ] {
                        part.remove(field);
                    }
                    if let Some(extra) =
                        part.get_mut("extra_content").and_then(Value::as_object_mut)
                    {
                        if let Some(google) = extra.get_mut("google").and_then(Value::as_object_mut)
                        {
                            google.remove("thought_signature");
                            if google.is_empty() {
                                extra.remove("google");
                            }
                        }
                        if extra.is_empty() {
                            part.remove("extra_content");
                        }
                    }
                }
            }
        }
    }
}
