//! RFC7692 transport adapter. Session policy remains in websocket.rs.
pub use ::tokio_tungstenite::tungstenite;
use futures_util::{Sink, Stream};
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tungstenite::Message;
use yawc::{Frame, OpCode, Options};

#[cfg(test)]
#[path = "ws_deflate_tests.rs"]
mod tests;

pub(super) fn options(enabled: bool, limit: usize) -> Options {
    let options = Options::default()
        // yawc uses exclusive bounds; tungstenite and the gateway use inclusive bounds.
        .with_limits(limit.saturating_add(1), limit.saturating_add(1))
        .with_utf8()
        .with_no_delay();
    if enabled {
        options
            .with_compression_level(yawc::CompressionLevel::default())
            .server_no_context_takeover()
            .client_no_context_takeover()
            .with_compression_threshold(0)
    } else {
        options.without_compression()
    }
}

pub(super) struct Socket<S, M> {
    inner: yawc::WebSocket<S>,
    marker: std::marker::PhantomData<M>,
    ended: bool,
}
impl<S, M> Socket<S, M> {
    fn new(inner: yawc::WebSocket<S>) -> Self {
        Self {
            inner,
            marker: std::marker::PhantomData,
            ended: false,
        }
    }
}

pub(super) trait WireMessage: Unpin {
    fn from_frame(frame: Frame) -> anyhow::Result<Self>
    where
        Self: Sized;
    fn into_frame(self) -> anyhow::Result<Frame>;
}
impl WireMessage for Message {
    fn from_frame(frame: Frame) -> anyhow::Result<Self> {
        let opcode = frame.opcode();
        let bytes = frame.into_payload();
        Ok(match opcode {
            OpCode::Text => Message::Text(String::from_utf8(bytes.to_vec())?.into()),
            OpCode::Binary => Message::Binary(bytes),
            OpCode::Ping => Message::Ping(bytes),
            OpCode::Pong => Message::Pong(bytes),
            OpCode::Close => Message::Close(if bytes.is_empty() {
                None
            } else {
                anyhow::ensure!(bytes.len() >= 2, "invalid close frame");
                Some(tungstenite::protocol::CloseFrame {
                    code: u16::from_be_bytes([bytes[0], bytes[1]]).into(),
                    reason: String::from_utf8(bytes[2..].to_vec())?.into(),
                })
            }),
            OpCode::Continuation => anyhow::bail!("unassembled continuation"),
        })
    }
    fn into_frame(self) -> anyhow::Result<Frame> {
        Ok(match self {
            Message::Text(text) => Frame::text(text.to_string()),
            Message::Binary(bytes) => Frame::binary(bytes),
            Message::Ping(bytes) => Frame::ping(bytes),
            Message::Pong(bytes) => Frame::pong(bytes),
            Message::Close(Some(close)) => {
                Frame::close(u16::from(close.code).into(), close.reason.as_bytes())
            }
            Message::Close(None) => Frame::close(yawc::close::CloseCode::Normal, []),
            Message::Frame(_) => anyhow::bail!("raw frames are not supported"),
        })
    }
}
impl WireMessage for axum::extract::ws::Message {
    fn from_frame(frame: Frame) -> anyhow::Result<Self> {
        use axum::extract::ws::{CloseFrame, Message as M};
        Ok(match Message::from_frame(frame)? {
            Message::Text(text) => M::Text(text.to_string().into()),
            Message::Binary(bytes) => M::Binary(bytes),
            Message::Ping(bytes) => M::Ping(bytes),
            Message::Pong(bytes) => M::Pong(bytes),
            Message::Close(close) => M::Close(close.map(|c| CloseFrame {
                code: c.code.into(),
                reason: c.reason.to_string().into(),
            })),
            _ => anyhow::bail!("unexpected frame"),
        })
    }
    fn into_frame(self) -> anyhow::Result<Frame> {
        use axum::extract::ws::Message as M;
        match self {
            M::Text(text) => Message::Text(text.to_string().into()),
            M::Binary(bytes) => Message::Binary(bytes),
            M::Ping(bytes) => Message::Ping(bytes),
            M::Pong(bytes) => Message::Pong(bytes),
            M::Close(close) => Message::Close(close.map(|c| tungstenite::protocol::CloseFrame {
                code: c.code.into(),
                reason: c.reason.to_string().into(),
            })),
        }
        .into_frame()
    }
}
impl<S: AsyncRead + AsyncWrite + Unpin, M: WireMessage> Stream for Socket<S, M> {
    type Item = Result<M, axum::Error>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Use the fallible API: yawc's Stream implementation hides protocol errors.
        let this = self.get_mut();
        if this.ended {
            return Poll::Ready(None);
        }
        this.inner.poll_next_frame(cx).map(|result| {
            let result = result.map_err(anyhow::Error::from).and_then(M::from_frame);
            if result.is_err() {
                this.ended = true;
            }
            Some(result.map_err(axum::Error::new))
        })
    }
}
impl<S: AsyncRead + AsyncWrite + Unpin, M: WireMessage> Sink<M> for Socket<S, M> {
    type Error = axum::Error;
    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().inner)
            .poll_ready(cx)
            .map_err(axum::Error::new)
    }
    fn start_send(self: Pin<&mut Self>, message: M) -> Result<(), Self::Error> {
        Pin::new(&mut self.get_mut().inner)
            .start_send(message.into_frame().map_err(axum::Error::new)?)
            .map_err(axum::Error::new)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().inner)
            .poll_flush(cx)
            .map_err(axum::Error::new)
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().inner)
            .poll_close(cx)
            .map_err(axum::Error::new)
    }
}

pub(super) type WebSocket = Socket<yawc::HttpStream, axum::extract::ws::Message>;
pub(crate) struct WebSocketUpgrade(yawc::IncomingUpgrade);
impl<S: Sync> axum::extract::FromRequestParts<S> for WebSocketUpgrade {
    type Rejection = http::StatusCode;
    async fn from_request_parts(
        parts: &mut http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let has_token = |name: &str, token: &str| {
            parts.headers.get_all(name).iter().any(|v| {
                v.to_str()
                    .is_ok_and(|s| s.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
            })
        };
        use base64::Engine;
        let valid_key = parts
            .headers
            .get("sec-websocket-key")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok())
            .is_some_and(|v| v.len() == 16);
        if parts.method != http::Method::GET
            || !has_token("connection", "upgrade")
            || !has_token("upgrade", "websocket")
            || !valid_key
        {
            return Err(http::StatusCode::BAD_REQUEST);
        }
        yawc::IncomingUpgrade::from_request_parts(parts, state)
            .await
            .map(Self)
    }
}
impl WebSocketUpgrade {
    pub(super) fn on_upgrade<F, Fut>(
        self,
        enabled: bool,
        limit: usize,
        callback: F,
    ) -> axum::response::Response
    where
        F: FnOnce(WebSocket) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        use axum::response::IntoResponse;
        match self.0.upgrade(options(enabled, limit)) {
            Ok((response, upgrade)) => {
                tokio::spawn(async move {
                    if let Ok(socket) = upgrade.await {
                        callback(Socket::new(socket)).await;
                    }
                });
                response.into_response()
            }
            Err(_) => http::StatusCode::BAD_REQUEST.into_response(),
        }
    }
}

// Signature-compatible entry point keeps acquisition timeout and session code unchanged.
pub(super) async fn connect_async_with_config(
    mut request: http::Request<()>,
    config: Option<tungstenite::protocol::WebSocketConfig>,
    _disable_nagle: bool,
) -> anyhow::Result<(
    Socket<yawc::MaybeTlsStream<tokio::net::TcpStream>, Message>,
    (),
)> {
    let enabled = request
        .headers_mut()
        .remove("sec-websocket-extensions")
        .is_some();
    let limit = config
        .and_then(|c| c.max_message_size)
        .unwrap_or(16 * 1024 * 1024);
    // yawc owns handshake-generated headers; forward only application headers.
    for name in [
        "host",
        "connection",
        "upgrade",
        "sec-websocket-key",
        "sec-websocket-version",
    ] {
        request.headers_mut().remove(name);
    }
    let url = url::Url::parse(&request.uri().to_string())?;
    let mut builder = http::Request::builder();
    *builder.headers_mut().expect("new builder") = request.headers().clone();
    let socket = yawc::WebSocket::connect(url)
        .with_request(builder)
        .with_options(options(enabled, limit))
        .await?;
    Ok((Socket::new(socket), ()))
}
