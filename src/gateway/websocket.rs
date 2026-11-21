//! Native Responses WebSockets. One upstream stream owns one Home admission.
use crate::home::client::HomeClient;
use anyhow::{Result, ensure};
use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::Response,
};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as UpMessage;
use tokio_util::sync::CancellationToken;

#[path = "ws_home.rs"]
mod ws_home;
#[path = "ws_http.rs"]
mod ws_http;
#[path = "ws_replay.rs"]
mod ws_replay;
#[path = "ws_state.rs"]
mod ws_state;
#[path = "ws_steering.rs"]
mod ws_steering;
#[cfg(test)]
#[path = "ws_tests.rs"]
mod ws_tests;
#[path = "ws_transport.rs"]
mod ws_transport;

pub(super) async fn upgrade(
    State(home): State<Arc<HomeClient>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !home.ready() {
        return super::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "not_ready",
            "Home is not ready",
        );
    }
    let principal = match ws_home::authorize(&home, &headers, None, None).await {
        Ok(principal) => principal,
        Err(_) => {
            return super::error(
                StatusCode::UNAUTHORIZED,
                "invalid_credential",
                "WebSocket authentication failed",
            );
        }
    };
    let limit = home
        .config()
        .max_request_body_bytes
        .unwrap_or(16 * 1024 * 1024);
    ws.max_message_size(limit)
        .max_frame_size(limit)
        .on_upgrade(move |socket| run(home, headers, principal, socket, limit))
}

struct Reader(tokio::task::JoinHandle<()>);
impl Drop for Reader {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type Writer = SplitSink<WebSocket, Message>;

async fn run(
    home: Arc<HomeClient>,
    headers: HeaderMap,
    principal: String,
    socket: WebSocket,
    limit: usize,
) {
    let (mut writer, mut reader) = socket.split();
    let disconnected = CancellationToken::new();
    let peer = disconnected.clone();
    let (tx, mut rx) = mpsc::channel(16);
    let _reader = Reader(tokio::spawn(async move {
        while let Some(Ok(message)) = reader.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
            if matches!(message, Message::Text(_) | Message::Binary(_))
                && tx.try_send(message).is_err()
            {
                break;
            }
        }
        peer.cancel();
    }));
    let membership = home.cancellation();
    let result = tokio::select! {
        biased;
        _ = disconnected.cancelled() => return,
        _ = membership.cancelled() => Err(anyhow::anyhow!("Home membership ended")),
        result = session(home.clone(), &headers, &principal, &mut writer, &mut rx, limit) => result,
    };
    if result.is_err() {
        // Never expose transport errors, URLs, credentials, or provider bodies.
        let _ = tokio::time::timeout(home.config().lifecycle.cancel_bound, async {
            send_error(
                &mut writer,
                "websocket_session_ended",
                "WebSocket session ended; reconnect with full input",
            )
            .await?;
            writer.send(Message::Close(None)).await
        })
        .await;
    }
}

fn decode(message: Message) -> Result<Value> {
    let value: Value = match message {
        Message::Text(text) => serde_json::from_str(text.as_str())?,
        Message::Binary(bytes) => serde_json::from_slice(&bytes)?,
        _ => anyhow::bail!("expected JSON message"),
    };
    ensure!(value.is_object(), "expected object");
    Ok(value)
}

async fn send_error(writer: &mut Writer, code: &str, message: &str) -> Result<(), axum::Error> {
    writer.send(Message::Text(json!({"type":"error", "error":{"type":"invalid_request_error", "code":code, "message":message}}).to_string().into())).await
}

async fn session(
    home: Arc<HomeClient>,
    headers: &HeaderMap,
    principal: &str,
    writer: &mut Writer,
    input: &mut mpsc::Receiver<Message>,
    limit: usize,
) -> Result<()> {
    let mut history = ws_state::History::default();
    history.native = true;
    let mut replay = ws_replay::ReplayCache::default();
    let mut first = loop {
        let frame = decode(
            input
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("downstream closed"))?,
        );
        match frame.and_then(|frame| history.hydrate(frame, limit)) {
            Ok(body) => break body,
            Err(_) => {
                send_error(
                    writer,
                    "invalid_request",
                    "Expected response.create with model and array input",
                )
                .await?
            }
        }
    };
    // Go scopes a long-lived WS execution session before deriving its prompt key.
    let execution = uuid::Uuid::new_v4().to_string();
    if first
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        && let Some(key) = crate::providers::codex::cache::provider_session(&execution, "")
    {
        first["prompt_cache_key"] = key.into();
    }
    let alias = first["model"].as_str().unwrap_or("").to_owned();
    ws_home::authorize(&home, headers, Some(principal), Some(&alias)).await?;
    // No redispatch loop: an ambiguous admission/write/output can never replay.
    let (selection, lease) =
        ws_home::dispatch(home.clone(), ws_home::dispatch_request(headers, &first)).await?;
    ensure!(
        selection.user_api_key == principal,
        "dispatch principal changed"
    );
    let cancel = lease.cancellation();
    ensure!(!cancel.is_cancelled(), "membership ended");
    let prepared = ws_transport::prepare(&home.config(), &selection, headers, first.clone())?;
    if !prepared.native {
        return ws_http::session(
            &home, headers, principal, &selection, first, writer, input, limit, cancel,
        )
        .await;
    }
    history.native = true;
    // Read once per socket, as Go binds steering at connect time. permessage-deflate
    // (`codex.websocket-compression`) is never negotiated by tungstenite/axum, so
    // both directions behave like Go's `websocket-compression: false`.
    let mut steering = ws_steering::Steering::new(home.config().providers.codex_response_steering);
    let mut turn = Some(ws_home::Turn::new(home.clone(), &selection, &alias));
    // CPA bounds connection acquisition, never the established response stream.
    let (mut upstream, _) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio_tungstenite::connect_async_with_config(
            prepared.handshake,
            Some(
                tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                    .max_message_size(Some(limit))
                    .max_frame_size(Some(limit)),
            ),
            true,
        ),
    )
    .await??;
    upstream
        .send(UpMessage::Text(prepared.message.into()))
        .await?;
    let mut pending = Some(first);
    let mut active_id: Option<String> = None;
    let idle = tokio::time::sleep(std::time::Duration::from_secs(300));
    tokio::pin!(idle);
    loop {
        let queued = steering.pop_ready(turn.is_none() && active_id.is_none());
        let frame = if let Some(frame) = queued {
            frame
        } else {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = &mut idle => { replay.invalidate(); anyhow::bail!("Codex websocket idle deadline") },
                message = upstream.next() => {
                    idle.as_mut().reset(tokio::time::Instant::now() + std::time::Duration::from_secs(300));
                    let Some(Ok(message)) = message else { anyhow::bail!("upstream disconnected"); };
                    let text = match message {
                        UpMessage::Text(text) => text.to_string(),
                        UpMessage::Binary(bytes) => String::from_utf8(bytes.to_vec())?,
                        UpMessage::Ping(bytes) => { tokio::time::timeout(std::time::Duration::from_secs(10), upstream.send(UpMessage::Pong(bytes))).await??; continue; },
                        UpMessage::Pong(_) => continue,
                        UpMessage::Close(_) => anyhow::bail!("upstream closed"),
                        _ => continue,
                    };
                    ensure!(text.len() <= limit, "upstream event exceeds limit");
                    let event: Value = serde_json::from_str(&text)?;
                    let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
                    if steering.enabled && kind.starts_with("response.steer.") {
                        // Acks/pending/failures are opaque and never end or fail a response.
                        steering.observe(&event);
                        writer.send(Message::Text(text.into())).await?;
                        continue;
                    }
                    if kind == "response.created" && steering.enabled {
                        ensure!(active_id.is_none(), "unexpected upstream response");
                        let explicit = turn.is_some();
                        steering.created(&event, explicit)?;
                        if !explicit {
                            // Automatic successor: same selection and lease, new usage record.
                            turn = Some(ws_home::Turn::new(home.clone(), &selection, &alias));
                            pending = None;
                        }
                    }
                    if let Some(turn) = turn.as_mut() { turn.event(&event); }
                    if kind == "response.created" {
                        ensure!(turn.is_some() && active_id.is_none(), "unexpected upstream response");
                        active_id = event.pointer("/response/id").and_then(Value::as_str).map(str::to_owned);
                        ensure!(active_id.is_some(), "response id missing");
                    }
                    let terminal = matches!(kind, "error" | "response.completed" | "response.done" | "response.failed" | "response.incomplete" | "response.cancelled");
                    let owns_active = ws_state::terminal_owns_active(&event, active_id.as_deref());
                    if terminal && kind != "error" {
                        ensure!(ws_state::terminal_response_id(&event, active_id.as_deref()).is_some(), "terminal response id missing");
                    }
                    if terminal && let Some(id) = ws_state::terminal_response_id(&event, active_id.as_deref()) {
                        steering.ended(id);
                    }
                    if owns_active {
                        ensure!(turn.is_some(), "terminal response without active turn");
                        let successful = matches!(kind, "response.completed" | "response.done")
                            && event.pointer("/response/status").and_then(Value::as_str).is_none_or(|s| s == "completed");
                        let current = turn.as_mut().expect("checked turn");
                        current.record.failed = !successful || current.usage.failed;
                        current.record.status_code = if current.record.failed { 502 } else { 200 };
                        if !current.record.failed {
                            if let Some(request) = pending.take() {
                                history.commit(request.clone(), &event, limit)?;
                                replay.commit(
                                    active_id.clone().ok_or_else(|| anyhow::anyhow!("response id missing"))?,
                                    request,
                                    event.clone(),
                                );
                            } else {
                                ensure!(steering.enabled, "request missing");
                                // A steering successor continues the parent's chain.
                                history.chain(&event, limit)?;
                            }
                        } else {
                            history.invalidate();
                            replay.invalidate();
                            pending = None;
                        }
                        // Usage completes per response; the stream lease is deliberately retained.
                        drop(turn.take());
                        active_id = None;
                    }
                    writer.send(Message::Text(text.into())).await?;
                    if kind == "error" && owns_active { anyhow::bail!("upstream rejected request"); }
                    continue;
                },
                message = input.recv() => {
                    let Some(message) = message else { break; };
                    let frame = match decode(message) {
                        Ok(frame) => frame,
                        Err(_) => { send_error(writer, "invalid_json", "Expected a JSON object").await?; continue; },
                    };
                    let kind = frame.get("type").and_then(Value::as_str);
                    if matches!(kind, Some("response.interrupt" | "response.cancel")) {
                        if !ws_state::control(&frame, active_id.as_deref()) {
                            send_error(writer, "no_active_response", "Control frame does not own the active response").await?;
                            continue;
                        }
                        ws_home::authorize(&home, headers, Some(principal), Some(&alias)).await?;
                        // Native controls retain their original fields and never acquire/release a turn.
                        upstream.send(UpMessage::Text(serde_json::to_string(&frame)?.into())).await?;
                        continue;
                    }
                    if steering.enabled && kind == Some("response.steer") {
                        if frame.get("model").and_then(Value::as_str).is_some_and(|m| m != alias) {
                            send_error(writer, "model_changed", "Reconnect to change the model").await?;
                            continue;
                        }
                        ws_home::authorize(&home, headers, Some(principal), Some(&alias)).await?;
                        let (message, parent) = match ws_steering::prepare(&home.config(), &selection, headers, &alias, frame, limit) {
                            Ok(prepared) => prepared,
                            Err(_) => { send_error(writer, "invalid_steer", "Steering input cannot be prepared").await?; continue; },
                        };
                        // Only upstream acknowledges steering; it never acquires a turn or redispatches.
                        steering.submitted(&parent);
                        upstream.send(UpMessage::Text(message.into())).await?;
                        continue;
                    }
                    if steering.enabled && matches!(kind, Some("response.create" | "response.append")) {
                        // Go queues explicit creates until the active response and any
                        // steering successor have settled on this same socket.
                        steering.enqueue(frame)?;
                        continue;
                    }
                    frame
                },
            }
        };
        if turn.is_some() {
            send_error(
                writer,
                "response_in_progress",
                "Wait for the active response terminal event",
            )
            .await?;
            continue;
        }
        if let Some(response_id) = frame.get("previous_response_id").and_then(Value::as_str) {
            let replay_known = replay.get(response_id).is_some();
            tracing::debug!(
                replay_known,
                "Codex websocket continuation replay state checked"
            );
        }
        let mut body = match history.hydrate(frame, limit) {
            Ok(body) => body,
            Err(_) => {
                send_error(
                    writer,
                    "invalid_continuation",
                    "Continuation requires known history or a full replacement transcript",
                )
                .await?;
                continue;
            }
        };
        if body
            .get("prompt_cache_key")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
            && let Some(key) = crate::providers::codex::cache::provider_session(&execution, "")
        {
            body["prompt_cache_key"] = key.into();
        }
        if body["model"].as_str() != Some(&alias) {
            send_error(writer, "model_changed", "Reconnect to change the model").await?;
            continue;
        }
        if steering.enabled
            && !steering.create_parent_ok(
                body.get("previous_response_id")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            )
        {
            send_error(
                writer,
                "invalid_continuation",
                "response.create must continue the response waiting for required input",
            )
            .await?;
            continue;
        }
        ws_home::authorize(&home, headers, Some(principal), Some(&alias)).await?;
        let prepared =
            match ws_transport::prepare(&home.config(), &selection, headers, body.clone()) {
                Ok(prepared) => prepared,
                Err(_) => {
                    send_error(
                        writer,
                        "unsupported_payload",
                        "Request cannot be prepared for native WebSocket transport",
                    )
                    .await?;
                    continue;
                }
            };
        turn = Some(ws_home::Turn::new(home.clone(), &selection, &alias));
        pending = Some(body);
        // Even a failed send may have been admitted: terminate, never reconnect/reselect.
        upstream
            .send(UpMessage::Text(prepared.message.into()))
            .await?;
    }
    // Drop upstream before the exactly-once execution guard on every return/cancellation.
    drop(upstream);
    drop(turn);
    drop(lease);
    Ok(())
}
