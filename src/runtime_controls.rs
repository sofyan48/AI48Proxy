//! Opt-in runtime consumers. The gateway must wire these before advertising support.
//!
//! Wrap the *downstream encoded* stream, after translation and usage accounting.
//! Heartbeats must never be parsed as upstream usage or counted as first tokens.
use crate::config::Config;
use axum::body::{Body, to_bytes};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use std::{fmt, future::Future, pin::Pin, time::Duration};
use tokio::time::{Instant, sleep_until};

#[derive(Clone, Debug)]
pub struct RuntimeControls {
    pub max_request_body_bytes: Option<usize>,
    pub tls_handshake_timeout: Duration,
    pub streaming_keepalive_interval: Duration,
    pub send_keepalive_before_ft: bool,
    pub nonstream_keepalive_interval: Duration,
}

impl From<&Config> for RuntimeControls {
    fn from(config: &Config) -> Self {
        Self {
            max_request_body_bytes: config.max_request_body_bytes,
            tls_handshake_timeout: config.tls_handshake_timeout,
            streaming_keepalive_interval: config.streaming.keepalive_interval,
            send_keepalive_before_ft: config.streaming.send_keepalive_before_ft,
            nonstream_keepalive_interval: config.nonstream_keepalive_interval,
        }
    }
}

#[derive(Debug)]
pub enum BodyReadError {
    TooLarge,
    Read(axum::Error),
}

impl fmt::Display for BodyReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => f.write_str("request body exceeds configured limit"),
            Self::Read(_) => f.write_str("failed to read request body"),
        }
    }
}
impl std::error::Error for BodyReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::TooLarge => None,
        }
    }
}

/// Preserves transport errors without conflating them with an expired handshake.
#[derive(Debug)]
pub enum HandshakeError<E> {
    Transport(E),
    Timeout,
}
impl<E: fmt::Display> fmt::Display for HandshakeError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(_) => f.write_str("upstream connection or TLS handshake failed"),
            Self::Timeout => f.write_str("upstream TLS handshake timed out"),
        }
    }
}
impl<E: std::error::Error + 'static> std::error::Error for HandshakeError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Timeout => None,
        }
    }
}

impl RuntimeControls {
    /// Enforce both declared and actual sizes, including chunked bodies. Call
    /// before JSON decoding, Home dispatch, or any upstream work. Map TooLarge to
    /// HTTP 413; a read error is not a size error. None intentionally has no cap.
    pub async fn read_body(
        &self,
        body: Body,
        declared_length: Option<u64>,
    ) -> Result<Bytes, BodyReadError> {
        if self.max_request_body_bytes.is_some_and(|limit| {
            declared_length.is_some_and(|length| u128::from(length) > limit as u128)
        }) {
            return Err(BodyReadError::TooLarge);
        }
        to_bytes(body, self.max_request_body_bytes.unwrap_or(usize::MAX))
            .await
            .map_err(|error| {
                let mut source: &(dyn std::error::Error + 'static) = &error;
                loop {
                    if source.is::<http_body_util::LengthLimitError>() {
                        return BodyReadError::TooLarge;
                    }
                    match source.source() {
                        Some(next) => source = next,
                        None => return BodyReadError::Read(error),
                    }
                }
            })
    }

    /// Apply only to the TLS negotiation future, after TCP/proxy CONNECT has
    /// completed. Dropping a timed-out future closes its owned transport. Never
    /// wrap request send, headers, response bodies, or stream reads with this.
    pub async fn handshake<T, E>(
        &self,
        handshake: impl Future<Output = Result<T, E>>,
    ) -> Result<T, HandshakeError<E>> {
        if self.tls_handshake_timeout.is_zero() {
            return handshake.await.map_err(HandshakeError::Transport);
        }
        tokio::time::timeout(self.tls_handshake_timeout, handshake)
            .await
            .map_err(|_| HandshakeError::Timeout)?
            .map_err(HandshakeError::Transport)
    }

    /// DNS, TCP and proxy CONNECT remain unbounded by the handshake setting.
    /// Reqwest ClientBuilder::connect_timeout is NOT equivalent to this helper.
    pub async fn connect_then_handshake<T, U, E, C, H, F>(
        &self,
        connect: C,
        handshake: H,
    ) -> Result<U, HandshakeError<E>>
    where
        C: Future<Output = Result<T, E>>,
        H: FnOnce(T) -> F,
        F: Future<Output = Result<U, E>>,
    {
        let transport = connect.await.map_err(HandshakeError::Transport)?;
        self.handshake(handshake(transport)).await
    }

    /// Input items must be complete downstream frames, not partial SSE lines.
    /// first_token_seen describes the gateway's already-consumed bootstrap item.
    /// For pre-first-token keepalives, the input must include the pending bootstrap
    /// work too; wrapping only response.bytes_stream() cannot cover header waits.
    pub fn streaming<S, E>(
        &self,
        input: S,
        first_token_seen: bool,
    ) -> Pin<Box<dyn Stream<Item = Result<Bytes, E>> + Send>>
    where
        S: Stream<Item = Result<Bytes, E>> + Send + 'static,
        E: Send + 'static,
    {
        keepalive(
            input,
            self.streaming_keepalive_interval,
            first_token_seen || self.send_keepalive_before_ft,
            Bytes::from_static(b": keep-alive\n\n"),
        )
    }

    /// Blank lines are legal whitespace before a JSON response. Enabling this
    /// commits HTTP status/headers early: the gateway must encode subsequent
    /// errors in the body and must not attempt an HTTP-status retry after commit.
    /// Input should include the pending execution future, not just the final body.
    pub fn nonstream<S, E>(&self, input: S) -> Pin<Box<dyn Stream<Item = Result<Bytes, E>> + Send>>
    where
        S: Stream<Item = Result<Bytes, E>> + Send + 'static,
        E: Send + 'static,
    {
        keepalive(
            input,
            self.nonstream_keepalive_interval,
            true,
            Bytes::from_static(b"\n"),
        )
    }
}

fn keepalive<S, E>(
    input: S,
    interval: Duration,
    active: bool,
    heartbeat: Bytes,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, E>> + Send>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
{
    // No spawned task, queue, or eager read: dropping the body drops the input
    // immediately. Each actual write resets the idle interval (no burst catchup).
    let state = (Box::pin(input), active, false, Instant::now() + interval);
    Box::pin(futures_util::stream::unfold(
        state,
        move |(mut input, mut active, ended, deadline)| {
            let heartbeat = heartbeat.clone();
            async move {
                if ended {
                    return None;
                }
                let item = if interval.is_zero() || !active {
                    input.next().await
                } else {
                    tokio::select! {
                        biased;
                        item = input.next() => item,
                        _ = sleep_until(deadline) => Some(Ok(heartbeat)),
                    }
                };
                let item = item?;
                // Empty upstream chunks do not count as first-token progress.
                if matches!(&item, Ok(bytes) if !bytes.is_empty()) {
                    active = true;
                }
                let ended = item.is_err();
                Some((item, (input, active, ended, Instant::now() + interval)))
            }
        },
    ))
}

#[cfg(test)]
#[path = "config/runtime_tests.rs"]
mod tests;
