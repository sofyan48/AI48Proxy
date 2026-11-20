//! xAI HTTP text execution. Media and WebSockets are intentionally separate.
use super::{
    Credentials, Prepared, ProviderFormat, ResponseMode, bearer, finish_credentials, insert, text,
};
use crate::{config::Config, home::client::Selection, payload::ApplyContext};
use anyhow::{Result, ensure};
use http::HeaderMap;
use serde_json::Value;

const API: &str = "https://api.x.ai/v1";
const CLI: &str = "https://cli-chat-proxy.grok.com/v1";
const CLIENT_VERSION: &str = "1.0.46";

pub(super) fn credentials(selection: &Selection, protocol: &str) -> Result<Credentials> {
    ensure!(
        protocol == "openai-response",
        "xAI Chat translation/response bridge unsupported; native Responses input required"
    );
    let auth = &selection.auth;
    let token = ["/attributes/api_key", "/metadata/access_token"]
        .into_iter()
        .map(|p| text(auth, p))
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let using_api = ["/attributes/using_api", "/metadata/using_api"]
        .into_iter()
        .find_map(|p| {
            auth.pointer(p).and_then(|v| {
                v.as_bool().or_else(|| match v.as_str()?.trim() {
                    "true" | "TRUE" | "True" | "1" | "t" | "T" => Some(true),
                    "false" | "FALSE" | "False" | "0" | "f" | "F" => Some(false),
                    _ => None,
                })
            })
        })
        .unwrap_or_else(|| {
            let kind = text(auth, "/attributes/auth_kind");
            let kind = if kind.is_empty() {
                text(auth, "/metadata/auth_kind")
            } else {
                kind
            };
            !kind.eq_ignore_ascii_case("oauth")
        });
    let configured = ["/attributes/base_url", "/metadata/base_url"]
        .into_iter()
        .map(|p| text(auth, p))
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .trim_end_matches('/');
    let base = if !using_api && (configured.is_empty() || configured == API) {
        CLI
    } else if configured.is_empty() {
        API
    } else {
        configured
    };
    let mut headers = HeaderMap::new();
    bearer(&mut headers, token)?;
    insert(&mut headers, "connection", "Keep-Alive")?;
    insert(
        &mut headers,
        "user-agent",
        &format!("xai-grok-workspace/{CLIENT_VERSION}"),
    )?;
    if !using_api && base == CLI {
        for (name, value) in [
            ("x-xai-token-auth", "xai-grok-cli"),
            ("x-grok-client-version", CLIENT_VERSION),
            ("x-grok-client-identifier", "grok-shell"),
            ("x-authenticateresponse", "authenticate-response"),
        ] {
            insert(&mut headers, name, value)?;
        }
    }
    finish_credentials(selection, headers, base.into(), "xai", !using_api)
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
    mut creds: Credentials,
) -> Result<Prepared> {
    ensure!(
        path == "/v1/responses",
        "xAI compact/Chat route unsupported"
    );
    let original = body.clone();
    validate_responses(&body)?;
    ensure!(
        !selection.model.starts_with("grok-composer-"),
        "xAI isolated composer conversations unsupported"
    );
    normalize_tools(&mut body)?;
    if !creds.headers.contains_key("x-xai-token-auth")
        && !has_custom_header(selection, "user-agent")
        && let Some(value) = incoming.get("user-agent")
    {
        creds.headers.insert("user-agent", value.clone());
    }
    let object = body
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("xAI request must be an object"))?;
    for key in [
        "previous_response_id",
        "prompt_cache_retention",
        "safety_identifier",
        "stream_options",
        "stop",
    ] {
        object.remove(key);
    }
    object.insert("model".into(), selection.model.clone().into());
    object.insert("stream".into(), true.into());
    if !object.contains_key("instructions") {
        object.insert("instructions".into(), "".into());
    }
    send(
        config,
        selection,
        path,
        incoming,
        body,
        &original,
        requested_stream,
        context,
        creds,
        "openai-response",
        true,
    )
}

pub(super) fn has_custom_header(selection: &Selection, name: &str) -> bool {
    selection
        .auth
        .get("attributes")
        .and_then(Value::as_object)
        .is_some_and(|attrs| {
            attrs.keys().any(|k| {
                k.strip_prefix("header:")
                    .is_some_and(|k| k.eq_ignore_ascii_case(name))
            })
        })
}

/// Text and ordinary function calls are supported; opaque replay is not.
pub(super) fn validate_responses(body: &Value) -> Result<()> {
    ensure!(body.is_object(), "Responses request must be an object");
    for key in [
        "previous_response_id",
        "context_management",
        "conversation",
        "generate",
    ] {
        ensure!(
            body.get(key).is_none_or(Value::is_null),
            "Responses continuation/compaction unsupported"
        );
    }
    if let Some(input) = body.get("input") {
        if let Some(items) = input.as_array() {
            for item in items {
                match text(item, "/type") {
                    "function_call" => ensure!(
                        !text(item, "/call_id").is_empty()
                            && !text(item, "/name").is_empty()
                            && item.get("arguments").is_some_and(Value::is_string),
                        "invalid Responses function call"
                    ),
                    "function_call_output" => ensure!(
                        !text(item, "/call_id").is_empty()
                            && item.get("output").is_some_and(Value::is_string),
                        "unsupported Responses function output"
                    ),
                    _ => (),
                }
                ensure!(
                    matches!(
                        text(item, "/type"),
                        "" | "message" | "function_call" | "function_call_output"
                    ),
                    "Responses reasoning/custom-tool replay unsupported"
                );
                if let Some(parts) = item.get("content").and_then(Value::as_array) {
                    for part in parts {
                        ensure!(
                            matches!(text(part, "/type"), "input_text" | "output_text" | "text"),
                            "provider text executor does not support this content type"
                        );
                    }
                }
            }
        } else {
            ensure!(
                input.is_string(),
                "Responses input must be text or messages"
            );
        }
    } else {
        anyhow::bail!("Responses input required; Chat bridge unsupported");
    }
    Ok(())
}

/// Simple function tools need no provider-specific response-name restoration.
pub(super) fn normalize_tools(body: &mut Value) -> Result<()> {
    validate_tools(body, false)?;
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if tool.get("parameters").is_none() {
                tool["parameters"] = serde_json::json!({"type":"object","properties":{}});
            }
            let params = &tool["parameters"];
            ensure!(
                !complex_schema(params),
                "provider tool schema references/unions unsupported"
            );
        }
    }
    Ok(())
}

fn complex_schema(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, v)| {
            matches!(
                key.as_str(),
                "$ref" | "$defs" | "definitions" | "oneOf" | "anyOf" | "allOf"
            ) || complex_schema(v)
        }),
        Value::Array(items) => items.iter().any(complex_schema),
        _ => false,
    }
}

pub(super) fn validate_tools(body: &Value, meta: bool) -> Result<()> {
    ensure!(
        body.get("functions").is_none(),
        "legacy function declarations unsupported"
    );
    if let Some(tools) = body.get("tools") {
        let tools = tools
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("tools must be an array"))?;
        ensure!(
            tools.len() <= if meta { 201 } else { 200 },
            "provider tool limit unsupported"
        );
        for tool in tools {
            if meta && tool == &super::meta::muse_namespace() {
                continue;
            }
            ensure!(
                text(tool, "/type") == "function" && !text(tool, "/name").is_empty(),
                "provider hosted/custom/namespace tools unsupported"
            );
            ensure!(
                text(tool, "/name") != "web_search",
                "xAI client web_search alias restoration unsupported"
            );
            ensure!(
                !tool.get("parameters").is_some_and(complex_schema),
                "provider tool schema references/unions unsupported"
            );
        }
    }
    if let Some(choice) = body.get("tool_choice") {
        ensure!(
            choice
                .as_str()
                .is_some_and(|s| matches!(s, "auto" | "none" | "required"))
                || (text(choice, "/type") == "function" && !text(choice, "/name").is_empty()),
            "provider tool choice unsupported"
        );
    }
    Ok(())
}

/// Shared transport for these executors. All normalization precedes payload rules.
#[allow(clippy::too_many_arguments)]
pub(super) fn send(
    config: &Config,
    selection: &Selection,
    path: &str,
    incoming: &HeaderMap,
    mut body: Value,
    original: &Value,
    _requested_stream: bool,
    context: Option<ApplyContext<'_>>,
    mut creds: Credentials,
    payload_protocol: &str,
    forced_sse: bool,
) -> Result<Prepared> {
    let mut url = url::Url::parse(&creds.base_url)?;
    let root = url.path().trim_end_matches('/');
    let suffix = match path {
        "/v1/responses" => "responses",
        "/v1/chat/completions" => "chat/completions",
        _ => anyhow::bail!("provider text route unsupported"),
    };
    url.set_path(&format!("{root}/{suffix}"));
    let mut client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(30));
    if let Some(proxy) = creds
        .proxy_url
        .as_deref()
        .or(config.proxy_url.as_deref())
        .filter(|p| *p != "direct")
    {
        let parsed =
            url::Url::parse(proxy).map_err(|_| anyhow::anyhow!("invalid provider proxy URL"))?;
        ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "proxy scheme unsupported"
        );
        client = client.proxy(
            reqwest::Proxy::all(proxy).map_err(|_| anyhow::anyhow!("invalid provider proxy"))?,
        );
    }
    let fallback = ApplyContext {
        model: &selection.model,
        protocol: payload_protocol,
        from_protocol: payload_protocol,
        headers: Some(incoming),
        requested_model: original
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&selection.model),
        root: "",
        original: Some(original),
    };
    let context = context
        .map(|ctx| ApplyContext {
            model: &selection.model,
            protocol: payload_protocol,
            root: "",
            ..ctx
        })
        .unwrap_or(fallback);
    config.payload.apply_context(&mut body, context)?;
    // Read-only validation after the final semantic barrier; never restore filtered fields.
    if path == "/v1/responses" {
        validate_responses(&body)?;
    }
    if selection.provider != "kimi" && selection.provider != "kimi-ai" {
        validate_tools(&body, selection.provider == "meta")?;
    } else {
        ensure!(
            body.get("tools")
                .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty))
                && body.get("functions").is_none()
                && body
                    .get("input")
                    .and_then(Value::as_array)
                    .is_none_or(|items| items
                        .iter()
                        .all(|item| matches!(text(item, "/type"), "" | "message"))),
            "Kimi tool normalization/replay unsupported"
        );
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        ensure!(
            messages.iter().all(|m| m.get("tool_calls").is_none()
                && m.get("function_call").is_none()
                && text(m, "/role") != "tool"),
            "provider tool history unsupported after payload rules"
        );
    }
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    ensure!(
        !forced_sse || stream,
        "provider HTTP requires stream=true after payload rules"
    );
    insert(&mut creds.headers, "content-type", "application/json")?;
    insert(
        &mut creds.headers,
        "accept",
        if stream {
            "text/event-stream"
        } else {
            "application/json"
        },
    )?;
    Ok(Prepared {
        request: client.build()?.post(url).headers(creds.headers).json(&body),
        stream,
        format: ProviderFormat::OpenAi,
        response_mode: if forced_sse || (stream && path == "/v1/responses") {
            ResponseMode::CodexSse
        } else if stream {
            ResponseMode::Sse
        } else {
            ResponseMode::Json
        },
    })
}

#[cfg(test)]
#[path = "xai_tests.rs"]
pub(crate) mod tests;
