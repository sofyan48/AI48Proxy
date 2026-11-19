//! Home-dispatched HTTP gateway with strict native and Chat/Messages execution.
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{OriginalUri, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};

use crate::home::client::HomeClient;

#[path = "gateway/antigravity.rs"]
mod antigravity;
#[path = "gateway/codex.rs"]
mod codex;
#[cfg(test)]
#[path = "gateway/compat_tests.rs"]
mod compat_tests;
#[path = "gateway/cors.rs"]
pub(crate) mod cors;
#[path = "gateway/count_tokens.rs"]
mod count_tokens;
#[path = "gateway/diagnostics.rs"]
mod diagnostics;
#[cfg(test)]
#[path = "gateway/expansion_tests.rs"]
mod expansion_tests;
#[cfg(test)]
#[path = "gateway/google_tests.rs"]
mod google_tests;
#[cfg(test)]
#[path = "gateway/integration_tests.rs"]
mod integration_tests;
#[path = "gateway/lifecycle.rs"]
mod lifecycle;
#[cfg(test)]
#[path = "gateway/provider_matrix_tests.rs"]
mod provider_matrix_tests;
#[path = "gateway/response.rs"]
mod response;
#[path = "gateway/routes.rs"]
mod routes;
#[path = "gateway/session.rs"]
mod session;
#[cfg(test)]
#[path = "gateway/tests.rs"]
mod tests;
#[path = "gateway/upstream.rs"]
pub(crate) mod upstream;
#[path = "gateway/usage.rs"]
mod usage;
#[path = "gateway/websocket.rs"]
mod websocket;

pub fn router(home: Arc<HomeClient>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(generate))
        .route("/v1/completions", post(generate))
        .route("/v1/responses", post(generate).get(websocket::upgrade))
        .route("/v1/responses/compact", post(generate))
        .route("/v1/messages", post(generate))
        .route("/v1/messages/count_tokens", post(generate))
        .route("/v1beta/models/{*action}", post(generate))
        .with_state(home)
        .layer(axum::middleware::from_fn(diagnostics::correlate))
        .layer(axum::middleware::from_fn(cors::cors))
}

async fn health(State(home): State<Arc<HomeClient>>) -> Response {
    if home.ready() && !home.cancellation().is_cancelled() {
        Json(json!({"status":"ready"})).into_response()
    } else {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "not_ready",
            "Home is not ready",
        )
    }
}

async fn models(
    State(home): State<Arc<HomeClient>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Response {
    if !home.ready() || home.cancellation().is_cancelled() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "not_ready",
            "Home is not ready",
        );
    }
    let request = json!({"type":"models", "headers": auth_headers(&headers),
        "query": auth_query(uri.query())});
    let result = home
        .command(vec![b"GET".to_vec(), request.to_string().into_bytes()])
        .await;
    let Ok(bytes) = result.and_then(|frame| frame.bytes()) else {
        return error(
            StatusCode::BAD_GATEWAY,
            "home_error",
            "Home models request failed",
        );
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return error(
            StatusCode::BAD_GATEWAY,
            "home_error",
            "Invalid Home models response",
        );
    };
    if value.get("error").is_some() {
        return home_error(&value);
    }
    match openai_models(&value) {
        Some(models) => Json(models).into_response(),
        None => error(
            StatusCode::BAD_GATEWAY,
            "home_error",
            "Invalid Home models response",
        ),
    }
}

/// Flatten only Home's authenticated model buckets; never read an unscoped registry.
fn openai_models(value: &Value) -> Option<Value> {
    let buckets = value.as_object()?;
    let mut models = std::collections::BTreeMap::new();
    for (provider, entries) in buckets {
        for entry in entries.as_array()? {
            let id = entry.get("id")?.as_str()?.trim();
            if id.is_empty() {
                return None;
            }
            models.entry(id.to_owned()).or_insert_with(|| {
                json!({"id":id, "object":"model",
                    "created":entry.get("created").and_then(Value::as_i64).unwrap_or(0),
                    "owned_by":entry.get("owned_by").and_then(Value::as_str)
                        .filter(|owner| !owner.is_empty()).unwrap_or(provider)})
            });
        }
    }
    Some(json!({"object":"list", "data":models.into_values().collect::<Vec<_>>()}))
}

async fn generate(State(home): State<Arc<HomeClient>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    if !parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|v| v.trim() == "application/json" || v.trim().ends_with("+json"))
        })
    {
        return error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_request",
            "Expected JSON content type",
        );
    }
    let config = home.config();
    let controls = crate::config::runtime_controls::RuntimeControls::from(config.as_ref());
    let length = parts
        .headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let bytes = match controls.read_body(body, length).await {
        Ok(bytes) => bytes,
        Err(crate::config::runtime_controls::BodyReadError::TooLarge) => {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "Request body exceeds configured limit",
            );
        }
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Request body could not be read",
            );
        }
    };
    let body = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Invalid JSON request",
            );
        }
    };
    routes::generate(home, parts.uri, parts.headers, body).await
}

const SESSION_HEADERS: &[&str] = &[
    "session-id",
    "session_id",
    "x-openai-subagent",
    "x-codex-session-id",
    "x-codex-parent-session-id",
    "x-session-id",
    "x-slot-session-id",
    "x-http-session-id",
    "x-parent-session-id",
    "x-claude-code-session-id",
    "x-claude-code-agent-id",
    "conversation-id",
    "x-conversation-id",
];

fn auth_headers(headers: &HeaderMap) -> Value {
    let mut values = serde_json::Map::new();
    for name in ["authorization", "x-api-key", "x-goog-api-key"]
        .into_iter()
        .chain(SESSION_HEADERS.iter().copied())
    {
        let entries: Vec<_> = headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_owned)
            .collect();
        if !entries.is_empty() {
            values.insert(name.into(), json!(entries));
        }
    }
    Value::Object(values)
}

fn auth_query(query: Option<&str>) -> Value {
    let mut values = serde_json::Map::new();
    for (key, value) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        if key == "key" || key == "api_key" {
            values.insert(key.into_owned(), json!(value));
        }
    }
    Value::Object(values)
}

fn home_error(value: &Value) -> Response {
    tracing::warn!(
        event = "home_rejection",
        dispatch_type = diagnostics::DispatchKind::from_value(value).as_str(),
        home_reason = home_reason(value),
        "Home rejected request"
    );
    let kind = value
        .pointer("/error/type")
        .and_then(Value::as_str)
        .unwrap_or("");
    let (status, code, message) = match (kind, home_reason(value)) {
        ("error", "price_not_configured") => (
            StatusCode::SERVICE_UNAVAILABLE,
            "model_price_not_configured",
            "Model price is not configured in Home",
        ),
        ("error", "credits_insufficient") => (
            StatusCode::PAYMENT_REQUIRED,
            "insufficient_credits",
            "Insufficient user credits",
        ),
        ("error", "model_not_found") => (
            StatusCode::NOT_FOUND,
            "model_not_found",
            "Model does not exist in Home",
        ),
        ("error", "invalid_request") => (
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Home rejected dispatch request format",
        ),
        _ => match kind {
            "user_credits_insufficient" => (
                StatusCode::PAYMENT_REQUIRED,
                "insufficient_credits",
                "Insufficient user credits",
            ),
            "user_period_limit_exceeded" => (
                StatusCode::TOO_MANY_REQUESTS,
                "period_limit_exceeded",
                "User period limit exceeded",
            ),
            "model_not_found" => (
                StatusCode::NOT_FOUND,
                "model_not_found",
                "Model does not exist in Home",
            ),
            "no_credentials" | "invalid_credential" | "not_handled" => (
                StatusCode::UNAUTHORIZED,
                "invalid_credential",
                "Invalid or missing API key",
            ),
            "model_not_allowed" | "permission_denied" => (
                StatusCode::FORBIDDEN,
                "permission_denied",
                "Model access denied",
            ),
            "credential_concurrency_exceeded"
            | "credential_model_concurrency_exceeded"
            | "user_concurrency_exceeded" => (
                StatusCode::TOO_MANY_REQUESTS,
                "concurrency_exceeded",
                "Concurrency limit exceeded",
            ),
            "model_cooldown" | "rate_limit_exceeded" => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_exceeded",
                "Model is temporarily rate limited",
            ),
            "auth_not_found" | "auth_unavailable" | "request_retry_exceeded" => (
                StatusCode::SERVICE_UNAVAILABLE,
                "upstream_unavailable",
                "No eligible upstream available",
            ),
            "internal_error" => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "home_error",
                "Home request failed",
            ),
            _ => (StatusCode::BAD_GATEWAY, "home_error", "Home request failed"),
        },
    };
    error(status, code, message)
}

fn home_reason(value: &Value) -> &'static str {
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("");
    match message {
        "invalid request json" => "invalid_request",
        "insufficient user credits" => "credits_insufficient",
        "period limit exceeded" => "period_limit",
        "no auth available" | "auth not found" => "auth_unavailable",
        "runtime not ready" => "runtime_not_ready",
        "missing required credential headers" => "missing_headers",
        "invalid api key" => "invalid_api_key",
        _ if message.starts_with("model ") && message.ends_with(" has no configured price") => {
            "price_not_configured"
        }
        _ if message.starts_with("model ") && message.ends_with(" does not exist") => {
            "model_not_found"
        }
        _ => "unclassified",
    }
}

fn error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    tracing::warn!(
        event = "request_rejected",
        status = status.as_u16(),
        code,
        "gateway returned error"
    );
    (
        status,
        Json(json!({"error":{"type":code,"message":message}})),
    )
        .into_response()
}

// Classify known Home legacy messages without exposing their variable contents.
#[cfg(test)]
mod home_error_contract_tests {
    use super::*;
    #[test]
    fn billing_rejections_preserve_home_semantics_without_messages() {
        assert_eq!(
            home_error(&json!({"error":{"type":"user_credits_insufficient","message":"private"}}))
                .status(),
            StatusCode::PAYMENT_REQUIRED
        );
        assert_eq!(
            home_error(&json!({"error":{"type":"user_period_limit_exceeded","message":"private"}}))
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            home_error(&json!({"error":{"type":"model_not_found","message":"private"}})).status(),
            StatusCode::NOT_FOUND
        );
    }
}
