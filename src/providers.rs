//! Home-selected credentials only: no local OAuth acquisition or refresh.
//! Call `prepare` after protocol translation/thinking, but before payload rules.
//! `resolve` is available for gateways that own request construction themselves.
use anyhow::{Result, bail, ensure};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use url::Url;

use crate::{config::Config, home::client::Selection};

#[path = "providers/antigravity.rs"]
pub mod antigravity;
#[path = "providers/claude.rs"]
pub mod claude;
#[path = "providers/claude_identity.rs"]
pub mod claude_identity;
#[path = "providers/claude_limits.rs"]
pub mod claude_limits;

#[path = "providers/claude_signing.rs"]
pub mod claude_signing;
#[path = "providers/codex.rs"]
pub mod codex;
#[cfg(test)]
#[path = "providers/http_options_tests.rs"]
mod http_options_tests;
#[path = "providers/kimi.rs"]
pub mod kimi;
#[path = "providers/meta.rs"]
pub mod meta;
#[path = "providers/openai.rs"]
pub mod openai;
#[path = "providers/refresh.rs"]
mod refresh;
#[path = "providers/vertex.rs"]
pub mod vertex;
#[path = "providers/xai.rs"]
pub mod xai;
pub use refresh::{RefreshedAuth, refresh, refresh_on_401};
#[cfg(test)]
#[path = "providers/google_tests.rs"]
mod google_tests;
#[cfg(test)]
#[path = "providers/tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderFormat {
    OpenAi,
    Claude,
    Gemini,
}

/// Codex HTTP always returns SSE; nonstream callers must collect response.completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseMode {
    Json,
    Sse,
    CodexSse,
}

/// Deliberately no Debug implementation: headers contain selected-credential secrets.
pub struct Credentials {
    pub base_url: String,
    pub headers: HeaderMap,
    pub provider_protocol: String,
    pub proxy_url: Option<String>,
    pub oauth: bool,
}

pub struct Prepared {
    pub request: reqwest::RequestBuilder,
    /// Upstream transport mode, not the downstream client's requested mode.
    pub stream: bool,
    pub format: ProviderFormat,
    pub response_mode: ResponseMode,
}

pub fn resolve(selection: &Selection, incoming_protocol: &str) -> Result<Credentials> {
    let auth = &selection.auth;
    ensure!(auth.is_object(), "invalid Home credential");
    ensure!(
        auth.get("disabled").and_then(Value::as_bool) != Some(true)
            && text(auth, "/status") != "disabled",
        "Home credential disabled"
    );
    let provider = selection.provider.to_ascii_lowercase();
    let key = text(auth, "/attributes/api_key");
    let token = access_token(auth);
    let mut headers = HeaderMap::new();
    let (default_base, protocol, oauth) = match provider.as_str() {
        "codex" => {
            ensure!(
                matches!(incoming_protocol, "openai-response" | "codex"),
                "Codex HTTP requires Responses input; translate before preparing"
            );
            ensure!(
                text(auth, "/metadata/auth_mode") != "siwc"
                    && text(auth, "/attributes/auth_mode") != "siwc"
                    && auth.pointer("/metadata/siwc").is_none()
                    && !matches!(text(auth, "/attributes/use_siwc"), "true" | "1")
                    && !matches!(text(auth, "/metadata/use_siwc"), "true" | "1")
                    && auth.pointer("/metadata/use_siwc").and_then(Value::as_bool) != Some(true),
                "Codex SIWC is unsupported"
            );
            let secret = if key.is_empty() { token } else { key };
            bearer(&mut headers, secret)?;
            codex::headers(
                &mut headers,
                auth,
                key.is_empty() && text(auth, "/attributes/auth_kind") != "apikey",
            )?;
            (
                "https://chatgpt.com/backend-api/codex",
                "codex",
                key.is_empty() && text(auth, "/attributes/auth_kind") != "apikey",
            )
        }
        "claude" => {
            ensure!(
                incoming_protocol == "claude",
                "Claude requires Messages input"
            );
            let secret = if key.is_empty() { token } else { key };
            let oauth = secret.contains("sk-ant-oat");
            ensure!(!secret.is_empty(), "Claude credential has no usable token");
            let base = text(auth, "/attributes/base_url");
            let first_party = base.is_empty()
                || Url::parse(base)
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_owned))
                    .as_deref()
                    == Some("api.anthropic.com");
            if oauth || !first_party || key.is_empty() {
                bearer(&mut headers, secret)?;
            } else {
                insert(&mut headers, "x-api-key", secret)?;
            }
            insert(&mut headers, "anthropic-version", "2023-06-01")?;
            if oauth {
                claude::headers(&mut headers)?;
            }
            ("https://api.anthropic.com", "claude", oauth)
        }
        "gemini" | "gemini-interactions" => {
            ensure!(
                matches!(incoming_protocol, "gemini" | "openai" | "openai-response"),
                "Gemini provider requires a supported native or translated input"
            );
            ensure!(!key.is_empty(), "Gemini API key missing");
            insert(&mut headers, "x-goog-api-key", key)?;
            if provider == "gemini-interactions" {
                (
                    "https://generativelanguage.googleapis.com",
                    "gemini-interactions",
                    false,
                )
            } else {
                ("https://generativelanguage.googleapis.com", "gemini", false)
            }
        }
        "gemini-cli" => bail!("Gemini CLI is plugin-hosted in current CPA; executor unsupported"),
        "antigravity" => {
            let base = antigravity::credentials(auth, incoming_protocol, &mut headers)?;
            return finish_credentials(selection, headers, base, "antigravity", true);
        }
        "vertex" | "vertexai" => {
            let (base, oauth) = vertex::credentials(selection, incoming_protocol, &mut headers)?;
            return finish_credentials(selection, headers, base, incoming_protocol, oauth);
        }
        "xai" => return xai::credentials(selection, incoming_protocol),
        "kimi" | "kimi-ai" => return kimi::credentials(selection, incoming_protocol),
        "meta" => return meta::credentials(selection, incoming_protocol),
        _ if openai::is_compat(selection) => {
            ensure!(
                matches!(incoming_protocol, "openai" | "openai-response"),
                "OpenAI input required"
            );
            // Compatibility executor reads API-key attributes, not OAuth metadata.
            // Empty API keys are valid for unauthenticated compatible endpoints.
            if !key.is_empty() {
                bearer(&mut headers, key)?;
            }
            let base = text(auth, "/attributes/base_url");
            ensure!(
                !base.is_empty(),
                "OpenAI compatibility provider base URL missing"
            );
            (base, "openai", false)
        }
        _ => bail!("provider executor unsupported"),
    };
    let configured_base = text(auth, "/attributes/base_url");
    let base = if configured_base.is_empty() {
        default_base
    } else {
        configured_base
    };
    finish_credentials(selection, headers, base.to_owned(), protocol, oauth)
}

fn finish_credentials(
    selection: &Selection,
    mut headers: HeaderMap,
    base: String,
    protocol: &str,
    oauth: bool,
) -> Result<Credentials> {
    let auth = &selection.auth;
    validate_base(&base)?;
    custom_headers(&mut headers, auth)?;
    // Native authentication cannot be replaced by incoming or custom headers.
    let proxy = [
        text(auth, "/proxy_url"),
        text(auth, "/attributes/proxy_url"),
    ]
    .into_iter()
    .find(|value| !value.is_empty())
    .map(str::to_owned);
    Ok(Credentials {
        base_url: base,
        headers,
        provider_protocol: protocol.into(),
        proxy_url: proxy,
        oauth,
    })
}

/// Inputs must already be in the selected provider's native protocol.
/// Payload configuration is applied exactly once, after built-in normalization.
pub fn prepare(
    config: &Config,
    selection: &Selection,
    path: &str,
    incoming: &HeaderMap,
    body: Value,
    requested_stream: bool,
) -> Result<Prepared> {
    prepare_context(
        config,
        selection,
        path,
        incoming,
        body,
        requested_stream,
        None,
    )
}

/// Carries the original caller context across protocol translation and normalization.
pub fn prepare_context(
    config: &Config,
    selection: &Selection,
    path: &str,
    incoming: &HeaderMap,
    mut body: Value,
    requested_stream: bool,
    context: Option<crate::payload::ApplyContext<'_>>,
) -> Result<Prepared> {
    let original = body.clone();
    let protocol = match path {
        "/v1/responses" | "/v1/responses/compact" => "openai-response",
        "/v1/chat/completions" | "/v1/completions" => "openai",
        "/v1/messages" => "claude",
        p if p.starts_with("/v1beta/models/")
            && (p.ends_with(":generateContent") || p.ends_with(":streamGenerateContent")) =>
        {
            "gemini"
        }
        _ => bail!("provider route unsupported"),
    };
    ensure!(body.is_object(), "provider request must be an object");
    let mut creds = resolve(selection, protocol)?;
    let is_codex = creds.provider_protocol == "codex";
    let is_openai = openai::is_compat(selection);
    let compact = path == "/v1/responses/compact";
    ensure!(
        !compact || !requested_stream,
        "compact does not support streaming"
    );
    ensure!(
        !compact || is_codex || is_openai,
        "provider compact route unsupported"
    );
    let is_antigravity = creds.provider_protocol == "antigravity";
    let is_vertex = matches!(
        selection.provider.to_ascii_lowercase().as_str(),
        "vertex" | "vertexai"
    );
    let vertex_openapi = if is_vertex {
        let requested = context
            .as_ref()
            .map(|ctx| ctx.requested_model)
            .unwrap_or_else(|| {
                original
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or(&selection.model)
            });
        let openapi = vertex::uses_openapi(selection, requested)?;
        ensure!(
            if openapi {
                protocol == "openai"
            } else {
                protocol == "gemini"
            },
            "Vertex input does not match Home native/OpenAPI routing; translate before preparing"
        );
        openapi
    } else {
        false
    };
    let format = match creds.provider_protocol.as_str() {
        "xai" => {
            return xai::prepare(
                config,
                selection,
                path,
                incoming,
                body,
                requested_stream,
                context,
                creds,
            );
        }
        "meta" => {
            return meta::prepare(
                config,
                selection,
                path,
                incoming,
                body,
                requested_stream,
                context,
                creds,
            );
        }
        "openai" | "openai-response"
            if selection.provider.eq_ignore_ascii_case("kimi")
                || selection.provider.eq_ignore_ascii_case("kimi-ai") =>
        {
            return kimi::prepare(
                config,
                selection,
                path,
                incoming,
                body,
                requested_stream,
                context,
                creds,
            );
        }
        "claude" => ProviderFormat::Claude,
        "gemini" | "antigravity" => ProviderFormat::Gemini,
        _ => ProviderFormat::OpenAi,
    };
    if is_codex {
        if compact {
            ensure!(
                !requested_stream,
                "Codex compact does not support streaming"
            );
            codex::normalize_compact(&mut body, &selection.model)?;
        } else {
            codex::normalize_request(&mut body, &selection.model)?;
        }
        // Read the trusted source before translation, not translated Responses fields
        // or credential metadata. Built-in cache injection precedes payload rules.
        let source = context.as_ref().map_or(protocol, |ctx| ctx.from_protocol);
        let cache_original = context
            .as_ref()
            .and_then(|ctx| ctx.original)
            .unwrap_or(&original);
        if let Some(key) = codex::cache::http_key(
            source,
            cache_original,
            incoming,
            &selection.model,
            &selection.user_api_key,
        ) {
            body["prompt_cache_key"] = key.clone().into();
            codex::prompt_cache_header(
                &mut creds.headers,
                &serde_json::json!({"prompt_cache_key": key}),
            )?;
        }
        codex::request_headers(
            &mut creds.headers,
            incoming,
            config,
            &selection.auth,
            creds.oauth,
        )?;
    } else if format != ProviderFormat::Gemini {
        body["model"] = selection.model.clone().into();
        body["stream"] = requested_stream.into();
    }
    if is_openai {
        ensure!(
            compact || body.get("input").is_none(),
            "OpenAI compatibility requires gateway Responses-to-Chat translation"
        );
        let route = context
            .as_ref()
            .map(|ctx| ctx.requested_model)
            .unwrap_or_else(|| {
                original
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or(&selection.model)
            });
        openai::normalize_request(&mut body, selection, route, compact, requested_stream)?;
        openai::headers(&mut creds.headers, incoming)?;
    }
    if is_antigravity {
        antigravity::normalize(&mut body, selection)?;
        antigravity::obfuscate_sensitive(&mut body, &config.providers.antigravity_sensitive);
    } else if is_vertex {
        vertex::normalize(&mut body, &selection.model, vertex_openapi)?;
    }
    if format == ProviderFormat::Claude {
        if creds.oauth {
            // Operator fallbacks replace the built-in baseline; native caller
            // headers are copied afterwards by prepare_body and still win.
            for (name, value) in config.providers.claude_headers.pairs() {
                if !value.is_empty() {
                    insert(&mut creds.headers, name, value)?;
                }
            }
        }
        // Go resolveClaudeWirePolicy: the global switch only changes the default
        // mode; an explicit credential cloak_mode still wins.
        let mut auth = std::borrow::Cow::Borrowed(&selection.auth);
        if config.providers.disable_claude_cloak_mode
            && text(&selection.auth, "/attributes/cloak_mode")
                .trim()
                .is_empty()
            && text(&selection.auth, "/metadata/cloak_mode")
                .trim()
                .is_empty()
            && auth.is_object()
        {
            let owned = auth.to_mut();
            if !owned.get("attributes").is_some_and(Value::is_object) {
                owned["attributes"] = serde_json::json!({});
            }
            owned["attributes"]["cloak_mode"] = "never".into();
        }
        let timezone = &config.providers.claude_headers.timezone;
        if !timezone.is_empty()
            && timezone.parse::<chrono_tz::Tz>().is_ok()
            && text(&auth, "/attributes/timezone")
                .trim()
                .parse::<chrono_tz::Tz>()
                .is_err()
            && text(&auth, "/metadata/timezone")
                .trim()
                .parse::<chrono_tz::Tz>()
                .is_err()
            && auth.is_object()
        {
            let owned = auth.to_mut();
            if !owned.get("attributes").is_some_and(Value::is_object) {
                owned["attributes"] = serde_json::json!({});
            }
            owned["attributes"]["timezone"] = timezone.as_str().into();
        }
        claude::prepare_body(&mut body, &mut creds.headers, &auth, incoming, creds.oauth)?;
        claude::operator_overrides(
            &mut creds.headers,
            incoming,
            &config.providers.claude_headers,
        )?;
    }
    let url = if is_antigravity {
        antigravity::url(&creds.base_url, requested_stream)?
    } else if is_vertex {
        vertex::url(&creds, selection, vertex_openapi, requested_stream)?
    } else {
        let mut url = Url::parse(&creds.base_url)?;
        let root = url.path().trim_end_matches('/');
        let suffix = match format {
            ProviderFormat::Claude if root.ends_with("/v1") => "messages".to_owned(),
            ProviderFormat::Claude => "v1/messages".to_owned(),
            ProviderFormat::Gemini => {
                ensure!(
                    !selection.model.is_empty()
                        && selection
                            .model
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
                    "unsupported Gemini model name"
                );
                format!(
                    "{}models/{}:{}",
                    if root.ends_with("/v1beta") {
                        ""
                    } else {
                        "v1beta/"
                    },
                    selection.model,
                    if requested_stream {
                        "streamGenerateContent"
                    } else {
                        "generateContent"
                    }
                )
            }
            _ if compact => "responses/compact".into(),
            _ if is_codex => "responses".into(),
            _ if is_openai => "chat/completions".into(),
            _ => path.trim_start_matches("/v1/").into(),
        };
        url.set_path(&format!("{root}/{suffix}"));
        if format == ProviderFormat::Gemini && requested_stream {
            url.query_pairs_mut().append_pair("alt", "sse");
        }
        url
    };
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
            Url::parse(proxy).map_err(|_| anyhow::anyhow!("invalid provider proxy URL"))?;
        ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "proxy scheme unsupported"
        );
        client = client.proxy(
            reqwest::Proxy::all(proxy).map_err(|_| anyhow::anyhow!("invalid provider proxy"))?,
        );
    }
    // Final business-payload mutation. Header negotiation below is read-only with respect to body.
    let fallback = crate::payload::ApplyContext {
        model: &selection.model,
        protocol: if compact {
            "openai-response"
        } else {
            &creds.provider_protocol
        },
        from_protocol: protocol,
        headers: Some(incoming),
        requested_model: original
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&selection.model),
        root: if is_antigravity { "request" } else { "" },
        original: Some(&original),
    };
    let context = context
        .map(|ctx| crate::payload::ApplyContext {
            model: &selection.model,
            protocol: if compact {
                "openai-response"
            } else {
                &creds.provider_protocol
            },
            root: if is_antigravity { "request" } else { ctx.root },
            ..ctx
        })
        .unwrap_or(fallback);
    config.payload.apply_context(&mut body, context)?;
    let stream = if format == ProviderFormat::Gemini {
        requested_stream
    } else {
        body.get("stream").and_then(Value::as_bool).unwrap_or(false)
    };
    ensure!(
        !is_codex || compact || stream,
        "Codex HTTP requires stream=true after payload rules"
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
    ensure!(
        !compact || !stream,
        "compact requires nonstream payload after payload rules"
    );
    let response_mode = if is_codex && !compact {
        ResponseMode::CodexSse
    } else if stream {
        ResponseMode::Sse
    } else {
        ResponseMode::Json
    };
    // CCH is a transport signature over the final bytes, never a business mutation.
    let mut bytes = serde_json::to_vec(&body)?;
    if format == ProviderFormat::Claude && creds.oauth {
        bytes = claude_signing::sign(bytes)?;
    }
    if is_codex && creds.oauth {
        bytes = codex_compress(config, &url, bytes, &mut creds.headers)?;
    }
    let client = match (is_antigravity, config.providers.antigravity_pool) {
        (true, Some(pool)) => pooled_client(
            client,
            pool,
            format!(
                "{}\u{0}{}",
                selection
                    .auth
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("anonymous"),
                creds
                    .proxy_url
                    .as_deref()
                    .or(config.proxy_url.as_deref())
                    .unwrap_or("")
            ),
        )?,
        _ => client.build()?,
    };
    Ok(Prepared {
        request: client.post(url).headers(creds.headers).body(bytes),
        stream,
        format,
        response_mode,
    })
}

/// Per-credential Antigravity pool (Go antigravity transport cache). Bounded so many
/// credentials degrade to short-lived clients instead of growing without limit.
fn pooled_client(
    builder: reqwest::ClientBuilder,
    pool: crate::config::providers::AntigravityPool,
    scope: String,
) -> Result<reqwest::Client> {
    use std::{collections::HashMap, sync::Mutex};
    type Key = (String, crate::config::providers::AntigravityPool);
    static CACHE: std::sync::OnceLock<Mutex<HashMap<Key, reqwest::Client>>> =
        std::sync::OnceLock::new();
    const LIMIT: usize = 1024;
    let cache = CACHE.get_or_init(Default::default);
    let key = (scope, pool);
    if let Some(client) = cache.lock().expect("pool cache").get(&key) {
        return Ok(client.clone());
    }
    let client = builder
        .pool_idle_timeout(pool.idle)
        .pool_max_idle_per_host(pool.max_idle_per_host)
        .http1_only()
        .build()?;
    let mut guard = cache.lock().expect("pool cache");
    // Drop entries from older settings first (config hot reload).
    guard.retain(|(_, settings), _| *settings == pool);
    if guard.len() < LIMIT {
        guard.insert(key, client.clone());
    }
    Ok(client)
}

/// Go codex_executor_request.go: zstd only for ChatGPT Codex OAuth and only when smaller.
/// Pure transport framing after the final payload barrier.
fn codex_compress(
    config: &Config,
    url: &Url,
    bytes: Vec<u8>,
    headers: &mut HeaderMap,
) -> Result<Vec<u8>> {
    let settings = &config.providers;
    if !settings.codex_request_compression
        || bytes.len() < settings.codex_request_compression_min_bytes
        || !url
            .host_str()
            .is_some_and(|h| h.eq_ignore_ascii_case("chatgpt.com"))
        || !url.path().starts_with("/backend-api/codex/")
    {
        return Ok(bytes);
    }
    let compressed = zstd::bulk::compress(&bytes, 3)?;
    if compressed.len() >= bytes.len() {
        return Ok(bytes);
    }
    insert(headers, "content-encoding", "zstd")?;
    Ok(compressed)
}

pub(super) fn text<'a>(auth: &'a Value, pointer: &str) -> &'a str {
    auth.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
}
pub(super) fn access_token(auth: &Value) -> &str {
    [
        "/metadata/access_token",
        "/metadata/accessToken",
        "/metadata/token/access_token",
        "/metadata/token/accessToken",
        "/metadata/Token/access_token",
        "/metadata/Token/accessToken",
    ]
    .into_iter()
    .map(|path| text(auth, path))
    .find(|s| !s.is_empty())
    .unwrap_or("")
}
pub(super) fn insert(headers: &mut HeaderMap, name: &str, value: &str) -> Result<()> {
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| anyhow::anyhow!("invalid provider header name"))?;
    let mut value = HeaderValue::from_str(value)
        .map_err(|_| anyhow::anyhow!("invalid provider header value"))?;
    value.set_sensitive(matches!(
        name.as_str(),
        "authorization" | "x-api-key" | "x-goog-api-key"
    ));
    headers.insert(name, value);
    Ok(())
}
fn bearer(headers: &mut HeaderMap, token: &str) -> Result<()> {
    ensure!(
        !token.is_empty(),
        "selected credential has no usable access token"
    );
    insert(headers, "authorization", &format!("Bearer {token}"))
}
fn validate_base(base: &str) -> Result<()> {
    let url = Url::parse(base).map_err(|_| anyhow::anyhow!("invalid provider base URL"))?;
    ensure!(
        matches!(url.scheme(), "https" | "http")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "invalid provider base URL"
    );
    Ok(())
}
fn custom_headers(headers: &mut HeaderMap, auth: &Value) -> Result<()> {
    if let Some(attrs) = auth.get("attributes").and_then(Value::as_object) {
        for (name, value) in attrs {
            if let Some(name) = name.strip_prefix("header:") {
                ensure!(
                    !matches!(
                        name.to_ascii_lowercase().as_str(),
                        "authorization"
                            | "x-api-key"
                            | "x-goog-api-key"
                            | "host"
                            | "content-length"
                            | "transfer-encoding"
                            | "connection"
                    ),
                    "unsafe credential header override"
                );
                insert(
                    headers,
                    name,
                    value
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("invalid credential header"))?,
                )?;
            }
        }
    }
    Ok(())
}
