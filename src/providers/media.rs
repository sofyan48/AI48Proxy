//! Media-only transport; selected Home credentials are the sole routing authority.
use crate::{config::Config, home::client::Selection, payload::ApplyContext, providers};
use anyhow::{Result, ensure};
use http::{HeaderMap, HeaderValue};
use serde_json::Value;
#[path = "media_codex.rs"]
pub mod codex;
#[path = "media_video.rs"]
pub mod video;

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
    let video = path.contains("/videos");
    let retrieve = video
        && body.get("request_id").is_some()
        && !matches!(
            path,
            "/v1/videos"
                | "/openai/v1/videos"
                | "/v1/videos/generations"
                | "/v1/videos/edits"
                | "/v1/videos/extensions"
        );
    let codex = selection.provider == "codex";
    let direct = codex && codex::direct(original, &selection.model);
    ensure!(
        !video || selection.provider == "xai",
        "unsupported video provider"
    );
    ensure!(
        (speech && selection.provider == "xai")
            || (!speech && (selection.provider == "xai" || codex || compat(selection))),
        "unsupported media provider"
    );
    let mut creds = if selection.provider == "xai" {
        xai_credentials(selection, speech)?
    } else if codex {
        codex::headers(config, selection, incoming, body, direct)?
    } else {
        providers::resolve(selection, "openai")?
    };
    let mut url = url::Url::parse(&creds.base_url)?;
    let root = url.path().trim_end_matches('/').to_owned();
    let suffix = if video && !retrieve && matches!(path, "/v1/videos" | "/openai/v1/videos") {
        "videos/generations"
    } else if retrieve {
        "videos"
    } else if codex && !direct {
        "responses"
    } else if speech {
        "tts"
    } else {
        path.trim_start_matches("/v1/")
    };
    url.set_path(&format!("{root}/{suffix}"));
    if retrieve {
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid video URL"))?
            .push(
                body["request_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("missing video ID"))?,
            );
    }
    if video && let Some(key) = incoming.get("x-idempotency-key") {
        creds.headers.insert("x-idempotency-key", key.clone());
    }
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
            model: if codex && !direct {
                &config.gpt_image_2_base_model
            } else {
                &selection.model
            },
            protocol: if codex && !direct { "codex" } else { "openai" },
            from_protocol: if speech {
                "openai-speech"
            } else if video {
                "openai-video"
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

    let (bytes, content_type) = match multipart {
        Some(form) => form.encode(body)?,
        None => (serde_json::to_vec(body)?, "application/json".into()),
    };
    creds
        .headers
        .insert("content-type", HeaderValue::from_str(&content_type)?);
    creds.headers.insert(
        "accept",
        HeaderValue::from_static(if codex && !direct {
            "text/event-stream"
        } else if speech {
            "*/*"
        } else if body.get("stream").and_then(Value::as_bool) == Some(true) {
            "text/event-stream"
        } else {
            "application/json"
        }),
    );
    let client = client.build()?;
    if retrieve {
        Ok(client.get(url).headers(creds.headers))
    } else {
        Ok(client.post(url).headers(creds.headers).body(bytes))
    }
}
