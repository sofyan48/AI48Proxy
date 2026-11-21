//! Encode only after providers::prepare's final semantic barrier.
use crate::{config::Config, home::client::Selection, providers};
use anyhow::{Result, ensure};
use axum::http::{HeaderMap, HeaderValue};
use serde_json::Value;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[cfg(test)]
#[path = "ws_transport_tests.rs"]
mod tests;

pub(super) struct Prepared {
    pub handshake: http::Request<()>,
    pub message: String,

    pub native: bool,
}

pub(super) fn prepare(
    config: &Config,
    selection: &Selection,
    headers: &HeaderMap,
    body: Value,
) -> Result<Prepared> {
    let credentials = providers::resolve(selection, "openai-response")?;
    ensure!(
        matches!(
            credentials.provider_protocol.as_str(),
            "codex" | "openai-response"
        ),
        "native Responses websocket required"
    );
    let codex = credentials.provider_protocol == "codex";
    let native = !codex || providers::codex::websocket::enabled(&selection.auth);
    // HTTP fallback uses reqwest's configured proxy; native WS must fail closed.
    ensure!(
        !native
            || credentials
                .proxy_url
                .as_deref()
                .or(config.proxy_url.as_deref())
                .is_none_or(|proxy| proxy == "direct"),
        "websocket transport proxy unsupported"
    );
    let request = if codex {
        providers::codex::websocket::prepare(config, selection, headers, body)?
    } else {
        providers::prepare(config, selection, "/v1/responses", headers, body, true)?
            .request
            .build()?
    };
    let bytes = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .ok_or_else(|| anyhow::anyhow!("final request body unavailable"))?;
    let mut business: Value = serde_json::from_slice(bytes)?;
    let object = business
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("final payload must be an object"))?;
    // `type` is transport framing, not a business field. Reject conflicting rules.
    ensure!(
        object
            .get("type")
            .is_none_or(|v| v.as_str() == Some("response.create")),
        "conflicting websocket event type"
    );
    object.insert("type".into(), "response.create".into());
    let message = serde_json::to_string(&business)?;
    let mut url = request.url().clone();
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        _ => anyhow::bail!("invalid upstream scheme"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid websocket URL"))?;
    let mut handshake = url.as_str().into_client_request()?;
    for (name, value) in request.headers() {
        if !matches!(
            name.as_str(),
            "host" | "connection" | "upgrade" | "content-type" | "content-length" | "accept"
        ) && !name.as_str().starts_with("sec-websocket-")
        {
            handshake.headers_mut().insert(name.clone(), value.clone());
        }
    }
    if !handshake.headers().contains_key("openai-beta") {
        handshake.headers_mut().insert(
            "openai-beta",
            HeaderValue::from_static("responses_websockets=2026-02-06"),
        );
    }
    if codex && config.providers.codex_websocket_compression {
        // The adapter consumes this offer and lets yawc validate negotiation.
        handshake.headers_mut().insert(
            "sec-websocket-extensions",
            HeaderValue::from_static(
                "permessage-deflate; server_no_context_takeover; client_no_context_takeover",
            ),
        );
    }
    Ok(Prepared {
        handshake,
        message,

        native,
    })
}
