//! Kimi Chat and native Responses text paths; Home owns credential refresh.
use super::{Credentials, Prepared, bearer, finish_credentials, insert, text, xai};
use crate::{config::Config, home::client::Selection, payload::ApplyContext};
use anyhow::{Result, ensure};
use http::HeaderMap;
use serde_json::{Value, json};

pub(super) fn token(auth: &Value) -> &str {
    [
        "/metadata/access_token",
        "/attributes/access_token",
        "/attributes/api_key",
    ]
    .into_iter()
    .map(|p| text(auth, p))
    .find(|s| !s.is_empty())
    .unwrap_or("")
}

pub(super) fn credentials(selection: &Selection, protocol: &str) -> Result<Credentials> {
    ensure!(
        matches!(protocol, "openai" | "openai-response"),
        "Kimi Claude delegation/thinking replay unsupported"
    );
    let auth = &selection.auth;
    let configured = ["/attributes/base_url", "/metadata/base_url"]
        .into_iter()
        .map(|p| text(auth, p))
        .find(|s| !s.is_empty());
    let domain = ["/attributes/domain", "/metadata/domain"]
        .into_iter()
        .map(|p| text(auth, p))
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let base = configured
        .unwrap_or(
            if domain.eq_ignore_ascii_case("kimi.ai")
                || selection.provider.eq_ignore_ascii_case("kimi-ai")
            {
                "https://api.kimi.ai/coding"
            } else {
                "https://api.kimi.com/coding"
            },
        )
        .trim_end_matches('/');
    let base = if base.ends_with("/v1") {
        base.into()
    } else {
        format!("{base}/v1")
    };
    let mut headers = HeaderMap::new();
    bearer(&mut headers, token(auth))?;
    // Go's buildinfo.Version defaults to dev. Do not substitute the Rust package version.
    for (name, value) in [
        ("user-agent", "CLIProxyAPI/dev"),
        ("x-msh-platform", "CLIProxyAPI"),
        ("x-msh-version", "dev"),
    ] {
        insert(&mut headers, name, value)?;
    }
    let hostname = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".into());
    insert(&mut headers, "x-msh-device-name", &hostname)?;
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    insert(&mut headers, "x-msh-device-model", &format!("{os} {arch}"))?;
    let device = text(auth, "/metadata/device_id");
    insert(
        &mut headers,
        "x-msh-device-id",
        if device.is_empty() {
            "cli-proxy-api-device"
        } else {
            device
        },
    )?;
    finish_credentials(
        selection,
        headers,
        base,
        protocol,
        text(auth, "/attributes/api_key").is_empty(),
    )
}

pub fn normalize_model(model: &str) -> String {
    let model = model.trim().to_ascii_lowercase();
    let base = model
        .split('(')
        .next()
        .unwrap_or("")
        .trim_end_matches("[1m]");
    match base {
        "kimi-k2.8" | "k2.8" | "kimi-k2.8-code" | "k2.8-code" | "kimi-k2.8-preview"
        | "k2.8-preview" | "kimi-k2.7-code" | "k2.7-code" | "kimi-for-coding" | "for-coding" => {
            "kimi-for-coding".into()
        }
        "kimi-k2.7-code-highspeed"
        | "k2.7-code-highspeed"
        | "kimi-for-coding-highspeed"
        | "for-coding-highspeed" => "kimi-for-coding-highspeed".into(),
        _ => base.strip_prefix("kimi-").unwrap_or(base).into(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare(
    config: &Config,
    selection: &Selection,
    path: &str,
    incoming: &HeaderMap,
    mut body: Value,
    requested_stream: bool,
    context: Option<ApplyContext<'_>>,
    creds: Credentials,
) -> Result<Prepared> {
    ensure!(
        matches!(path, "/v1/responses" | "/v1/chat/completions"),
        "Kimi text route unsupported"
    );
    let original = body.clone();
    ensure!(
        !selection.model.contains('('),
        "Kimi model thinking suffix requires provider-specific translation; unsupported"
    );
    if path == "/v1/responses" {
        xai::validate_responses(&body)?;
        ensure!(
            body.get("input")
                .and_then(Value::as_array)
                .is_none_or(|items| items
                    .iter()
                    .all(|item| matches!(text(item, "/type"), "" | "message"))),
            "Kimi tool input ordering/replay unsupported"
        );
    }
    // These controls require the Go tool/replay bridges, not pass-through behavior.
    ensure!(
        body.get("tools")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
            && body.get("functions").is_none(),
        "Kimi tool schema/replay normalization unsupported"
    );
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages.iter() {
            ensure!(
                message.get("tool_calls").is_none()
                    && message.get("function_call").is_none()
                    && text(message, "/role") != "tool",
                "Kimi tool history/reasoning replay unsupported"
            );
        }
        messages.retain(|m| {
            !(text(m, "/role") == "assistant"
                && empty_content(m.get("content"))
                && text(m, "/reasoning_content").is_empty())
        });
    }
    let object = body
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Kimi request must be an object"))?;
    object.insert("model".into(), normalize_model(&selection.model).into());
    object.insert("stream".into(), requested_stream.into());
    let expected = if object
        .get("thinking")
        .is_some_and(|v| text(v, "/type").eq_ignore_ascii_case("disabled"))
    {
        0.6
    } else {
        1.0
    };
    if object
        .get("temperature")
        .is_some_and(|v| v.as_f64() != Some(expected))
    {
        object.remove("temperature");
    }
    if requested_stream && path == "/v1/chat/completions" {
        object.insert("stream_options".into(), json!({"include_usage":true}));
    }
    xai::send(
        config,
        selection,
        path,
        incoming,
        body,
        &original,
        requested_stream,
        context,
        creds,
        if path == "/v1/responses" {
            "openai-response"
        } else {
            "openai"
        },
        false,
    )
}

fn empty_content(content: Option<&Value>) -> bool {
    match content {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        Some(Value::Array(parts)) => parts.iter().all(|p| {
            empty_content(Some(p))
                || p.as_object().is_some_and(|o| {
                    o.is_empty() || (text(p, "/type") == "text" && text(p, "/text").is_empty())
                })
        }),
        _ => false,
    }
}

#[cfg(test)]
#[path = "kimi_tests.rs"]
mod tests;
