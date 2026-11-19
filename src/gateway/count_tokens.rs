//! Native Claude API-key counting only; no local estimates or OAuth normalization claims.
use anyhow::{Result, ensure};
use axum::http::HeaderMap;
use serde_json::Value;

use super::upstream::{Format, Prepared, ResponseKind};
use crate::{config::Config, home::client::Selection, providers};

pub(super) fn prepare(
    config: &Config,
    selection: &Selection,
    headers: &HeaderMap,
    mut body: Value,
    requested_stream: bool,
) -> Result<Prepared> {
    ensure!(body.is_object(), "counting requires an object");
    ensure!(
        selection
            .auth
            .pointer("/attributes/api_key")
            .and_then(Value::as_str)
            .is_some_and(|key| !key.trim().is_empty()),
        "counting requires an API key"
    );
    ensure!(
        selection.provider.eq_ignore_ascii_case("claude"),
        "counting requires Claude"
    );
    ensure!(!requested_stream, "counting does not support streaming");
    ensure!(
        body.get("messages").is_some_and(Value::is_array),
        "counting requires messages"
    );
    let mut creds = providers::resolve(selection, "claude")?;
    ensure!(!creds.oauth, "OAuth count normalization unsupported");
    let original = body.clone();
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&selection.model)
        .to_owned();
    body["model"] = selection.model.clone().into();
    // These are generation-only fields, removed before the final user rule barrier.
    for field in ["stream", "max_tokens"] {
        body.as_object_mut().unwrap().remove(field);
    }
    if let Some(beta) = headers.get("anthropic-beta") {
        creds.headers.insert("anthropic-beta", beta.clone());
    }
    let mut url = url::Url::parse(&creds.base_url)?;
    let root = url.path().trim_end_matches('/');
    let suffix = if root.ends_with("/v1") {
        "messages/count_tokens"
    } else {
        "v1/messages/count_tokens"
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
        let parsed = url::Url::parse(proxy)?;
        ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "proxy scheme unsupported"
        );
        client = client.proxy(reqwest::Proxy::all(proxy)?);
    }
    config.payload.apply_context(
        &mut body,
        crate::payload::ApplyContext {
            model: &selection.model,
            protocol: "claude",
            from_protocol: "claude",
            headers: Some(headers),
            requested_model: &requested_model,
            root: "",
            original: Some(&original),
        },
    )?;
    for name in super::SESSION_HEADERS {
        for value in headers.get_all(*name) {
            creds.headers.append(*name, value.clone());
        }
    }
    creds
        .headers
        .insert("content-type", "application/json".parse()?);
    creds.headers.insert("accept", "application/json".parse()?);
    Ok(Prepared {
        request: client.build()?.post(url).headers(creds.headers).json(&body),
        stream: false,
        response_mode: providers::ResponseMode::Json,
        format: Format::Claude,
        translation: None,
        response_model: requested_model,
        response_kind: ResponseKind::CountTokens,
    })
}
