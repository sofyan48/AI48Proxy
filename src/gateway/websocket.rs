//! Native Responses WebSockets. One upstream stream owns one Home admission.
use crate::home::client::HomeClient;
use anyhow::{Result, ensure};
use axum::{
    extract::{State, ws::Message},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as UpMessage;
use tokio_util::sync::CancellationToken;
use ws_deflate::{self as tokio_tungstenite, WebSocket, WebSocketUpgrade};

#[path = "ws_codex.rs"]
mod ws_codex;
#[path = "ws_deflate.rs"]
mod ws_deflate;
#[path = "ws_failure.rs"]
mod ws_failure;
#[path = "ws_home.rs"]
mod ws_home;
#[path = "ws_http.rs"]
mod ws_http;
#[path = "ws_recovery.rs"]
mod ws_recovery;
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
    let compression = home.config().providers.codex_websocket_compression;
    ws.on_upgrade(compression, limit, move |socket| {
        run(home, headers, principal, socket, limit)
    })
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
    super::codex::multiagent::prepare_boundary_home(
        &home,
        &home.config(),
        "/v1/responses",
        headers,
        &mut first,
    )
    .await;
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
    let (mut prepared, mut flags) =
        ws_codex::attempt(&home.config(), &selection, headers, first.clone())?;
    ensure!(
        prepared.message.len() <= limit,
        "prepared request exceeds limit"
    );
    if !prepared.native {
        return ws_http::session(
            &home, headers, principal, &selection, first, writer, input, limit, cancel,
        )
        .await;
    }
    history.native = true;
    // Read once per socket, as Go binds steering at connect time.
    let mut steering = ws_steering::Steering::new(home.config().providers.codex_response_steering);
    let mut owners = ws_codex::Owners::default();
    loop {
        owners.pending.push_back(ws_codex::Owner {
            request: first.clone(),
            flags,
            automatic: false,
            turn: ws_home::Turn::new(home.clone(), &selection, &alias),
        });
        let mut connection = ws_codex::connection_state(false, flags);
        // Retain only settings needed by automatic successors; steers pin their parent.
        let mut settings = std::collections::HashMap::new();
        let mut settings_order = std::collections::VecDeque::new();
        let mut pinned = std::collections::HashMap::new();
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
        history.submitted(&first);
        let mut active_id: Option<String> = None;
        let mut latest_id = String::new();
        let mut established = false;
        let mut consumed_event: Option<String> = None;
        let idle = tokio::time::sleep(std::time::Duration::from_secs(300));
        tokio::pin!(idle);
        let outcome: Result<()> = async {
    upstream.send(UpMessage::Text(prepared.message.into())).await.map_err(ws_recovery::transport)?;
    loop {
        consumed_event = None;
        let queued = if established && owners.outstanding() < ws_steering::MAX_QUEUED {
            steering.pop_ready(owners.automatic_active())
        } else { None };
        let frame = if let Some(frame) = queued {
            frame
        } else {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = &mut idle => { replay.invalidate(); anyhow::bail!("Codex websocket idle deadline") },
                message = upstream.next() => {
                    idle.as_mut().reset(tokio::time::Instant::now() + std::time::Duration::from_secs(300));
                    let message = match message {
                        Some(Ok(message)) => message,
                        Some(Err(error)) => return Err(ws_recovery::transport(error)),
                        None => return Err(ws_recovery::Lost.into()),
                    };
                    let text = match message {
                        UpMessage::Text(text) => text.to_string(),
                        UpMessage::Binary(bytes) => String::from_utf8(bytes.to_vec())?,
                        UpMessage::Ping(bytes) => { tokio::time::timeout(std::time::Duration::from_secs(10), upstream.send(UpMessage::Pong(bytes))).await?.map_err(ws_recovery::transport)?; continue; },
                        UpMessage::Pong(_) => continue,
                        UpMessage::Close(Some(close)) if u16::from(close.code) == 1006 => return Err(ws_recovery::Lost.into()),
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
                    if kind == "response.created" {
                        let id = event.pointer("/response/id").and_then(Value::as_str)
                            .filter(|id| !id.is_empty()).ok_or_else(|| anyhow::anyhow!("response id missing"))?.to_owned();
                        ensure!(!owners.active.contains_key(&id), "duplicate response id");
                        let parent = event.pointer("/response/previous_response_id").and_then(Value::as_str)
                            .filter(|id| !id.is_empty()).unwrap_or(&latest_id).to_owned();
                        let explicit = !owners.pending.is_empty();
                        steering.created(&event, explicit)?;
                        let owner = if let Some(owner) = owners.pending.pop_front() { owner } else {
                            ensure!(steering.enabled, "unexpected automatic response");
                            let (request, flags) = pinned.get(&parent).or_else(|| settings.get(&parent))
                                .cloned().ok_or_else(|| anyhow::anyhow!("automatic successor has no parent settings"))?;
                            ws_codex::Owner { request, flags, automatic: true,
                                turn: ws_home::Turn::new(home.clone(), &selection, &alias) }
                        };
                        let mut snapshot = json!({});
                        for key in ["model", "instructions", "reasoning", "service_tier"] {
                            if let Some(value) = owner.request.get(key) { snapshot[key] = value.clone(); }
                        }
                        settings.insert(id.clone(), (snapshot, owner.flags));
                        settings_order.push_back(id.clone());
                        if settings_order.len() > 16 && let Some(old) = settings_order.pop_front() { settings.remove(&old); }
                        owners.active.insert(id.clone(), owner);
                        active_id = Some(id.clone());
                        established = true;
                        latest_id = id.clone();
                        history.created(&id);
                    }
                    let failure = matches!(kind, "error" | "response.failed");
                    let terminal = failure || matches!(kind, "response.completed" | "response.done" | "response.incomplete" | "response.cancelled");
                    let id = if failure {
                        match owners.failure_owner(&event, steering.has_unacknowledged()) {
                            Ok(id) => id,
                            Err(error) => {
                                let status = ws_failure::status(&event);
                                if ws_failure::credential(status) {
                                    // Account health applies to the socket even when the
                                    // rejected request cannot safely be identified.
                                    for owner in owners.active.values_mut().chain(owners.pending.iter_mut()) {
                                        owner.turn.record.status_code = status;
                                    }
                                }
                                writer.send(Message::Text(text.into())).await?;
                                return Err(error);
                            }
                        }
                    } else {
                        event.get("response_id").and_then(Value::as_str)
                            .or_else(|| event.pointer("/response/id").and_then(Value::as_str))
                            .map(str::to_owned).or_else(|| if terminal { None } else { active_id.clone() })
                    };
                    if terminal && !failure {
                        ensure!(id.as_ref().is_some_and(|id| owners.active.contains_key(id)), "terminal response has no owner");
                    }
                    let mut rejected = if failure && id.is_none() { owners.pending.pop_front() } else { None };
                    let owner = id.as_ref().and_then(|id| owners.active.get_mut(id)).or(rejected.as_mut());
                    let flags = owner.as_ref().map(|owner| owner.flags).unwrap_or_default();
                    let restored = ws_codex::restore(text, flags, connection);
                    let restored_event: Value = serde_json::from_str(&restored)?;
                    if let Some(owner) = owner {
                        owner.turn.event(&restored_event);
                        if terminal {
                            let successful = matches!(kind, "response.completed" | "response.done")
                                && event.pointer("/response/status").and_then(Value::as_str).is_none_or(|s| s == "completed");
                            owner.turn.record.failed = !successful || owner.turn.usage.failed;
                            owner.turn.record.status_code = if failure { ws_failure::status(&event) }
                                else if owner.turn.record.failed { 502 } else { 200 };
                            if !owner.turn.record.failed && id.as_deref() == Some(latest_id.as_str()) {
                                history.commit(owner.request.clone(), &restored_event, limit)?;
                                replay.commit(id.clone().unwrap(), owner.request.clone(), restored_event);
                            } else if owner.turn.record.failed { history.invalidate(); replay.invalidate(); }
                        }
                    }
                    if terminal && let Some(id) = id {
                        steering.ended(&id);
                        owners.active.remove(&id);
                        if active_id.as_deref() == Some(&id) { active_id = owners.active.keys().next_back().cloned(); }
                    }
                    drop(rejected);
                    if steering.settled() { pinned.clear(); }
                    writer.send(Message::Text(restored.into())).await?;
                    if failure && ws_failure::credential(ws_failure::status(&event)) {
                        anyhow::bail!("upstream credential rejected");
                    }
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
                        if !frame.get("response_id").and_then(Value::as_str).is_some_and(|id| owners.active.contains_key(id)) {
                            send_error(writer, "no_active_response", "Control frame does not own the active response").await?;
                            continue;
                        }
                        ws_home::authorize(&home, headers, Some(principal), Some(&alias)).await?;
                        // Native controls retain their original fields and never acquire/release a turn.
                        upstream.send(UpMessage::Text(serde_json::to_string(&frame)?.into())).await.map_err(ws_recovery::transport)?;
                        continue;
                    }
                    if steering.enabled && kind == Some("response.steer") {
                        if frame.get("model").and_then(Value::as_str).is_some_and(|m| m != alias) {
                            send_error(writer, "model_changed", "Reconnect to change the model").await?;
                            continue;
                        }
                        ws_home::authorize(&home, headers, Some(principal), Some(&alias)).await?;
                        let event_id = frame.get("event_id").and_then(Value::as_str).unwrap_or("").to_owned();
                        let (message, parent) = match ws_steering::prepare(&home.config(), &selection, headers, &alias, frame, limit) {
                            Ok(prepared) => prepared,
                            Err(_) => { send_error(writer, "invalid_steer", "Steering input cannot be prepared").await?; continue; },
                        };
                        // Only upstream acknowledges steering; it never acquires a turn or redispatches.
                        let target = if parent.is_empty() { latest_id.as_str() } else { &parent };
                        if let Some(snapshot) = settings.get(target) { pinned.insert(target.to_owned(), snapshot.clone()); }
                        steering.submitted(&parent, &event_id);
                        upstream.send(UpMessage::Text(message.into())).await.map_err(ws_recovery::transport)?;
                        continue;
                    }
                    if steering.enabled && matches!(kind, Some("response.create" | "response.append")) {
                        // Queue behind steering, but pipeline while an explicit response runs.
                        if owners.outstanding() + steering.queued_len() >= ws_steering::MAX_QUEUED {
                            send_error(writer, "too_many_requests", "Too many outstanding response.create requests").await?;
                            continue;
                        }
                        steering.enqueue(frame)?;
                        continue;
                    }
                    frame
                },
            }
        };
        consumed_event = frame.get("event_id").and_then(Value::as_str).map(str::to_owned);
        if !steering.enabled && owners.outstanding() > 0 {
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
        super::codex::multiagent::prepare_boundary_home(
            &home, &home.config(), "/v1/responses", headers, &mut body,
        ).await;
        ws_home::authorize(&home, headers, Some(principal), Some(&alias)).await?;
        let (prepared, flags) =
            match ws_codex::attempt(&home.config(), &selection, headers, body.clone()) {
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
        if prepared.message.len() > limit {
            send_error(writer, "unsupported_payload", "Prepared request exceeds session limit").await?;
            continue;
        }
        connection = ws_codex::connection_state(connection, flags);
        history.submitted(&body);
        consumed_event = None;
        owners.pending.push_back(ws_codex::Owner {
            request: body, flags, automatic: false,
            turn: ws_home::Turn::new(home.clone(), &selection, &alias),
        });
        // A failed send is uncertain: abandon it, never replay it on recovery.
        upstream
            .send(UpMessage::Text(prepared.message.into()))
            .await.map_err(ws_recovery::transport)?;
    }
    Ok(())
    }.await;
        drop(upstream);
        if outcome.is_err() {
            let recoverable = outcome.as_ref().unwrap_err().is::<ws_recovery::Lost>();
            if recoverable {
                for owner in owners.active.values_mut().chain(owners.pending.iter_mut()) {
                    owner.turn.record.failed = true;
                    owner.turn.record.status_code = 502;
                }
            }
            history.disconnected();
            replay.invalidate();
            let mut losses =
                ws_recovery::abandon(&mut owners, &mut steering, consumed_event.as_deref());
            if recoverable {
                // Fence input already buffered in the failed epoch. It is never a
                // subsequent explicit request, even if it has not reached steering.
                while let Ok(message) = input.try_recv() {
                    if let Ok(frame) = decode(message) {
                        let event_id = frame.get("event_id").and_then(Value::as_str).unwrap_or("");
                        let continuation = frame
                            .get("previous_response_id")
                            .and_then(Value::as_str)
                            .is_some_and(|id| !id.is_empty())
                            || frame.get("type").and_then(Value::as_str) == Some("response.append");
                        losses.push(if continuation {
                            ws_recovery::continuation(event_id)
                        } else {
                            ws_failure::lost(event_id, true)
                        });
                    }
                }
            }
            for event in losses {
                writer.send(Message::Text(event.to_string().into())).await?;
            }
            if recoverable && !cancel.is_cancelled() {
                // No reporter or steer crosses the epoch. The original Home lease
                // and selection remain pinned; recovery never redispatches.
                loop {
                    let frame = decode(
                        input
                            .recv()
                            .await
                            .ok_or_else(|| anyhow::anyhow!("downstream closed"))?,
                    );
                    let event_id = frame
                        .as_ref()
                        .ok()
                        .and_then(|frame| frame.get("event_id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let mut body = match frame.and_then(|frame| history.hydrate(frame, limit)) {
                        Ok(body) => body,
                        Err(_) => {
                            writer
                                .send(Message::Text(
                                    ws_recovery::continuation(&event_id).to_string().into(),
                                ))
                                .await?;
                            continue;
                        }
                    };
                    if body["model"].as_str() != Some(&alias) {
                        send_error(writer, "model_changed", "Reconnect to change the model")
                            .await?;
                        continue;
                    }
                    ws_home::authorize(&home, headers, Some(principal), Some(&alias)).await?;
                    if body
                        .get("prompt_cache_key")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
                        && let Some(key) =
                            crate::providers::codex::cache::provider_session(&execution, "")
                    {
                        body["prompt_cache_key"] = key.into();
                    }
                    super::codex::multiagent::prepare_boundary_home(
                        &home,
                        &home.config(),
                        "/v1/responses",
                        headers,
                        &mut body,
                    )
                    .await;
                    match ws_codex::attempt(&home.config(), &selection, headers, body.clone()) {
                        Ok((next, next_flags)) if next.native && next.message.len() <= limit => {
                            first = body;
                            prepared = next;
                            flags = next_flags;
                            break;
                        }
                        _ => {
                            send_error(
                                writer,
                                "unsupported_payload",
                                "Request cannot be prepared for native WebSocket transport",
                            )
                            .await?
                        }
                    }
                }
                continue;
            }
        }
        // Reporters settle before the socket-scoped lease is released.
        drop(owners);
        drop(lease);
        return outcome;
    }
}
