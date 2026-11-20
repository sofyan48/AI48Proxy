use std::sync::Arc;

use axum::{
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};

use serde_json::{Value, json};
use tracing::Instrument;

use super::{
    auth_headers, auth_query, diagnostics, error, home_error, lifecycle,
    response::{buffered, streaming},
    upstream,
};
use crate::home::client::{HomeClient, Rejection};

#[cfg(test)]
#[path = "feedback_tests.rs"]
mod feedback_tests;

pub(super) async fn generate(
    home: Arc<HomeClient>,
    uri: Uri,
    headers: HeaderMap,
    body: Value,
) -> Response {
    if !home.ready() || home.cancellation().is_cancelled() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "not_ready",
            "Home is not ready",
        );
    }
    if !body.is_object() || body.get("stream").is_some_and(|v| !v.is_boolean()) {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Expected an object with boolean stream",
        );
    }
    let Ok(format) = upstream::format(uri.path()) else {
        return error(
            StatusCode::NOT_IMPLEMENTED,
            "unsupported_capability",
            "Route is not implemented",
        );
    };
    let model = if format == upstream::Format::Gemini {
        uri.path()
            .strip_prefix("/v1beta/models/")
            .and_then(|p| p.split_once(':'))
            .map(|(model, _)| model)
    } else {
        body.get("model").and_then(Value::as_str)
    };
    let Some(model) = model.filter(|m| !m.trim().is_empty()) else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "A model is required",
        );
    };
    let requested_stream = if format == upstream::Format::Gemini {
        uri.path().ends_with(":streamGenerateContent")
    } else {
        body.get("stream").and_then(Value::as_bool).unwrap_or(false)
    };
    let request_id = diagnostics::request_id();
    let config = home.config();
    let mut excluded: Vec<String> = Vec::new();
    // Home's selected limit includes credential overrides, including explicit zero.
    let mut round = 0;
    let mut refreshed_once = false;
    loop {
        let mut request = json!({"type":"auth", "model":model, "count":1,
            "concurrency_protocol":1, "retry_round":round, "excluded_auth_ids":excluded,
            "headers":auth_headers(&headers), "query":auth_query(uri.query())});
        super::session::dispatch_fields(&headers, &body, &mut request);
        let result =
            lifecycle::dispatch(home.clone(), request, request_id.clone(), requested_stream).await;
        let (mut selection, mut attempt) = match result {
            Ok(selected) => selected,
            Err(err) => {
                let kind = err
                    .downcast_ref::<Rejection>()
                    .map(|r| diagnostics::DispatchKind::from_value(&r.0).as_str())
                    .unwrap_or("dispatch_transport");
                tracing::warn!(
                    event = "dispatch_failed",
                    dispatch_type = kind,
                    retry_round = round,
                    "Home dispatch failed"
                );
                if let Some(delay) =
                    cooldown_delay(&err, round, config.request_retry, config.max_retry_interval)
                {
                    tracing::info!(
                        event = "dispatch_retry",
                        retry_round = round,
                        delay_ms = delay.as_millis() as u64,
                        "waiting for Home cooldown"
                    );
                    let cancel = home.cancellation();
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return dispatch_error(&err),
                        _ = tokio::time::sleep(delay) => {},
                    }
                    round += 1;
                    continue;
                }
                return dispatch_error(&err);
            }
        };
        let attempt_span = tracing::info_span!("gateway_attempt", retry_round = round,
            provider = diagnostics::provider(&selection.provider),
            credential_hash = %diagnostics::credential_hash(&selection.auth_index));
        let cancellation = attempt.cancellation();
        tracing::info!(parent: &attempt_span, event = "dispatch_selected", "Home selected upstream");
        if cancellation.is_cancelled() || home.cancellation().is_cancelled() {
            attempt.record.local_failure(503);
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "not_ready",
                "Home membership ended",
            );
        }
        let identity = tokio::select! {
            _ = cancellation.cancelled() => Err(anyhow::anyhow!("membership ended")),
            result = crate::providers::claude_identity::prepare(&home, &mut selection) => result,
        };
        if identity.is_err() {
            tracing::warn!(parent: &attempt_span, event = "prepare_failed", stage = "claude_identity", "Home-backed Claude identity preparation failed");
            attempt.record.local_failure(502);
            return error(
                StatusCode::BAD_GATEWAY,
                "identity_unavailable",
                "Upstream identity preparation failed",
            );
        }
        let caller_api_key = selection.user_api_key.clone();
        let cache_context = || crate::providers::codex::cache::Context {
            execution: String::new(),
            derived: crate::providers::codex::cache::derived_session(
                match uri.path() {
                    "/v1/responses" | "/v1/responses/compact" => "openai-response",
                    "/v1/messages" => "claude",
                    _ => "openai",
                },
                &body,
                &headers,
                &caller_api_key,
            ),
            api_key: caller_api_key.clone(),
        };
        let mut prepared = match crate::providers::codex::cache::with_context(
            cache_context(),
            || {
                upstream::prepare(
                    &config,
                    &selection,
                    uri.path(),
                    &headers,
                    body.clone(),
                    requested_stream,
                )
            },
        ) {
            Ok(prepared) => prepared,
            Err(err) => {
                tracing::warn!(parent: &attempt_span, event = "prepare_failed",
                    stage = "request_prepare", "upstream request preparation failed");
                if let Some(thinking) = err.downcast_ref::<crate::thinking::ThinkingError>() {
                    let (status, code, message) = match thinking {
                        crate::thinking::ThinkingError::Invalid(_) => (
                            StatusCode::BAD_REQUEST,
                            "invalid_thinking",
                            "Invalid reasoning configuration",
                        ),
                        crate::thinking::ThinkingError::Unsupported(_) => (
                            StatusCode::NOT_IMPLEMENTED,
                            "unsupported_thinking",
                            "Reasoning requires supported model metadata and format",
                        ),
                    };
                    attempt.record.local_failure(status.as_u16());
                    return error(status, code, message);
                }
                if let Some(translation) = err.downcast_ref::<crate::protocol::TranslationError>() {
                    let status = match translation {
                        crate::protocol::TranslationError::Invalid(_) => StatusCode::BAD_REQUEST,
                        crate::protocol::TranslationError::Unsupported(_) => {
                            StatusCode::NOT_IMPLEMENTED
                        }
                    };
                    attempt.record.local_failure(status.as_u16());
                    return error(
                        status,
                        "translation_error",
                        "Unsupported or invalid cross-format request",
                    );
                }
                attempt.record.local_failure(501);
                return error(
                    StatusCode::NOT_IMPLEMENTED,
                    "unsupported_capability",
                    "Provider, credentials, proxy, or payload capability is not implemented or invalid",
                );
            }
        };
        attempt.record.stream =
            if prepared.response_mode == crate::providers::ResponseMode::CodexSse {
                requested_stream
            } else {
                prepared.stream
            };
        let mut result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                attempt.record.local_failure(503);
                return error(StatusCode::SERVICE_UNAVAILABLE, "not_ready", "Home membership ended");
            },
            response = prepared.request.send() => response,
        };
        let rules =
            crate::config::execution::resolve_rules(&config, &selection.provider, &selection.auth);
        let mut scoped = observe_result_with_rules(&mut result, &mut attempt, &rules).await;
        if refresh_allowed(scoped, refreshed_once)
            && result
                .as_ref()
                .is_ok_and(|r| r.status() == StatusCode::UNAUTHORIZED)
        {
            tracing::info!(parent: &attempt_span, event = "refresh_requested", upstream_status = 401,
                "requesting Home credential refresh");
            refreshed_once = true;
            // One Home-owned refresh per execution, never a local token endpoint or replay after output.
            let refreshed = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    attempt.record.local_failure(503);
                    return error(StatusCode::SERVICE_UNAVAILABLE, "not_ready", "Home membership ended");
                },
                refreshed = crate::providers::refresh_on_401(&home, &selection, StatusCode::UNAUTHORIZED) => refreshed,
            };
            let refresh_outcome = match &refreshed {
                Ok(Some(_)) => "refreshed",
                Ok(None) => "unavailable",
                Err(_) => "failed",
            };
            tracing::info!(parent: &attempt_span, event = "refresh_result", outcome = refresh_outcome,
                "Home credential refresh result");
            if let Ok(Some(refreshed)) = refreshed {
                drop(result);
                selection.auth = refreshed.auth;
                selection.auth_index = refreshed.auth_index;
                let identity = tokio::select! {
                    _ = cancellation.cancelled() => Err(anyhow::anyhow!("membership ended")),
                    result = crate::providers::claude_identity::prepare(&home, &mut selection) => result,
                };
                if identity.is_err() {
                    attempt.record.local_failure(502);
                    return error(
                        StatusCode::BAD_GATEWAY,
                        "identity_unavailable",
                        "Refreshed upstream identity preparation failed",
                    );
                }
                attempt.refreshed(&selection);
                prepared =
                    match crate::providers::codex::cache::with_context(cache_context(), || {
                        upstream::prepare(
                            &config,
                            &selection,
                            uri.path(),
                            &headers,
                            body.clone(),
                            requested_stream,
                        )
                    }) {
                        Ok(prepared) => prepared,
                        Err(_) => {
                            tracing::warn!(parent: &attempt_span, event = "prepare_failed",
                            stage = "refresh_prepare", "refreshed request preparation failed");
                            attempt.record.local_failure(502);
                            return error(
                                StatusCode::BAD_GATEWAY,
                                "refresh_failed",
                                "Refreshed credential cannot execute request",
                            );
                        }
                    };
                result = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        attempt.record.local_failure(503);
                        return error(StatusCode::SERVICE_UNAVAILABLE, "not_ready", "Home membership ended");
                    },
                    response = prepared.request.send() => response,
                };
                let rules = crate::config::execution::resolve_rules(
                    &config,
                    &selection.provider,
                    &selection.auth,
                );
                scoped = observe_result_with_rules(&mut result, &mut attempt, &rules).await;
            }
        }
        let translation = prepared.translation;
        let response_model = prepared.response_model.clone();
        let retryable = result
            .as_ref()
            .map(|r| retryable(r.status()))
            .unwrap_or(true);
        let credential_id = selection
            .auth
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let cap = config.max_retry_credentials;
        // This decision is strictly before any body read/output. Body failures below
        // never return to dispatch, including SSE failures after response headers.
        let will_retry = rotation_allowed(scoped, retryable)
            && !(result.is_ok()
                && attempt.record.fail_code == "request_scoped"
                && scoped.is_none())
            && can_retry(
                round,
                selection.request_retry,
                credential_id,
                excluded.len(),
                cap,
            );
        match &result {
            Ok(response) => tracing::info!(parent: &attempt_span, event = "upstream_response",
                upstream_status = response.status().as_u16(), retryable, will_retry,
                "upstream response headers received"),
            Err(err) => tracing::warn!(parent: &attempt_span, event = "upstream_transport_failed",
                error_kind = diagnostics::transport_kind(err), will_retry,
                "upstream transport failed"),
        }
        if will_retry {
            // Stop upstream I/O before completing execution, and ACK release before redispatch.
            drop(result);
            let frame = attempt.finish();
            drop(attempt);
            if let Some(frame) = frame
                && home.flush_release(&frame).await.is_err()
            {
                diagnostics::stage("retry_release");
                home.fence();
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "release_failed",
                    "Home release failed",
                );
            }
            if home.cancellation().is_cancelled() {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "not_ready",
                    "Home membership ended",
                );
            }
            excluded.push(credential_id.to_owned());
            // can_retry guarantees round < a u32 limit, so this cannot overflow.
            round += 1;
            continue;
        }
        let Ok(response) = result else {
            attempt.record.local_failure(502);
            return error(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "Upstream connection failed",
            );
        };

        if !response.status().is_success() {
            let status = response.status();
            // Upstream error bodies may echo credentials or trusted URLs; never expose them.
            return super::rejection::response(
                status,
                &attempt.record.provider,
                response.headers(),
            );
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        // Captured before the body is consumed; only filtered names can pass through.
        let upstream_headers = response.headers().clone();
        if prepared.response_mode != crate::providers::ResponseMode::Json {
            // CPA feeds successful Codex responses into its bounded SSE scanner
            // regardless of the media-type header. Terminal validation remains mandatory.
            if !accepts_sse(prepared.response_mode, &content_type) {
                tracing::warn!(parent: &attempt_span, event = "content_type_mismatch", expected = "sse",
                    actual = diagnostics::content_type(&content_type), "unexpected upstream content type");
                attempt.record.local_failure(502);
                return error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    "Expected an upstream SSE response",
                );
            }
            if prepared.response_mode == crate::providers::ResponseMode::CodexSse
                && !requested_stream
            {
                let output = super::codex::buffered(
                    response,
                    attempt,
                    prepared.response_kind,
                    &response_model,
                )
                .instrument(attempt_span.clone())
                .await;
                return super::response::configured(output, &upstream_headers, &config, false);
            }
            let _entered = attempt_span.enter();
            let output = streaming(
                response,
                attempt,
                prepared.format,
                translation,
                response_model,
                prepared.response_mode,
                prepared.response_kind,
            );
            let sse = output.status().is_success();
            return super::response::configured(output, &upstream_headers, &config, sse);
        }
        if !content_type
            .to_ascii_lowercase()
            .starts_with("application/json")
        {
            tracing::warn!(parent: &attempt_span, event = "content_type_mismatch", expected = "json",
                actual = diagnostics::content_type(&content_type), "unexpected upstream content type");
            attempt.record.local_failure(502);
            return error(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "Expected an upstream JSON response",
            );
        }
        let output = buffered(
            response,
            attempt,
            prepared.format,
            translation,
            response_model,
            prepared.response_kind,
        )
        .instrument(attempt_span)
        .await;
        return super::response::configured(output, &upstream_headers, &config, false);
    }
}

#[cfg(test)]
async fn observe_result(
    result: &mut Result<reqwest::Response, reqwest::Error>,
    attempt: &mut lifecycle::Attempt,
) {
    observe_result_with_rules(result, attempt, &[]).await;
}

/// Preserve HTTP attribution before refresh/retry can discard the response.
/// Configured request-scoped rules match the bounded body locally, as CPA does.
async fn observe_result_with_rules(
    result: &mut Result<reqwest::Response, reqwest::Error>,
    attempt: &mut lifecycle::Attempt,
    rules: &[crate::config::execution::RequestScopedRule],
) -> Option<crate::config::execution::ErrorAction> {
    let response = match result {
        Ok(response) => response,
        Err(_) => {
            attempt.record.local_failure(502);
            return None;
        }
    };
    let status = response.status().as_u16();
    attempt.record.upstream_status(status);
    if response.status().is_success() {
        // Headers do not complete an execution. A dropped/incomplete body remains a
        // request-scoped failure until the response consumer proves completion.
        attempt.record.local_failure(status);
        return None;
    }
    let retry_after_seconds = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let cancellation = attempt.cancellation();
    let Some(raw) = super::rejection::raw_error_body(response, &cancellation).await else {
        attempt.record.upstream_failure(status, &Value::Null);
        return None;
    };
    let body = serde_json::from_slice(&raw)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&raw).into_owned()));
    let mut request_scoped = false;
    if attempt.record.provider == "claude" {
        let limit = super::rejection::claude_limit(status, response.headers(), &body);
        request_scoped = limit.scope == crate::providers::claude_limits::LimitScope::Request;
        if let Some(delay) = limit.retry_after {
            let seconds = delay
                .as_secs()
                .saturating_add(u64::from(delay.subsec_nanos() > 0));
            if let Ok(value) = seconds.to_string().parse() {
                response.headers_mut().insert("retry-after", value);
            }
        }
        tracing::warn!(event = "claude_limit", scope = ?limit.scope,
            retry_after_seconds = limit.retry_after.map(|delay| delay.as_secs()),
            "Claude upstream failure classified");
        let category = match body.pointer("/error/type").and_then(Value::as_str) {
            Some("rate_limit_error") => "rate_limit_error",
            Some("overloaded_error") => "overloaded_error",
            Some("authentication_error") => "authentication_error",
            Some("permission_error") => "permission_error",
            Some("invalid_request_error") => "invalid_request_error",
            _ => "unclassified",
        };
        tracing::warn!(
            event = "claude_rejected",
            upstream_status = status,
            error_category = category,
            retry_after_seconds,
            "Claude rejected execution"
        );
    }
    if attempt.record.provider == "antigravity"
        && let Some(detail) =
            crate::providers::antigravity::forbidden_diagnostic(status, body.to_string().as_bytes())
    {
        tracing::warn!(
            event = "antigravity_rejected",
            upstream_status = status,
            provider_code = detail.code,
            provider_status = detail.status,
            provider_reason = detail.reason,
            "Antigravity rejected execution"
        );
    }
    // An unreadable error body does not erase the genuine HTTP failure.
    // Rules see the bounded body text only; it is never returned or logged.
    let action = attempt
        .record
        .upstream_failure_with_rule_bytes(status, &raw, rules);
    if request_scoped && action.is_none() {
        attempt.record.fail_code = "request_scoped".into();
    }
    action
}

// Go tryRefreshAfterUnauthorized leaves every matched rule in control of the
// request, including continue actions. Only unmatched 401s may refresh.
fn refresh_allowed(action: Option<crate::config::execution::ErrorAction>, refreshed: bool) -> bool {
    action.is_none() && !refreshed
}

fn rotation_allowed(
    action: Option<crate::config::execution::ErrorAction>,
    retryable: bool,
) -> bool {
    action.map_or(retryable, |action| !action.stops())
}

fn accepts_sse(mode: crate::providers::ResponseMode, content_type: &str) -> bool {
    mode == crate::providers::ResponseMode::CodexSse
        || content_type
            .to_ascii_lowercase()
            .starts_with("text/event-stream")
}

pub(super) fn dispatch_error(err: &anyhow::Error) -> Response {
    if let Some(rejection) = err.downcast_ref::<Rejection>() {
        return home_error(&rejection.0);
    }
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "home_dispatch_failed",
        "Home dispatch failed",
    )
}

pub(super) fn can_retry(
    round: u32,
    limit: u32,
    credential_id: &str,
    excluded: usize,
    cap: usize,
) -> bool {
    round < limit && !credential_id.is_empty() && (cap == 0 || excluded.saturating_add(1) < cap)
}

pub(super) fn cooldown_delay(
    err: &anyhow::Error,
    round: u32,
    default_limit: u32,
    max: std::time::Duration,
) -> Option<std::time::Duration> {
    let rejection = err.downcast_ref::<Rejection>()?;
    let error = rejection.0.get("error")?;
    if error.get("type")?.as_str()? != "model_cooldown" || !error.get("retryable")?.as_bool()? {
        return None;
    }
    let limit = error
        .get("request_retry")
        .and_then(Value::as_u64)
        .unwrap_or(default_limit as u64);
    if round == u32::MAX || round as u64 >= limit {
        return None;
    }
    let delay = std::time::Duration::from_millis(error.get("retry_after_ms")?.as_u64()?);
    (!delay.is_zero() && delay <= max).then_some(delay)
}

fn retryable(status: StatusCode) -> bool {
    matches!(
        status.as_u16(),
        401 | 403 | 408 | 429 | 500 | 502 | 503 | 504
    )
}

#[cfg(test)]
mod media_type_tests {
    use super::*;
    use crate::providers::ResponseMode;
    #[test]
    fn codex_uses_terminal_validated_parser_instead_of_header_gate() {
        assert!(accepts_sse(ResponseMode::CodexSse, ""));
        assert!(accepts_sse(
            ResponseMode::CodexSse,
            "application/octet-stream"
        ));
        assert!(!accepts_sse(ResponseMode::Sse, ""));
        assert!(!accepts_sse(ResponseMode::Sse, "application/json"));
        assert!(accepts_sse(
            ResponseMode::Sse,
            "text/event-stream; charset=utf-8"
        ));
    }
}
