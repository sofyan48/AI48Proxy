//! Media-only transport; selected Home credentials are the sole routing authority.
use crate::{config::Config, home::client::Selection, payload::ApplyContext, providers};
use anyhow::{Result, ensure};
use http::{HeaderMap, HeaderValue};
use serde_json::Value;

pub fn compat(selection: &Selection) -> bool {
    matches!(
        selection.provider.as_str(),
        "openai" | "openai-compatibility"
    ) || ["compat_name", "provider_key"].iter().any(|key| {
        selection
            .auth
            .pointer(&format!("/attributes/{key}"))
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    })
}

pub fn text<'a>(value: &'a Value, path: &str) -> &'a str {
    value
        .pointer(path)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
}

fn xai_credentials(selection: &Selection, speech: bool) -> Result<providers::Credentials> {
    let auth = &selection.auth;
    ensure!(
        auth.get("disabled").and_then(Value::as_bool) != Some(true)
            && text(auth, "/status") != "disabled",
        "disabled credential"
    );
    let key = text(auth, "/attributes/api_key");
    let token = if key.is_empty() {
        text(auth, "/metadata/access_token")
    } else {
        key
    };
    let mut base = text(auth, "/attributes/base_url");
    if base.is_empty() {
        base = text(auth, "/metadata/base_url");
    }
    let using_api = auth
        .pointer("/attributes/using_api")
        .or_else(|| auth.pointer("/metadata/using_api"))
        .and_then(|v| {
            v.as_bool().or_else(|| match v.as_str()? {
                "true" | "1" => Some(true),
                "false" | "0" => Some(false),
                _ => None,
            })
        })
        .unwrap_or_else(|| {
            !["/attributes/auth_kind", "/metadata/auth_kind"]
                .iter()
                .any(|p| text(auth, p).eq_ignore_ascii_case("oauth"))
        });
    let official = "https://api.x.ai/v1";
    let cli = "https://cli-chat-proxy.grok.com/v1";
    if speech && (base.is_empty() || base.trim_end_matches('/') == cli) {
        base = official;
    } else if !speech && !using_api && (base.is_empty() || base.trim_end_matches('/') == official) {
        base = cli;
    } else if base.is_empty() {
        base = official;
    }
    // Resolve through the common validator, proxy selection, and custom-header boundary.
    let mut selected = Selection {
        model: selection.model.clone(),
        provider: "openai".into(),
        auth_index: selection.auth_index.clone(),
        user_api_key: selection.user_api_key.clone(),
        auth: selection.auth.clone(),
        model_info: selection.model_info.clone(),
        concurrency: None,
        request_retry: selection.request_retry,
    };
    selected.auth["attributes"] = auth
        .get("attributes")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    selected.auth["attributes"]["base_url"] = base.into();
    selected.auth["attributes"]["api_key"] = token.into();
    providers::resolve(&selected, "openai")
}

pub fn prepare(
    config: &Config,
    selection: &Selection,
    path: &str,
    incoming: &HeaderMap,
    body: &mut Value,
    original: &Value,
    multipart: Option<&super::multipart::Form>,
) -> Result<reqwest::RequestBuilder> {
    let speech = path == "/v1/audio/speech" || path == "/v1/tts";
    ensure!(
        (speech && selection.provider == "xai")
            || (!speech && (selection.provider == "xai" || compat(selection))),
        "unsupported media provider"
    );
    let mut creds = if selection.provider == "xai" {
        xai_credentials(selection, speech)?
    } else {
        providers::resolve(selection, "openai")?
    };
    let mut url = url::Url::parse(&creds.base_url)?;
    let root = url.path().trim_end_matches('/').to_owned();
    let suffix = if speech {
        "tts"
    } else {
        path.trim_start_matches("/v1/")
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
            "unsupported proxy"
        );
        client = client.proxy(reqwest::Proxy::all(proxy)?);
    }
    if !creds.headers.contains_key("user-agent") {
        creds.headers.insert(
            "user-agent",
            incoming
                .get("user-agent")
                .cloned()
                .unwrap_or_else(|| HeaderValue::from_static("cli-proxy-openai-compat")),
        );
    }
    if let Some(form) = multipart {
        *body = form.payload(body);
    }
    let original_view = multipart.map(|form| form.payload(original));
    // Final semantic barrier: only multipart framing/serialization follows.
    config.payload.apply_context(
        body,
        ApplyContext {
            model: &selection.model,
            protocol: "openai",
            from_protocol: if speech {
                "openai-speech"
            } else {
                "openai-image"
            },
            headers: Some(incoming),
            requested_model: original
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(&selection.model),
            root: "",
            original: Some(original_view.as_ref().unwrap_or(original)),
        },
    )?;
    ensure!(
        body.get("stream").and_then(Value::as_bool) != Some(true),
        "image SSE unsupported"
    );
    let (bytes, content_type) = match multipart {
        Some(form) => form.encode(body)?,
        None => (serde_json::to_vec(body)?, "application/json".into()),
    };
    creds
        .headers
        .insert("content-type", HeaderValue::from_str(&content_type)?);
    creds.headers.insert(
        "accept",
        HeaderValue::from_static(if speech { "*/*" } else { "application/json" }),
    );
    Ok(client.build()?.post(url).headers(creds.headers).body(bytes))
}
