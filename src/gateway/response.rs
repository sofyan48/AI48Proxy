use super::{diagnostics, error, lifecycle, upstream, usage::Usage};
use axum::{
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use serde_json::Value;
use std::{io, sync::Arc};
use tracing::Instrument;
const MAX_RESPONSE: usize = 32 * 1024 * 1024;

/// Route owners call this after translation for both JSON and SSE responses.
/// The original upstream headers must be captured before consuming the response.
pub(super) fn configured(
    mut response: Response,
    headers: &http::HeaderMap,
    config: &crate::config::Config,
    sse: bool,
) -> Response {
    if config.passthrough_headers {
        crate::config::keepalive::passthrough(response.headers_mut(), headers);
    }
    if sse {
        crate::config::keepalive::body(
            response,
            config.streaming.keepalive_interval,
            true,
            config.streaming.send_keepalive_before_ft,
        )
    } else {
        response
    }
}

#[cfg(test)]
#[path = "google_translation_response_tests.rs"]
mod google_translation_response_tests;

pub(super) fn decode_json(value: Value, kind: upstream::ResponseKind) -> anyhow::Result<Value> {
    match kind {
        upstream::ResponseKind::Antigravity
        | upstream::ResponseKind::GeminiChat {
            antigravity: true, ..
        } => crate::providers::antigravity::unwrap_response(&value),
        upstream::ResponseKind::CountTokens => {
            anyhow::ensure!(
                value.get("error").is_none()
                    && value.get("input_tokens").and_then(Value::as_u64).is_some(),
                "invalid count response"
            );
            Ok(value)
        }
        upstream::ResponseKind::Native
        | upstream::ResponseKind::ResponsesChat
        | upstream::ResponseKind::ResponsesChatUsage
        | upstream::ResponseKind::ChatResponses
        | upstream::ResponseKind::GeminiChat {
            antigravity: false, ..
        } => Ok(value),
    }
}

pub(super) async fn buffered(
    response: reqwest::Response,
    mut attempt: lifecycle::Attempt,
    format: upstream::Format,
    translation: Option<(crate::protocol::Format, crate::protocol::Format)>,
    model: String,
    response_kind: upstream::ResponseKind,
) -> Response {
    let status = response.status();
    let mut source = response.bytes_stream();
    let cancellation = attempt.cancellation();
    let mut bytes = Vec::new();
    loop {
        let next = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return error(StatusCode::SERVICE_UNAVAILABLE,
                "not_ready", "Home membership ended"),
            next = source.next() => next,
        };
        match next {
            Some(Ok(chunk)) if bytes.len().saturating_add(chunk.len()) <= MAX_RESPONSE => {
                if bytes.is_empty() {
                    attempt.first_byte();
                }
                bytes.extend_from_slice(&chunk);
            }
            Some(result) => {
                match result {
                    Err(ref err) => tracing::warn!(
                        event = "response_failure",
                        stage = "json_body",
                        error_kind = diagnostics::transport_kind(err),
                        "upstream body read failed"
                    ),
                    Ok(_) => diagnostics::stage("json_body_limit"),
                }
                attempt.record.status_code = 502;
                return error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    "Upstream response failed or exceeded the limit",
                );
            }
            None => break,
        }
    }
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        diagnostics::stage("json_parse");
        attempt.record.status_code = 502;
        return error(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "Invalid upstream JSON response",
        );
    };
    let value = match decode_json(value, response_kind) {
        Ok(value) => value,
        Err(_) => {
            diagnostics::stage("json_decode");
            attempt.record.failed = true;
            attempt.record.status_code = 502;
            return error(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "Invalid upstream response",
            );
        }
    };
    let mut usage = Usage::new(format);
    // A count is not inference usage; do not bill the returned input_tokens estimate.
    if response_kind != upstream::ResponseKind::CountTokens {
        usage.json(&value);
    }
    attempt.record.breakdown = usage.breakdown();
    attempt.record.failed = usage.failed;
    if matches!(
        response_kind,
        upstream::ResponseKind::ChatResponses | upstream::ResponseKind::GeminiChat { .. }
    ) {
        let translated = if matches!(response_kind, upstream::ResponseKind::GeminiChat { .. }) {
            crate::protocol::gemini::chat_response(&value, &model)
        } else {
            crate::protocol::responses::responses_response(&value, &model)
                .map_err(anyhow::Error::from)
        };
        return match translated {
            Ok(value) => (status, axum::Json(value)).into_response(),
            Err(_) => {
                diagnostics::stage("chat_responses_translate");
                attempt.record.failed = true;
                attempt.record.status_code = 502;
                error(
                    StatusCode::BAD_GATEWAY,
                    "translation_error",
                    "Invalid Chat completion",
                )
            }
        };
    }
    if let Some((from, to)) = translation {
        match crate::protocol::translate_response(
            from,
            to,
            &value,
            crate::protocol::ResponseOptions {
                target_model: &model,
                created: chrono::Utc::now().timestamp().max(0) as u64,
            },
        ) {
            Ok(value) => return (status, axum::Json(value)).into_response(),
            Err(_) => {
                diagnostics::stage("json_translate");
                attempt.record.failed = true;
                attempt.record.status_code = 502;
                return error(
                    StatusCode::BAD_GATEWAY,
                    "translation_error",
                    "Unsupported or invalid upstream response",
                );
            }
        }
    }
    (status, axum::Json(value)).into_response()
}

pub(super) fn streaming(
    response: reqwest::Response,
    attempt: lifecycle::Attempt,
    format: upstream::Format,
    translation: Option<(crate::protocol::Format, crate::protocol::Format)>,
    model: String,
    response_mode: crate::providers::ResponseMode,
    response_kind: upstream::ResponseKind,
) -> Response {
    let status = response.status();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, io::Error>>(1);
    let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let producer_completed = completed.clone();
    let downstream_complete = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut attempt = attempt;
    attempt.downstream_complete = Some(downstream_complete.clone());
    let (attempt_tx, attempt_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(
        async move {
            let started = std::time::Instant::now();
            let mut attempt = attempt;
            let mut source = response.bytes_stream();
            let cancellation = attempt.cancellation();
            let mut usage = Usage::new(format);
            let mut codex =
                (response_mode == crate::providers::ResponseMode::CodexSse).then(|| {
                    if response_kind.responses_chat() {
                        super::codex::Events::for_chat()
                    } else {
                        super::codex::Events::new()
                    }
                });
            let mut antigravity = matches!(
                response_kind,
                upstream::ResponseKind::Antigravity
                    | upstream::ResponseKind::GeminiChat {
                        antigravity: true,
                        ..
                    }
            )
            .then(super::antigravity::Events::default);
            let mut gemini_chat = match response_kind {
                upstream::ResponseKind::GeminiChat { include_usage, .. } => Some(
                    crate::protocol::gemini::StreamTranslator::new(&model, include_usage),
                ),
                _ => None,
            };
            let mut responses_chat = response_kind.responses_chat().then(|| {
                crate::protocol::responses::StreamTranslator::new(
                    &model,
                    response_kind == upstream::ResponseKind::ResponsesChatUsage,
                )
            });
            let mut chat_responses = (response_kind == upstream::ResponseKind::ChatResponses)
                .then(|| crate::protocol::responses::ChatStreamTranslator::new(&model));
            let mut translator = translation.map(|(from, _)| {
                crate::protocol::StreamTranslator::new(
                    from,
                    &model,
                    chrono::Utc::now().timestamp().max(0) as u64,
                )
            });
            loop {
                let next = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        diagnostics::stage("sse_cancelled");
                        attempt.record.status_code = 503;

                        break;
                    }
                    _ = tx.closed() => {
                        diagnostics::stage("sse_downstream_closed");
                        break;
                    },
                    next = source.next() => next,
                };
                let bytes = match next {
                    Some(Ok(bytes)) => bytes,
                    Some(Err(err)) => {
                        tracing::warn!(
                            event = "response_failure",
                            stage = "sse_body",
                            error_kind = diagnostics::transport_kind(&err),
                            "upstream stream read failed"
                        );
                        attempt.record.status_code = 502;

                        break;
                    }
                    None => {
                        // Flush framing before translation: Cloud Code may omit the final
                        // blank line, and Gemini normally omits the [DONE] sentinel.
                        let flushed = (|| -> anyhow::Result<Vec<u8>> {
                            let tail = if let Some(events) = &mut antigravity {
                                events.feed(b"\n\n", &mut usage)?
                            } else {
                                usage.feed(b"\n\n");
                                Vec::new()
                            };
                            if let Some(translator) = &mut gemini_chat {
                                let mut output = translator.feed(&tail)?;
                                output.extend(translator.finish()?);
                                Ok(output)
                            } else {
                                Ok(tail)
                            }
                        })();
                        let flush_failed = match flushed {
                            Ok(bytes) if !bytes.is_empty() => {
                                tokio::select! {
                                    biased;
                                    _ = cancellation.cancelled() => true,
                                    result = tx.send(Ok(Bytes::from(bytes))) => result.is_err(),
                                }
                            }
                            Ok(_) => false,
                            Err(_) => true,
                        };
                        attempt.record.failed = flush_failed
                            || usage.failed
                            || translator.as_ref().is_some_and(|t| t.finish().is_err())
                            || responses_chat.as_ref().is_some_and(|t| t.finish().is_err())
                            || chat_responses.as_ref().is_some_and(|t| t.finish().is_err())
                            || codex.as_ref().is_some_and(|events| !events.complete())
                            || antigravity
                                .as_ref()
                                .is_some_and(|events| !events.complete());
                        if attempt.record.failed {
                            diagnostics::stage("sse_finish");
                            attempt.record.status_code = 502;
                        }
                        producer_completed
                            .store(!attempt.record.failed, std::sync::atomic::Ordering::Release);
                        break;
                    }
                };
                attempt.first_byte();
                let bytes = if let Some(events) = &mut antigravity {
                    match events.feed(&bytes, &mut usage) {
                        Ok(bytes) => Bytes::from(bytes),
                        Err(_) => {
                            attempt.record.status_code = 502;
                            attempt.record.failed = true;
                            break;
                        }
                    }
                } else {
                    bytes
                };
                if let Some(events) = &mut codex {
                    if events.feed(&bytes, &mut usage).is_err() {
                        attempt.record.status_code = 502;
                        attempt.record.failed = true;
                        break;
                    }
                } else if antigravity.is_none() {
                    usage.feed(&bytes);
                }
                let bytes = if let Some(translator) = &mut gemini_chat {
                    match translator.feed(&bytes) {
                        Ok(bytes) => Bytes::from(bytes),
                        Err(_) => {
                            diagnostics::stage("gemini_chat_sse_translate");
                            attempt.record.failed = true;
                            attempt.record.status_code = 502;
                            break;
                        }
                    }
                } else if let Some(translator) = &mut responses_chat {
                    match translator.feed(&bytes) {
                        Ok(bytes) => Bytes::from(bytes),
                        Err(_) => {
                            diagnostics::stage("responses_chat_sse_translate");
                            attempt.record.failed = true;
                            attempt.record.status_code = 502;
                            break;
                        }
                    }
                } else if let Some(translator) = &mut chat_responses {
                    match translator.feed(&bytes) {
                        Ok(bytes) => Bytes::from(bytes),
                        Err(_) => {
                            diagnostics::stage("chat_responses_sse_translate");
                            attempt.record.failed = true;
                            attempt.record.status_code = 502;
                            break;
                        }
                    }
                } else if let Some(translator) = &mut translator {
                    match translator.feed(&bytes) {
                        Ok(bytes) => Bytes::from(bytes),
                        Err(_) => {
                            diagnostics::stage("sse_translate");
                            attempt.record.status_code = 502;
                            break;
                        }
                    }
                } else {
                    bytes
                };
                let sent = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => false,
                    result = tx.send(Ok(bytes)) => result.is_ok(),
                };
                if !sent {
                    break;
                }
                if codex.as_ref().is_some_and(|events| events.complete())
                    || chat_responses
                        .as_ref()
                        .is_some_and(|translator| translator.complete())
                {
                    attempt.record.failed = usage.failed;
                    producer_completed.store(!usage.failed, std::sync::atomic::Ordering::Release);
                    break;
                }
            }
            if attempt.record.failed && attempt.record.status_code < 400 && !tx.is_closed() {
                attempt.record.status_code = 502;
            }
            attempt.record.breakdown = usage.breakdown();
            tracing::info!(
                event = "stream_finished",
                status = attempt.record.status_code,
                completed = producer_completed.load(std::sync::atomic::Ordering::Acquire),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "gateway stream producer stopped"
            );
            // Transport must stop before the execution guard completes.
            drop(source);
            // The downstream body owns final release/accounting, even when the
            // producer has already buffered its last chunk.
            let _ = attempt_tx.send(attempt);
        }
        .in_current_span(),
    );
    let body = stream::unfold((rx, Some(attempt_rx)), move |(mut rx, mut owner)| {
        let completed = completed.clone();
        let downstream_complete = downstream_complete.clone();
        async move {
            if let Some(item) = rx.recv().await {
                return Some((item, (rx, owner)));
            }
            let receiver = owner.take()?;
            let attempt = receiver.await.ok();
            downstream_complete.store(true, std::sync::atomic::Ordering::Release);
            drop(attempt);
            let terminal = if completed.load(std::sync::atomic::Ordering::Acquire) {
                Ok(Bytes::new())
            } else {
                Err(io::Error::other("upstream stream incomplete"))
            };
            Some((terminal, (rx, owner)))
        }
    });
    let mut response = Response::new(Body::from_stream(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert("content-type", "text/event-stream".parse().unwrap());
    response
        .headers_mut()
        .insert("cache-control", "no-cache".parse().unwrap());
    response
}
