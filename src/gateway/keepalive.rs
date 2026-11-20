//! Downstream keepalives operate after translation and never enter usage accounting.
#[cfg(test)]
#[path = "keepalive_tests.rs"]
mod tests;
use axum::{body::Body, response::Response};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use http::HeaderMap;
use std::{future::Future, io, time::Duration};
use tokio::time::{Instant, sleep_until};

pub const SSE_COMMENT: &[u8] = b": keep-alive\n\n";

/// A response body adapter, not a request deadline. Partial SSE frames must finish
/// before comments can be inserted, even if the upstream pauses mid-frame.
pub fn body(response: Response, interval: Duration, sse: bool, before_first: bool) -> Response {
    if interval.is_zero() {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    parts.headers.remove("content-length");
    let source = body.into_data_stream();
    let state = (source, Instant::now() + interval, false, Vec::new());
    let output = stream::unfold(
        state,
        move |(mut source, mut deadline, mut seen, mut tail)| async move {
            let eligible = (!sse || tail.is_empty()) && (seen || before_first);
            tokio::select! {
                biased;
                next = source.next() => {
                    let next = next?;
                    if let Ok(bytes) = &next
                        && !bytes.is_empty() {
                            seen = true;
                            deadline = Instant::now() + interval;
                            if sse {
                                for byte in bytes {
                                    tail.push(*byte);
                                    if tail.ends_with(b"\n\n") || tail.ends_with(b"\r\n\r\n") {
                                        tail.clear();
                                    } else if tail.len() > 4 {
                                        tail.remove(0);
                                    }
                                }
                            }
                    }
                    Some((next.map_err(io::Error::other), (source, deadline, seen, tail)))
                }
                _ = sleep_until(deadline), if eligible => {
                    deadline = Instant::now() + interval;
                    let bytes = Bytes::from_static(if sse { SSE_COMMENT } else { b" " });
                    Some((Ok(bytes), (source, deadline, seen, tail)))
                }
            }
        },
    );
    Response::from_parts(parts, Body::from_stream(output))
}

/// Keep a non-stream request alive while waiting for headers/body, including
/// dispatch. Once whitespace commits HTTP 200, a later error is delivered as JSON
/// in that response, just as CPA cannot change status after its first flush.
pub async fn nonstream_wait(
    future: impl Future<Output = Response> + Send + 'static,
    interval: Duration,
) -> Response {
    if interval.is_zero() {
        return future.await;
    }
    let mut future = Box::pin(future);
    tokio::select! {
        biased;
        response = &mut future => response,
        _ = tokio::time::sleep(interval) => {
            let output = stream::unfold((future, None::<axum::body::BodyDataStream>, true), move |(mut future, mut source, first)| async move {
                if first {
                    return Some((Ok::<_, io::Error>(Bytes::from_static(b" ")), (future, source, false)));
                }
                loop {
                    if let Some(mut active) = source.take() {
                        return active.next().await.map(|next| (next.map_err(io::Error::other), (future, Some(active), false)));
                    }
                    tokio::select! {
                        biased;
                        response = &mut future => { source = Some(response.into_body().into_data_stream()); }
                        _ = tokio::time::sleep(interval) => {
                            return Some((Ok(Bytes::from_static(b" ")), (future, source, false)));
                        }
                    }
                }
            });
            let mut response = Response::new(Body::from_stream(output));
            response.headers_mut().insert("content-type", "application/json".parse().unwrap());
            response
        }
    }
}

/// Wait for upstream headers without spawning detached work. The first tick
/// commits SSE; dropping this body drops the in-flight execution future.
pub async fn stream_wait(
    future: impl Future<Output = Response> + Send + 'static,
    settings: &crate::config::Streaming,
    path: &str,
) -> Response {
    if !settings.send_keepalive_before_ft || settings.keepalive_interval.is_zero() {
        return future.await;
    }
    let interval = settings.keepalive_interval;
    let path = path.to_owned();
    let mut future = Box::pin(future);
    tokio::select! {
        biased;
        response = &mut future => {
            if !response.status().is_success() { return response; }
            committed(response, &path)
        }
        _ = tokio::time::sleep(interval) => {
            let output = stream::unfold((Some(future), None::<axum::body::BodyDataStream>, true), move |(mut pending, mut source, first)| {
                let path = path.clone();
                async move {
                    if first {
                        return Some((Ok::<_, io::Error>(Bytes::from_static(SSE_COMMENT)), (pending, source, false)));
                    }
                    loop {
                        if let Some(mut active) = source.take() {
                            return active.next().await.map(|next| (next.map_err(io::Error::other), (pending, Some(active), false)));
                        }
                        let future = pending.as_mut()?;
                        tokio::select! {
                            biased;
                            response = future => {
                                pending.take();
                                source = Some(committed(response, &path).into_body().into_data_stream());
                            }
                            _ = tokio::time::sleep(interval) => {
                                return Some((Ok(Bytes::from_static(SSE_COMMENT)), (pending, source, false)));
                            }
                        }
                    }
                }
            });
            let mut response = Response::new(Body::from_stream(output));
            response.headers_mut().insert("content-type", "text/event-stream".parse().unwrap());
            response.headers_mut().insert("cache-control", "no-cache".parse().unwrap());
            response
        }
    }
}

/// Errors after commitment use the route's Go SSE envelope. Only static safe
/// messages are rendered; raw provider errors and bodies are never forwarded.
fn committed(response: Response, path: &str) -> Response {
    let (parts, body) = response.into_parts();
    if !parts.status.is_success() {
        return Response::new(Body::from(sse_error(path, parts.status.as_u16())));
    }
    let path = path.to_owned();
    let source = body.into_data_stream();
    let output = stream::unfold(
        (source, false, Vec::new()),
        move |(mut source, done, mut tail)| {
            let path = path.clone();
            async move {
                if done {
                    return None;
                }
                source.next().await.map(|next| match next {
                    Ok(bytes) => {
                        for byte in &bytes {
                            tail.push(*byte);
                            if tail.ends_with(b"\n\n") || tail.ends_with(b"\r\n\r\n") {
                                tail.clear();
                            } else if tail.len() > 4 {
                                tail.remove(0);
                            }
                        }
                        (Ok::<_, io::Error>(bytes), (source, false, tail))
                    }
                    Err(_) => {
                        let separator = if tail.is_empty() {
                            ""
                        } else if tail.ends_with(b"\n") {
                            "\n"
                        } else {
                            "\n\n"
                        };
                        (
                            Ok(Bytes::from(format!("{separator}{}", sse_error(&path, 502)))),
                            (source, true, tail),
                        )
                    }
                })
            }
        },
    );
    Response::from_parts(parts, Body::from_stream(output))
}

fn sse_error(path: &str, status: u16) -> String {
    let (kind, code) = match status {
        401 => ("authentication_error", "invalid_api_key"),
        403 => ("permission_error", "insufficient_quota"),
        429 => ("rate_limit_error", "rate_limit_exceeded"),
        404 => ("invalid_request_error", "model_not_found"),
        _ => ("server_error", "internal_server_error"),
    };
    let error = serde_json::json!({"type":kind,"code":code,"message":"Upstream request failed"});
    if path == "/v1/messages" {
        format!(
            "event: error\ndata: {}\n\n",
            serde_json::json!({"type":"error","error":{"type":"api_error","message":"Upstream request failed"}})
        )
    } else if path.starts_with("/v1/responses") {
        format!(
            "event: error\ndata: {}\n\n",
            serde_json::json!({"type":"error","error":{"type":kind,"code":code,"message":"Upstream request failed","param":null},"sequence_number":0})
        )
    } else if path.starts_with("/v1beta/") {
        format!(
            "event: error\ndata: {}\n\n",
            serde_json::json!({"error":error})
        )
    } else {
        format!("data: {}\n\n", serde_json::json!({"error":error}))
    }
}

/// CPA response passthrough is denylisted and cannot overwrite handler headers.
pub fn passthrough(dst: &mut HeaderMap, src: &HeaderMap) {
    let scoped: Vec<_> = src
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .map(|s| s.trim().to_ascii_lowercase())
        .collect();
    for name in src.keys() {
        let key = name.as_str();
        if dst.contains_key(name)
            || scoped.iter().any(|s| s == key)
            || matches!(
                key,
                "connection"
                    | "keep-alive"
                    | "proxy-authenticate"
                    | "proxy-authorization"
                    | "te"
                    | "trailer"
                    | "transfer-encoding"
                    | "upgrade"
                    | "set-cookie"
                    | "content-length"
                    | "content-encoding"
                    | "access-control-allow-credentials"
                    | "access-control-allow-headers"
                    | "access-control-allow-methods"
                    | "access-control-allow-origin"
                    | "access-control-expose-headers"
                    | "access-control-max-age"
                    | "x-cpa-trace-id"
            )
            || [
                "x-litellm-",
                "helicone-",
                "x-portkey-",
                "cf-aig-",
                "x-kong-",
                "x-bt-",
            ]
            .iter()
            .any(|p| key.starts_with(p))
        {
            continue;
        }
        for value in src.get_all(name) {
            dst.append(name.clone(), value.clone());
        }
    }
}
