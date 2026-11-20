//! Responses SSE to downstream WebSocket bridge for credentials without WS opt-in.
use super::{Writer, ws_home, ws_state};
use crate::{
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
        let prepared = providers::prepare(
            &home.config(),
            selection,
            "/v1/responses",
            headers,
            body.clone(),
            true,
        )?;
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
        let mut source = response.bytes_stream();
        let mut decoder = Decoder::default();
        let mut completed = false;
        while !completed {
            let bytes = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                next = source.next() => next.ok_or_else(|| anyhow::anyhow!("incomplete HTTP response"))??,
            };
            for event in decoder.feed(&bytes, limit)? {
                turn.event(&event);
                let kind = event["type"].as_str().unwrap_or("");
                if matches!(kind, "response.completed" | "response.done") {
                    history.commit(body.clone(), &event, limit)?;
                    turn.record.failed = turn.usage.failed;
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
mod tests {
    use super::*;
    #[test]
    fn fragmented_sse_and_limits() {
        let mut decoder = Decoder::default();
        let mut events = Vec::new();
        for b in b"event: response.created\r\ndata: {\"type\":\"response.created\"}\r\n\r\ndata: [DONE]\n\n" {
            events.extend(decoder.feed(&[*b], 1024).unwrap());
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "response.created");
        assert!(decoder.feed(&[b'x'; 100], 10).is_err());
    }
}
