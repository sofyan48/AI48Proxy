//! Responses SSE to downstream WebSocket bridge for credentials without WS opt-in.
use super::super::codex::multiagent;
use super::{Writer, ws_codex, ws_home, ws_state};
use crate::{
    config::Config,
    home::client::{HomeClient, Selection},
    providers,
};
use anyhow::{Result, ensure};
use axum::{extract::ws::Message, http::HeaderMap};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// One owner retains the Home lease while bridging sequential response turns.
#[allow(clippy::too_many_arguments)]
pub(super) async fn session(
    home: &Arc<HomeClient>,
    headers: &HeaderMap,
    principal: &str,
    selection: &Selection,
    first: Value,
    writer: &mut Writer,
    input: &mut mpsc::Receiver<Message>,
    limit: usize,
    cancel: CancellationToken,
) -> Result<()> {
    let alias = first["model"].as_str().unwrap_or("").to_owned();
    let mut history = ws_state::History::default();
    let mut body = first;
    loop {
        ws_home::authorize(home, headers, Some(principal), Some(&alias)).await?;
        let (prepared, flags) = prepare(&home.config(), selection, headers, body.clone())?;
        let mut turn = ws_home::Turn::new(home.clone(), selection, &alias);
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            result = prepared.request.send() => result?,
        };
        ensure!(
            response.status().is_success(),
            "upstream HTTP rejected request"
        );
        let mut decoder = ResponseDecoder::new(&response, flags);
        let mut source = response.bytes_stream();
        let mut completed = false;
        while !completed {
            let bytes = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                next = source.next() => next.transpose()?,
            };
            let events = match bytes {
                Some(ref bytes) => decoder.feed(bytes, limit)?,
                None => decoder.finish(limit)?,
            };
            for event in events {
                turn.event(&event);
                let kind = event["type"].as_str().unwrap_or("");
                if matches!(kind, "response.completed" | "response.done") {
                    let successful = event
                        .pointer("/response/status")
                        .and_then(Value::as_str)
                        .is_none_or(|status| status == "completed");
                    turn.record.failed = !successful || turn.usage.failed;
                    if !turn.record.failed {
                        history.commit(body.clone(), &event, limit)?;
                    } else {
                        history.invalidate();
                    }
                    turn.record.status_code = if turn.record.failed { 502 } else { 200 };
                    completed = true;
                }
                writer
                    .send(Message::Text(serde_json::to_string(&event)?.into()))
                    .await?;
                ensure!(
                    !matches!(kind, "error" | "response.failed" | "response.incomplete"),
                    "upstream turn failed"
                );
                if completed {
                    break;
                }
            }
            ensure!(bytes.is_some() || completed, "incomplete HTTP response");
        }
        drop(turn);
        body = loop {
            let frame = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                next = input.recv() => match next { Some(frame) => frame, None => return Ok(()) },
            };
            match super::decode(frame).and_then(|frame| history.hydrate(frame, limit)) {
                Ok(body) if body["model"].as_str() == Some(&alias) => break body,
                _ => {
                    super::send_error(
                        writer,
                        "invalid_continuation",
                        "Expected known history or full input with the same model",
                    )
                    .await?
                }
            }
        };
        // The first request already crossed the handler boundary in websocket::session.
        // Hydrated HTTP continuations need the same boundary before attempt preparation.
        multiagent::prepare_boundary_home(
            home,
            &home.config(),
            "/v1/responses",
            headers,
            &mut body,
        )
        .await;
    }
}

/// Flags belong to this HTTP attempt only. There is no upstream WS connection
/// whose optimization state can carry over from an earlier turn.
fn prepare(
    config: &Config,
    selection: &Selection,
    headers: &HeaderMap,
    mut body: Value,
) -> Result<(providers::Prepared, ws_codex::Flags)> {
    let conflict = multiagent::has_conflict(&body);
    let optimized =
        multiagent::prepare_attempt(config, selection, "/v1/responses", headers, &mut body);
    // Provider normalization and user payload rules remain the final barrier.
    let prepared = providers::prepare(config, selection, "/v1/responses", headers, body, true)?;
    Ok((
        prepared,
        ws_codex::Flags {
            optimized,
            conflict,
        },
    ))
}

struct ResponseDecoder {
    flags: ws_codex::Flags,
    json: bool,
    body: Vec<u8>,
    sse: Decoder,
}

impl ResponseDecoder {
    fn new(response: &reqwest::Response, flags: ws_codex::Flags) -> Self {
        let content_type = response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        Self {
            flags,
            json: content_type.eq_ignore_ascii_case("application/json")
                || content_type.to_ascii_lowercase().ends_with("+json"),
            body: Vec::new(),
            sse: Decoder::default(),
        }
    }

    fn restore(&self, events: Vec<Value>) -> Result<Vec<Value>> {
        events
            .into_iter()
            .map(|event| {
                let text = ws_codex::restore(serde_json::to_string(&event)?, self.flags, false);
                Ok(serde_json::from_str(&text)?)
            })
            .collect()
    }

    fn feed(&mut self, bytes: &[u8], limit: usize) -> Result<Vec<Value>> {
        if self.json {
            ensure!(
                self.body.len().saturating_add(bytes.len()) <= limit,
                "JSON response exceeds limit"
            );
            self.body.extend_from_slice(bytes);
            Ok(Vec::new())
        } else {
            let events = self.sse.feed(bytes, limit)?;
            self.restore(events)
        }
    }

    fn finish(&mut self, limit: usize) -> Result<Vec<Value>> {
        if self.json {
            let value: Value = serde_json::from_slice(&std::mem::take(&mut self.body))?;
            let event = if value.get("object").and_then(Value::as_str) == Some("response") {
                let kind = match value.get("status").and_then(Value::as_str) {
                    Some("completed") => "response.completed",
                    Some("failed") => "response.failed",
                    Some("incomplete") => "response.incomplete",
                    _ => anyhow::bail!("incomplete JSON response"),
                };
                serde_json::json!({"type":kind, "response":value})
            } else {
                value
            };
            self.restore(vec![event])
        } else {
            // Dispatch the last SSE event even when EOF omits the blank line.
            let events = self.sse.feed(b"\n\n", limit)?;
            self.restore(events)
        }
    }
}

#[derive(Default)]
struct Decoder {
    line: Vec<u8>,
    data: Vec<u8>,
}
impl Decoder {
    fn feed(&mut self, bytes: &[u8], limit: usize) -> Result<Vec<Value>> {
        let mut events = Vec::new();
        for &byte in bytes {
            ensure!(
                self.line.len().saturating_add(self.data.len()) < limit,
                "SSE event exceeds limit"
            );
            if byte != b'\n' {
                self.line.push(byte);
                continue;
            }
            if self.line.last() == Some(&b'\r') {
                self.line.pop();
            }
            if self.line.is_empty() {
                if !self.data.is_empty() {
                    if self.data != b"[DONE]\n" {
                        events.push(serde_json::from_slice(&self.data)?);
                    }
                    self.data.clear();
                }
            } else if let Some(data) = self.line.strip_prefix(b"data:") {
                self.data
                    .extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
                self.data.push(b'\n');
            }
            self.line.clear();
        }
        Ok(events)
    }
}

#[cfg(test)]
#[path = "ws_http_tests.rs"]
mod tests;
