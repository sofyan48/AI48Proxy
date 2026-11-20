//! Diagnostic fields are closed categories, never upstream text or request data.
use std::{error::Error, time::Instant};

use axum::{
    extract::{MatchedPath, Request},
    http::HeaderValue,
    middleware::Next,
    response::Response,
};
use sha2::{Digest, Sha256};
use tracing::Instrument;

tokio::task_local! {
    static REQUEST_ID: String;
}

pub(super) fn request_id() -> String {
    REQUEST_ID
        .try_with(Clone::clone)
        .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string())
}

pub(super) async fn correlate(request: Request, next: Next) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    // MatchedPath is a route template, not the URI (which can contain credentials).
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str())
        .unwrap_or("unmatched")
        .to_owned();
    let span = tracing::info_span!("gateway_request", request_id = %request_id, route = %route);
    let header = HeaderValue::from_str(&request_id).expect("UUID is a valid header");
    REQUEST_ID
        .scope(
            request_id,
            async move {
                let started = Instant::now();
                let mut response = next.run(request).await;
                response.headers_mut().insert("x-request-id", header);
                // This measures response headers, not completion of a streaming body.
                tracing::info!(
                    event = "http_response",
                    status = response.status().as_u16(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "gateway response headers"
                );
                response
            }
            .instrument(span),
        )
        .await
}

pub(super) fn provider(value: &str) -> &'static str {
    match value {
        "openai" => "openai",
        "openai-compatibility" => "openai-compatibility",
        "claude" => "claude",
        "gemini" => "gemini",
        "codex" => "codex",
        "antigravity" => "antigravity",
        "vertex" | "vertexai" => "vertex",
        "gemini-cli" => "gemini-cli",
        "qwen" => "qwen",
        "iflow" => "iflow",
        "kiro" => "kiro",
        "github-copilot" => "github-copilot",
        _ => "other",
    }
}

pub(super) fn credential_hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DispatchKind {
    NoCredentials,
    InvalidCredential,
    NotHandled,
    ModelNotAllowed,
    PermissionDenied,
    CredentialConcurrency,
    CredentialModelConcurrency,
    UserConcurrency,
    ModelCooldown,
    RateLimit,
    AuthNotFound,
    AuthUnavailable,
    RetryExceeded,
    Internal,
    Unknown,
}
impl DispatchKind {
    pub fn from_value(value: &serde_json::Value) -> Self {
        match value
            .pointer("/error/type")
            .and_then(serde_json::Value::as_str)
        {
            Some("no_credentials") => Self::NoCredentials,
            Some("invalid_credential") => Self::InvalidCredential,
            Some("not_handled") => Self::NotHandled,
            Some("model_not_allowed") => Self::ModelNotAllowed,
            Some("permission_denied") => Self::PermissionDenied,
            Some("credential_concurrency_exceeded") => Self::CredentialConcurrency,
            Some("credential_model_concurrency_exceeded") => Self::CredentialModelConcurrency,
            Some("user_concurrency_exceeded") => Self::UserConcurrency,
            Some("model_cooldown") => Self::ModelCooldown,
            Some("rate_limit_exceeded") => Self::RateLimit,
            Some("auth_not_found") => Self::AuthNotFound,
            Some("auth_unavailable") => Self::AuthUnavailable,
            Some("request_retry_exceeded") => Self::RetryExceeded,
            Some("internal_error") => Self::Internal,
            _ => Self::Unknown,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoCredentials => "no_credentials",
            Self::InvalidCredential => "invalid_credential",
            Self::NotHandled => "not_handled",
            Self::ModelNotAllowed => "model_not_allowed",
            Self::PermissionDenied => "permission_denied",
            Self::CredentialConcurrency => "credential_concurrency_exceeded",
            Self::CredentialModelConcurrency => "credential_model_concurrency_exceeded",
            Self::UserConcurrency => "user_concurrency_exceeded",
            Self::ModelCooldown => "model_cooldown",
            Self::RateLimit => "rate_limit_exceeded",
            Self::AuthNotFound => "auth_not_found",
            Self::AuthUnavailable => "auth_unavailable",
            Self::RetryExceeded => "request_retry_exceeded",
            Self::Internal => "internal_error",
            Self::Unknown => "unknown",
        }
    }
}

pub(super) fn transport_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        return "timeout";
    }
    // reqwest has no is_tls flag. Inspect typed causes, never Display/debug strings.
    let mut cause: Option<&(dyn Error + 'static)> = Some(error);
    while let Some(error) = cause {
        if error.is::<rustls::Error>() {
            return "tls";
        }
        cause = error.source();
    }
    if error.is_connect() {
        "connect"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else if error.is_builder() {
        "request_build"
    } else {
        "transport"
    }
}

pub(super) fn content_type(value: &str) -> &'static str {
    let value = value.to_ascii_lowercase();
    if value.starts_with("text/event-stream") {
        "sse"
    } else if value.starts_with("application/json") {
        "json"
    } else if value.is_empty() {
        "missing"
    } else {
        "other"
    }
}

/// Read at most 64 KiB of an error body. No timeout after connection establishment.
/// Never return a prefix of truncated JSON or log any of these bytes.
pub(super) async fn error_body(
    response: &mut reqwest::Response,
    cancellation: &tokio_util::sync::CancellationToken,
) -> serde_json::Value {
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return serde_json::Value::Null,
            chunk = response.chunk() => chunk,
        };
        match chunk {
            Ok(Some(chunk)) if chunk.len() <= (64 * 1024usize).saturating_sub(bytes.len()) => {
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => {
                return serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                    // Used only by canonical scheduler matchers, never returned or logged.
                    String::from_utf8(bytes)
                        .map(serde_json::Value::String)
                        .unwrap_or_default()
                });
            }
            _ => return serde_json::Value::Null,
        }
    }
}

/// Only structured, known provider failure codes establish upstream attribution.
/// The typed error carries already-sanitized scheduler hints, not provider text.
#[derive(Debug)]
pub(super) struct UpstreamFailure {
    pub status: u16,
    pub body: serde_json::Value,
}
impl std::fmt::Display for UpstreamFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("upstream failure event")
    }
}
impl Error for UpstreamFailure {}

pub(super) fn event_failure(event: &serde_json::Value, provider: &str) -> Option<UpstreamFailure> {
    let body = if event.get("error").is_some() {
        event
    } else {
        event.get("response")?
    };
    let error = body.get("error")?;
    let code = error
        .get("code")
        .and_then(serde_json::Value::as_str)
        .or_else(|| error.get("type").and_then(serde_json::Value::as_str))
        .unwrap_or("");
    let status = match code {
        "usage_limit_reached"
        | "rate_limit_exceeded"
        | "rate_limit_error"
        | "RESOURCE_EXHAUSTED" => 429,
        "server_error" | "internal_error" | "api_error" | "INTERNAL" => 500,
        "overloaded_error" | "UNAVAILABLE" => 503,
        "authentication_error" | "UNAUTHENTICATED" => 401,
        "permission_error" | "PERMISSION_DENIED" => 403,
        _ => match error.get("code").and_then(serde_json::Value::as_u64) {
            Some(status @ (400 | 401 | 403 | 408 | 429 | 500 | 502 | 503 | 504)) => status as u16,
            _ => return None,
        },
    };
    let mut record = crate::reporting::UsageRecord {
        provider: provider.into(),
        ..Default::default()
    };
    record.upstream_failure(status, body);
    Some(UpstreamFailure {
        status,
        body: serde_json::from_str(&record.fail_body).unwrap_or_default(),
    })
}

pub(super) fn processing_failure(
    record: &mut crate::reporting::UsageRecord,
    error: &anyhow::Error,
    status: u16,
) {
    if let Some(failure) = error.downcast_ref::<UpstreamFailure>() {
        record.upstream_failure(failure.status, &failure.body);
    } else {
        record.local_failure(status);
    }
}

pub(super) fn stage(stage: &'static str) {
    tracing::warn!(
        event = "response_failure",
        stage,
        "gateway response processing failed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, http::Request, routing::get};
    use tower::ServiceExt;

    #[test]
    fn rejection_and_identity_fields_are_closed() {
        let malicious = "https://secret:token@example.invalid/?key=private\nFORGED";
        let value = serde_json::json!({"error":{"type":malicious,"message":malicious}});
        assert_eq!(DispatchKind::from_value(&value), DispatchKind::Unknown);
        assert_eq!(DispatchKind::from_value(&value).as_str(), "unknown");
        assert_eq!(provider(malicious), "other");
        let known = serde_json::json!({"error":{"type":"model_cooldown","message":malicious}});
        assert_eq!(DispatchKind::from_value(&known).as_str(), "model_cooldown");
        let hash = credential_hash(malicious);
        assert_eq!(hash.len(), 64);
        assert!(!hash.contains("secret"));
    }

    #[derive(Clone)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn emitted_logs_scrub_malicious_home_and_sse_errors() {
        let capture = Capture(Default::default());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(capture.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let malicious = "PRIVATE_TOKEN https://user:password@private.invalid/?key=SECRET";
            let response = super::super::home_error(&serde_json::json!({
                "error":{"type":malicious,"message":malicious}
            }));
            assert_eq!(response.status(), axum::http::StatusCode::BAD_GATEWAY);
            let event = format!(
                "data: {{\"type\":\"error\",\"error\":{{\"message\":\"{malicious}\"}}}}\n\n"
            );
            let mut usage = super::super::usage::Usage::new(super::super::upstream::Format::OpenAi);
            assert!(
                super::super::codex::Events::new()
                    .feed(event.as_bytes(), &mut usage)
                    .is_err()
            );
            assert!(
                super::super::antigravity::Events::default()
                    .feed(event.as_bytes(), &mut usage)
                    .is_err()
            );
        });
        let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("home_rejection"));
        assert!(logs.contains("unknown"));
        assert!(logs.contains("codex_sse_parse"));
        assert!(logs.contains("antigravity_sse_parse"));
        for secret in [
            "PRIVATE_TOKEN",
            "https://",
            "password",
            "private.invalid",
            "SECRET",
        ] {
            assert!(!logs.contains(secret), "diagnostic leaked {secret}");
        }
    }

    #[tokio::test]
    async fn reqwest_url_and_error_text_never_become_categories() {
        let error = reqwest::Client::new()
            .get("http://[invalid/?key=private")
            .build()
            .unwrap_err();
        assert_eq!(transport_kind(&error), "request_build");
    }

    #[tokio::test]
    async fn every_response_gets_a_fresh_id_even_rejections_and_fallbacks() {
        let app = Router::new()
            .route("/ok", get(|| async { request_id() }))
            .route(
                "/reject",
                get(|| async { axum::http::StatusCode::TOO_MANY_REQUESTS }),
            )
            .layer(axum::middleware::from_fn(correlate));
        let mut ids = std::collections::HashSet::new();
        for path in ["/ok?key=private", "/reject", "/missing", "/ok"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("x-request-id", "arbitrary-secret-client-id")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let id = response.headers()["x-request-id"]
                .to_str()
                .unwrap()
                .to_owned();
            assert!(uuid::Uuid::parse_str(&id).is_ok());
            assert!(ids.insert(id.clone()));
            if path.starts_with("/ok") {
                let body = axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap();
                assert_eq!(body.as_ref(), id.as_bytes());
            }
        }
    }
}
