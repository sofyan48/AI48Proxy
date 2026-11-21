//! Codex HTTP is always SSE, even for downstream JSON requests.
//! Bound both individual events and cumulative input during JSON collection.
use super::{diagnostics, error, lifecycle, upstream::Format, usage::Usage};
use anyhow::{Result, bail, ensure};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[path = "codex_multiagent.rs"]
pub(super) mod multiagent;

const MAX_RESPONSE: usize = 32 * 1024 * 1024;

pub(super) struct Events {
    line: Vec<u8>,
    data: Vec<u8>,
    limit: usize,
    pub response: Option<Value>,
    allow_incomplete: bool,
}
impl Events {
    pub fn new() -> Self {
        Self::with_limit(MAX_RESPONSE)
    }
    fn with_limit(limit: usize) -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            limit,
            response: None,
            allow_incomplete: false,
        }
    }
    pub fn for_chat() -> Self {
        Self {
            allow_incomplete: true,
            ..Self::new()
        }
    }
    pub fn complete(&self) -> bool {
        self.response.is_some()
    }
    pub fn feed(&mut self, bytes: &[u8], usage: &mut Usage) -> Result<()> {
        let result = self.feed_inner(bytes, usage);
        if result.is_err() {
            diagnostics::stage("codex_sse_parse");
        }
        result
    }
    fn feed_inner(&mut self, bytes: &[u8], usage: &mut Usage) -> Result<()> {
        for &byte in bytes {
            if self.complete() {
                break;
            }
            if byte != b'\n' {
                ensure!(
                    self.line.len().saturating_add(self.data.len()) < self.limit,
                    "Codex event exceeds limit"
                );
                self.line.push(byte);
                continue;
            }
            if self.line.last() == Some(&b'\r') {
                self.line.pop();
            }
            if self.line.is_empty() {
                let data = std::mem::take(&mut self.data);
                if data.is_empty() {
                    continue;
                }
                ensure!(
                    data.strip_suffix(b"\n").unwrap_or(&data) != b"[DONE]",
                    "Codex DONE before completion"
                );
                let event: Value = serde_json::from_slice(&data)?;
                ensure!(
                    event.get("type").and_then(Value::as_str).is_some(),
                    "Codex event type missing"
                );
                if let Some(failure) = diagnostics::event_failure(&event, "codex") {
                    return Err(failure.into());
                }
                usage.json(&event);
                let incomplete = self.allow_incomplete && event["type"] == "response.incomplete";
                let response = if incomplete {
                    crate::protocol::responses::finish_reason(&event["response"], false)?;
                    Some(event["response"].clone())
                } else {
                    crate::providers::codex::completed_response(&event)?
                };
                if let Some(response) = &response {
                    ensure!(
                        response
                            .get("id")
                            .and_then(Value::as_str)
                            .is_some_and(|id| !id.is_empty()),
                        "Codex response id missing"
                    );
                    ensure!(
                        response.get("object").and_then(Value::as_str) == Some("response"),
                        "Codex terminal object is not a response"
                    );
                    ensure!(
                        response.get("status").and_then(Value::as_str) == Some("completed")
                            || (incomplete && response["status"] == "incomplete"),
                        "Codex terminal response not completed"
                    );
                }
                self.response = response;
            } else if let Some(data) = self.line.strip_prefix(b"data:") {
                let data = data.strip_prefix(b" ").unwrap_or(data);
                ensure!(
                    self.data.len().saturating_add(data.len() + 1) <= self.limit,
                    "Codex event exceeds limit"
                );
                self.data.extend_from_slice(data);
                self.data.push(b'\n');
            }
            self.line.clear();
        }
        Ok(())
    }
}

#[cfg(test)]
async fn collect<S, E>(
    source: S,
    cancellation: CancellationToken,
    usage: &mut Usage,
    limit: usize,
    first_byte: impl FnMut(),
) -> Result<Value>
where
    S: Stream<Item = std::result::Result<bytes::Bytes, E>> + Unpin,
{
    collect_events(
        source,
        cancellation,
        usage,
        limit,
        first_byte,
        Events::with_limit(limit),
    )
    .await
}

async fn collect_events<S, E>(
    mut source: S,
    cancellation: CancellationToken,
    usage: &mut Usage,
    limit: usize,
    mut first_byte: impl FnMut(),
    mut events: Events,
) -> Result<Value>
where
    S: Stream<Item = std::result::Result<bytes::Bytes, E>> + Unpin,
{
    let mut received = 0usize;
    loop {
        let next = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                diagnostics::stage("codex_collect_cancelled");
                bail!("Codex collection cancelled");
            },
            next = source.next() => next,
        };
        match next {
            Some(Ok(bytes)) => {
                if bytes.len() > limit.saturating_sub(received) {
                    diagnostics::stage("codex_collect_limit");
                }
                ensure!(
                    bytes.len() <= limit.saturating_sub(received),
                    "Codex collected response exceeds limit"
                );
                if received == 0 && !bytes.is_empty() {
                    first_byte();
                }
                received += bytes.len();
                events.feed(&bytes, usage)?;
                if let Some(response) = events.response {
                    return Ok(response);
                }
            }
            Some(Err(_)) => {
                diagnostics::stage("codex_collect_body");
                bail!("Codex transport failed");
            }
            None => {
                diagnostics::stage("codex_collect_incomplete");
                bail!("Codex stream ended before completion");
            }
        }
    }
}

pub(super) async fn buffered(
    response: reqwest::Response,
    mut attempt: lifecycle::Attempt,
    response_kind: super::upstream::ResponseKind,
    model: &str,
) -> Response {
    let mut usage = Usage::new(Format::OpenAi);
    let events = if response_kind.responses_chat() {
        Events::for_chat()
    } else {
        Events::new()
    };
    let result = collect_events(
        response.bytes_stream().map(|result| {
            if let Err(err) = &result {
                tracing::warn!(
                    event = "response_failure",
                    stage = "codex_body",
                    error_kind = diagnostics::transport_kind(err),
                    "Codex body read failed"
                );
            }
            result
        }),
        attempt.cancellation(),
        &mut usage,
        MAX_RESPONSE,
        || attempt.first_byte(),
        events,
    )
    .await;
    attempt.record.breakdown = usage.breakdown();
    match result {
        Ok(response) => {
            if response_kind.responses_chat() {
                match crate::protocol::responses::chat_response(&response, model) {
                    Ok(chat) => {
                        attempt.record.failed = false;
                        Json(chat).into_response()
                    }
                    Err(_) => {
                        diagnostics::stage("codex_chat_translate");
                        attempt.record.local_failure(502);
                        error(
                            StatusCode::BAD_GATEWAY,
                            "translation_error",
                            "Invalid Responses completion",
                        )
                    }
                }
            } else {
                attempt.record.failed = false;
                Json(response).into_response()
            }
        }
        Err(err) => {
            let cancelled = attempt.cancellation().is_cancelled();
            let status = if cancelled {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::BAD_GATEWAY
            };
            diagnostics::processing_failure(&mut attempt.record, &err, status.as_u16());
            error(
                status,
                "upstream_error",
                "Codex response failed, was incomplete, or exceeded the limit",
            )
        }
    }
}

/// Go codexBootstrapMaxBufferedFrames: counted in upstream lines, blank separators included.
const BOOTSTRAP_MAX_LINES: usize = 48;
/// Go codexBootstrapMaxBufferedBytes: bounds raw bytes held, including a partial line.
const BOOTSTRAP_MAX_BYTES: usize = 1 << 20;

pub(super) enum Bootstrap {
    /// Held bytes are replayed ahead of the untouched remainder of the body.
    Released(reqwest::Response),
    /// A synthesized HTTP 503 carrying the normalized error body, so the caller
    /// runs the same failure/rotation path as a genuine upstream 503.
    Rejected(reqwest::Response),
    Cancelled,
}

enum Line {
    Hold,
    Release,
    Reject(Value),
}

/// Go stream-bootstrap-buffering (HTTP/SSE): hold pre-generation frames until the
/// first generated event so an in-stream overload rejection can fail the attempt
/// before downstream headers are committed. Payloads are never logged.
pub(super) async fn bootstrap(
    response: reqwest::Response,
    timeout: Option<std::time::Duration>,
    cancellation: &CancellationToken,
) -> Bootstrap {
    let deadline = timeout.map(|timeout| tokio::time::Instant::now() + timeout);
    let status = response.status();
    let version = response.version();
    let headers = response.headers().clone();
    let mut source = response.bytes_stream();
    let mut held = Vec::new();
    let mut scanned = 0usize;
    let mut lines = 0usize;
    let mut failure = None;
    let reason = 'probe: loop {
        while let Some(end) = held[scanned..].iter().position(|&b| b == b'\n') {
            lines += 1;
            if lines > BOOTSTRAP_MAX_LINES {
                break 'probe "frame_budget";
            }
            match classify(&held[scanned..scanned + end]) {
                Line::Hold => scanned += end + 1,
                Line::Release => break 'probe "generation_started",
                Line::Reject(body) => {
                    tracing::info!(
                        event = "codex_bootstrap_rejected",
                        held_lines = lines,
                        "Codex overload during stream bootstrap; failing attempt over"
                    );
                    let body = serde_json::to_vec(&body).unwrap_or_default();
                    let mut builder = http::Response::builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .version(version)
                        .header("content-type", "application/json");
                    if let Some(value) = headers.get("retry-after") {
                        builder = builder.header("retry-after", value);
                    }
                    return match builder.body(reqwest::Body::from(body)) {
                        Ok(rejected) => Bootstrap::Rejected(rejected.into()),
                        Err(_) => Bootstrap::Cancelled,
                    };
                }
            }
        }
        if held.len() > BOOTSTRAP_MAX_BYTES {
            break 'probe "byte_budget";
        }
        let expired = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        let next = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Bootstrap::Cancelled,
            _ = expired => break 'probe "time_budget",
            next = source.next() => next,
        };
        match next {
            Some(Ok(bytes)) => held.extend_from_slice(&bytes),
            Some(Err(err)) => {
                failure = Some(Err(err));
                break 'probe "transport_error";
            }
            // A clean end stays request-scoped: the stream consumer reports it incomplete.
            None => break 'probe "ended",
        }
    };
    tracing::debug!(
        event = "codex_bootstrap_released",
        reason,
        held_lines = lines,
        held_bytes = held.len(),
        "Codex stream bootstrap released"
    );
    let prefix = (!held.is_empty()).then(|| Ok(bytes::Bytes::from(held)));
    let body = futures_util::stream::iter(prefix.into_iter().chain(failure)).chain(source);
    let mut builder = http::Response::builder().status(status).version(version);
    if let Some(target) = builder.headers_mut() {
        *target = headers;
    }
    match builder.body(reqwest::Body::wrap_stream(body)) {
        Ok(released) => Bootstrap::Released(released.into()),
        Err(_) => Bootstrap::Cancelled,
    }
}

fn classify(line: &[u8]) -> Line {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    // event:, id:, comments and blank separators belong to the frame they frame.
    let Some(data) = line.strip_prefix(b"data:") else {
        return Line::Hold;
    };
    let data = data.trim_ascii();
    if data.is_empty() {
        return Line::Hold;
    }
    let Ok(event) = serde_json::from_slice::<Value>(data) else {
        return Line::Release;
    };
    let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
    if matches!(kind, "error" | "response.failed") {
        return match terminal_error(&event, kind) {
            Some(body) if overload(&body) => Line::Reject(body),
            _ => Line::Release,
        };
    }
    if bufferable(kind, &event) {
        Line::Hold
    } else {
        Line::Release
    }
}

/// Go isCodexBootstrapBufferableEvent: a closed list, so unknown frames release.
fn bufferable(kind: &str, event: &Value) -> bool {
    match kind {
        "response.created"
        | "response.in_progress"
        | "codex.rate_limits"
        | "codex.response.metadata"
        | "response.metadata"
        | "keepalive" => true,
        "response.output_item.added" => {
            let item = &event["item"];
            let text = |key: &str| item[key].as_str().unwrap_or("").is_empty();
            match item["type"].as_str() {
                Some("message") => empty_list(&item["content"]),
                Some("reasoning") => {
                    text("encrypted_content")
                        && empty_list(&item["summary"])
                        && empty_list(&item["content"])
                }
                Some("function_call") => text("arguments"),
                Some("custom_tool_call") => text("input"),
                _ => false,
            }
        }
        "response.content_part.added" | "response.reasoning_summary_part.added" => {
            empty_part(&event["part"])
        }
        _ => false,
    }
}

fn empty_list(list: &Value) -> bool {
    list.as_array()
        .is_none_or(|entries| entries.iter().all(empty_part))
}

fn empty_part(part: &Value) -> bool {
    let empty = |key: &str| part[key].as_str().unwrap_or("").is_empty();
    match part["type"].as_str() {
        Some("output_text" | "summary_text" | "text" | "reasoning_text") => empty("text"),
        Some("refusal") => empty("refusal"),
        _ => false,
    }
}

/// Go codexTerminalFailureBody, normalized to `{"error":{...}}`.
fn terminal_error(event: &Value, kind: &str) -> Option<Value> {
    let paths: &[&str] = if kind == "response.failed" {
        &["/response/error", "/error", "/body/error"]
    } else {
        &["/error", "/body/error", "/response/error"]
    };
    let error = paths
        .iter()
        .filter_map(|path| event.pointer(path))
        .find(|value| !value.is_null())
        .map(|value| match value {
            Value::String(message) => serde_json::json!({"message": message}),
            other => other.clone(),
        })
        .or_else(|| {
            (kind == "error").then(|| {
                let mut error = serde_json::Map::new();
                for (from, to) in [
                    ("message", "message"),
                    ("code", "code"),
                    ("error_type", "type"),
                ] {
                    if let Some(value) = event.get(from).filter(|v| v.is_string()) {
                        error.insert(to.into(), value.clone());
                    }
                }
                Value::Object(error)
            })
        })?;
    Some(serde_json::json!({ "error": error }))
}

/// Go isCodexOverloadBootstrapFailure: only transient capacity, overload and
/// rate-limit rejections fail over; everything else keeps in-stream delivery.
fn overload(body: &Value) -> bool {
    let field = |key: &str| {
        body["error"][key]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    };
    let (kind, code, message) = (field("type"), field("code"), field("message"));
    let capacity_code = matches!(
        code.as_str(),
        "" | "turn_rate_limit_exceeded" | "model_at_capacity" | "model_is_at_capacity"
    );
    let at_capacity = message.contains("model is at capacity")
        || message.contains("model_at_capacity")
        || message.contains("model_is_at_capacity")
        || (message.contains("model") && message.contains("at capacity"));
    (capacity_code && (at_capacity || code == "turn_rate_limit_exceeded"))
        || kind == "service_unavailable_error"
        || code == "server_is_overloaded"
        || kind == "rate_limit_error"
        || code == "rate_limit_exceeded"
        || ((kind == "server_error" || code == "server_error")
            && message.contains("you can retry your request"))
}

#[cfg(test)]
#[path = "codex_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "codex_bootstrap_tests.rs"]
mod bootstrap_tests;

#[cfg(test)]
mod terminal_alias_tests {
    use super::*;
    use bytes::Bytes;
    use futures_util::stream;
    use serde_json::json;

    fn wire(response: Value) -> String {
        format!(
            "event: response.done\r\ndata: {}\r\n\r\n",
            json!({"type":"response.done","response":response})
        )
    }

    fn response() -> Value {
        json!({"id":"resp_fixture","object":"response","status":"completed","output":[],
            "usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}})
    }

    #[tokio::test]
    async fn alias_collects_before_eof_and_tracks_fragmented_stream_for_both_modes() {
        for chat in [false, true] {
            let mut events = if chat {
                Events::for_chat()
            } else {
                Events::new()
            };
            let mut translator = crate::protocol::responses::StreamTranslator::new("alias", true);
            let mut usage = Usage::new(Format::OpenAi);
            let mut output = Vec::new();
            for byte in wire(response()).bytes() {
                events.feed(&[byte], &mut usage).unwrap();
                if chat {
                    output.extend(translator.feed(&[byte]).unwrap());
                }
            }
            assert!(events.complete());
            assert_eq!(events.response.unwrap(), response());
            assert_eq!(usage.breakdown().total_tokens, 5);
            if chat {
                translator.finish().unwrap();
                assert!(String::from_utf8(output).unwrap().contains("data: [DONE]"));
            }
            let source =
                stream::iter([Ok::<_, ()>(Bytes::from(wire(response())))]).chain(stream::pending());
            let events = if chat {
                Events::for_chat()
            } else {
                Events::new()
            };
            let collected = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                collect_events(
                    source,
                    CancellationToken::new(),
                    &mut usage,
                    4096,
                    || {},
                    events,
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(collected, response());
        }
    }

    #[test]
    fn alias_never_marks_invalid_or_unfinished_response_complete() {
        for chat in [false, true] {
            for (field, value) in [
                ("status", json!("failed")),
                ("status", json!("in_progress")),
                ("status", json!("incomplete")),
                ("status", Value::Null),
                ("object", json!("chat.completion")),
                ("id", json!("")),
            ] {
                let mut response = response();
                response[field] = value;
                response["incomplete_details"] = json!({"reason":"max_output_tokens"});
                let mut events = if chat {
                    Events::for_chat()
                } else {
                    Events::new()
                };
                assert!(
                    events
                        .feed(wire(response).as_bytes(), &mut Usage::new(Format::OpenAi))
                        .is_err()
                );
                assert!(!events.complete());
            }
            let mut events = if chat {
                Events::for_chat()
            } else {
                Events::new()
            };
            events
                .feed(
                    wire(response()).trim_end().as_bytes(),
                    &mut Usage::new(Format::OpenAi),
                )
                .unwrap();
            assert!(!events.complete());
        }
    }
}
