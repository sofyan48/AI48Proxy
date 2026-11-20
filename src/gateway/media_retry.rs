//! Shared pre-output media refresh/retry ownership, matching the text conductor.
use super::super::{auth_headers, auth_query, diagnostics, error, lifecycle, routes};
use crate::{
    config::Config,
    home::client::{HomeClient, Selection},
};
use axum::{
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use serde_json::{Value, json};
use std::sync::Arc;

// Responses are moved directly to Axum; boxing this private error adds no useful boundary.
#[allow(clippy::too_many_arguments, clippy::result_large_err)]
pub(super) async fn execute(
    home: Arc<HomeClient>,
    config: &Config,
    model: &str,
    original: &Value,
    headers: &HeaderMap,
    uri: &Uri,
    streaming: bool,
    pinned: Option<&str>,
    prepare: impl Fn(&Selection) -> Result<reqwest::RequestBuilder, Response>,
) -> Result<(Selection, lifecycle::Attempt, reqwest::Response), Response> {
    let mut excluded = Vec::<String>::new();
    let mut round = 0;
    let mut refreshed_once = false;
    let request_id = diagnostics::request_id();
    loop {
        let mut dispatch = json!({"type":"auth","model":model,"count":1,"concurrency_protocol":1,
            "retry_round":round,"excluded_auth_ids":excluded,"headers":auth_headers(headers),"query":auth_query(uri.query())});
        super::super::session::dispatch_fields(headers, original, &mut dispatch);
        if let Some(id) = pinned {
            dispatch["pinned_auth_id"] = id.into();
        }
        let (mut selection, mut attempt) = match lifecycle::dispatch(
            home.clone(),
            dispatch,
            request_id.clone(),
            streaming,
        )
        .await
        {
            Ok(selected) => selected,
            Err(err) => {
                if let Some(delay) = routes::cooldown_delay(
                    &err,
                    round,
                    config.request_retry,
                    config.max_retry_interval,
                ) {
                    let cancel = home.cancellation();
                    tokio::select! { biased; _ = cancel.cancelled() => return Err(ended()), _ = tokio::time::sleep(delay) => {} }
                    excluded.clear();
                    round += 1;
                    continue;
                }
                return Err(routes::dispatch_error(&err));
            }
        };
        if pinned.is_some_and(|id| selection.auth.get("id").and_then(Value::as_str) != Some(id)) {
            attempt.record.local_failure(503);
            return Err(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "binding_unavailable",
                "Home did not select the bound video credential",
            ));
        }
        let cancel = attempt.cancellation();
        let mut result;
        let mut scoped;
        loop {
            if pinned.is_some_and(|id| selection.auth.get("id").and_then(Value::as_str) != Some(id))
            {
                attempt.record.local_failure(503);
                return Err(error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "binding_unavailable",
                    "Refresh changed the bound video credential",
                ));
            }
            let request = match prepare(&selection) {
                Ok(request) => request,
                Err(response) => {
                    attempt.record.local_failure(response.status().as_u16());
                    return Err(response);
                }
            };
            result = tokio::select! { biased;
                _ = cancel.cancelled() => { attempt.record.local_failure(503); return Err(ended()); }
                result = request.send() => result,
            };
            scoped = None;
            match &mut result {
                Ok(response) => {
                    let status = response.status();
                    attempt.record.upstream_status(status.as_u16());
                    if status.is_success() {
                        attempt.record.local_failure(status.as_u16());
                    } else {
                        let raw = super::rejection::raw_error_body(response, &cancel).await;
                        let body = raw.as_deref().map_or(Value::Null, |bytes| {
                            serde_json::from_slice(bytes).unwrap_or_else(|_| {
                                Value::String(String::from_utf8_lossy(bytes).into_owned())
                            })
                        });
                        let rules = crate::config::execution::resolve_rules(
                            config,
                            &selection.provider,
                            &selection.auth,
                        );
                        if let Some(raw) = raw {
                            scoped = attempt.record.upstream_failure_with_rule_bytes(
                                status.as_u16(),
                                &raw,
                                &rules,
                            );
                        } else {
                            // Incomplete/oversized bodies must not match a partial prefix.
                            attempt
                                .record
                                .upstream_failure(status.as_u16(), &Value::Null);
                        }
                        if matches!(uri.path(), "/v1/tts" | "/v1/audio/speech")
                            && status == StatusCode::NOT_FOUND
                            && !super::speech::model_unavailable(&body)
                        {
                            attempt.record.fail_code = "request_scoped".into();
                        }
                    }
                }
                Err(_) => attempt.record.local_failure(502),
            }
            if !refreshed_once
                && !scoped.is_some_and(|a| a.stops())
                && result
                    .as_ref()
                    .is_ok_and(|r| r.status() == StatusCode::UNAUTHORIZED)
            {
                refreshed_once = true;
                let refreshed = tokio::select! { biased;
                    _ = cancel.cancelled() => { attempt.record.local_failure(503); return Err(ended()); }
                    result = crate::providers::refresh_on_401(&home, &selection, StatusCode::UNAUTHORIZED) => result,
                };
                if let Ok(Some(refreshed)) = refreshed {
                    selection.auth = refreshed.auth;
                    selection.auth_index = refreshed.auth_index;
                    attempt.refreshed(&selection);
                    drop(result);
                    continue;
                }
            }
            break;
        }
        let credential = selection
            .auth
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let retryable = result.as_ref().map_or(true, |r| {
            matches!(
                r.status().as_u16(),
                401 | 403 | 408 | 429 | 500 | 502 | 503 | 504
            )
        });
        // Pinned retrieval must never rotate to another credential.
        if pinned.is_none()
            && retryable
            && !scoped.is_some_and(|a| a.stops())
            && !(result.is_ok() && attempt.record.fail_code == "request_scoped" && scoped.is_none())
            && routes::can_retry(
                round,
                selection.request_retry,
                &credential,
                excluded.len(),
                config.max_retry_credentials,
            )
        {
            drop(result);
            let release = attempt.finish();
            drop(attempt);
            if let Some(frame) = release
                && home.flush_release(&frame).await.is_err()
            {
                home.fence();
                return Err(error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "release_failed",
                    "Home release failed",
                ));
            }
            if home.cancellation().is_cancelled() {
                return Err(ended());
            }
            excluded.push(credential);
            round += 1;
            continue;
        }
        return match result {
            Ok(response) => Ok((selection, attempt, response)),
            Err(_) => {
                attempt.record.local_failure(502);
                Err(error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    "Media upstream connection failed",
                ))
            }
        };
    }
}
fn ended() -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "not_ready",
        "Home membership ended",
    )
}
