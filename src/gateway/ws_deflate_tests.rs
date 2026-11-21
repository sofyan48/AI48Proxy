use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::test]
async fn ws_deflate_rejects_unnegotiated_and_inflated_oversize() {
    for (enabled, limit) in [(false, 1024), (true, 32)] {
        let (mut peer, io) = tokio::io::duplex(4096);
        let inner = yawc::WebSocket::from_stream_with_extensions(
            io,
            yawc::Role::Client,
            enabled.then_some(
                "permessage-deflate; server_no_context_takeover; client_no_context_takeover",
            ),
            options(enabled, limit),
        )
        .unwrap();
        let mut socket = Socket::<_, Message>::new(inner);
        let wire = compressed();
        peer.write_all(&[0xc1, wire.len() as u8]).await.unwrap();
        peer.write_all(&wire).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), socket.next())
            .await
            .unwrap();
        assert!(result.unwrap().is_err());
    }
}

#[tokio::test]
async fn ws_deflate_fragmented_compressed_message_and_ping() {
    let (mut peer, io) = tokio::io::duplex(4096);
    let inner = yawc::WebSocket::from_stream_with_extensions(
        io,
        yawc::Role::Client,
        Some("permessage-deflate; server_no_context_takeover; client_no_context_takeover"),
        options(true, TEXT.len()),
    )
    .unwrap();
    let mut socket = Socket::<_, Message>::new(inner);
    let wire = compressed();
    let middle = wire.len() / 2;
    peer.write_all(&[0x41, middle as u8]).await.unwrap();
    peer.write_all(&wire[..middle]).await.unwrap();
    peer.write_all(&[0x89, 1, b'p']).await.unwrap();
    peer.write_all(&[0x80, (wire.len() - middle) as u8])
        .await
        .unwrap();
    peer.write_all(&wire[middle..]).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Text(text) => {
                    assert_eq!(text.as_str(), TEXT);
                    break;
                }
                Message::Ping(bytes) => assert_eq!(bytes.as_ref(), b"p"),
                message => panic!("unexpected {message:?}"),
            }
        }
    })
    .await
    .unwrap();
}

const TEXT: &str =
    "Codex compressed round trip: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

async fn headers(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(stream.read_u8().await.unwrap());
        assert!(bytes.len() < 16384);
    }
    String::from_utf8(bytes).unwrap()
}
fn compressed() -> Vec<u8> {
    let mut codec = flate2::Compress::new(flate2::Compression::default(), false);
    let mut bytes = Vec::with_capacity(1024);
    codec
        .compress_vec(TEXT.as_bytes(), &mut bytes, flate2::FlushCompress::Sync)
        .unwrap();
    assert!(bytes.ends_with(&[0, 0, 255, 255]));
    bytes.truncate(bytes.len() - 4);
    bytes
}
async fn send_wire(stream: &mut TcpStream, enabled: bool, masked: bool) {
    let mut bytes = if enabled {
        compressed()
    } else {
        TEXT.as_bytes().to_vec()
    };
    assert!(bytes.len() < 126);
    stream
        .write_all(&[
            if enabled { 0xc1 } else { 0x81 },
            bytes.len() as u8 | if masked { 128 } else { 0 },
        ])
        .await
        .unwrap();
    if masked {
        let mask = [17, 29, 37, 41];
        stream.write_all(&mask).await.unwrap();
        for (i, b) in bytes.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
    }
    stream.write_all(&bytes).await.unwrap();
}
async fn read_wire(stream: &mut TcpStream, enabled: bool, masked: bool) {
    let first = stream.read_u8().await.unwrap();
    assert_eq!(
        first,
        if enabled { 0xc1 } else { 0x81 },
        "RSV1 proves actual wire compression"
    );
    let second = stream.read_u8().await.unwrap();
    assert_eq!(second & 128 != 0, masked);
    let mut size = (second & 127) as usize;
    if size == 126 {
        size = stream.read_u16().await.unwrap() as usize;
    }
    assert!(size < 1024);
    let mut mask = [0; 4];
    if masked {
        stream.read_exact(&mut mask).await.unwrap();
    }
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).await.unwrap();
    if masked {
        for (i, b) in bytes.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
    }
    if enabled {
        bytes.extend_from_slice(&[0, 0, 255, 255]);
        let mut output = Vec::with_capacity(1024);
        flate2::Decompress::new(false)
            .decompress_vec(&bytes, &mut output, flate2::FlushDecompress::Sync)
            .unwrap();
        bytes = output;
    }
    assert_eq!(bytes, TEXT.as_bytes());
}

#[tokio::test]
async fn ws_deflate_upstream_wire_roundtrip_on_off_and_declined() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        for (offer, accept) in [(true, true), (false, false), (true, false)] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = headers(&mut stream).await;
                assert_eq!(request.to_lowercase().contains("permessage-deflate"), offer);
                assert!(request.to_lowercase().contains("authorization: bearer test"));
                let key = request.lines().find_map(|line| line.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-key")).map(|(_, v)| v.trim())).unwrap();
                let accept_key = tungstenite::handshake::derive_accept_key(key.as_bytes());
                let extension = if accept { "Sec-WebSocket-Extensions: permessage-deflate; server_no_context_takeover; client_no_context_takeover\r\n" } else { "" };
                stream.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept_key}\r\n{extension}\r\n").as_bytes()).await.unwrap();
                for _ in 0..2 {
                    read_wire(&mut stream, accept, true).await;
                    send_wire(&mut stream, accept, false).await;
                }
            });
            use tungstenite::client::IntoClientRequest;
            let mut request = format!("ws://{address}/responses").into_client_request().unwrap();
            request.headers_mut().insert("authorization", "Bearer test".parse().unwrap());
            if offer { request.headers_mut().insert("sec-websocket-extensions", "permessage-deflate".parse().unwrap()); }
            let (mut socket, _) = connect_async_with_config(request, None, true).await.unwrap();
            for _ in 0..2 {
                socket.send(Message::Text(TEXT.into())).await.unwrap();
                assert_eq!(socket.next().await.unwrap().unwrap().to_text().unwrap(), TEXT);
            }
            server.await.unwrap();
        }
    }).await.unwrap();
}

#[tokio::test]
async fn ws_deflate_downstream_wire_roundtrip_on_off_and_unoffered() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        for (enabled, offer) in [(true, true), (false, true), (true, false)] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let app = axum::Router::new().route("/", axum::routing::get(move |ws: WebSocketUpgrade| async move {
                ws.on_upgrade(enabled, 1024, |mut socket| async move {
                    for _ in 0..2 {
                        let message = socket.next().await.unwrap().unwrap();
                        assert!(matches!(&message, axum::extract::ws::Message::Text(t) if t.as_str() == TEXT));
                        socket.send(message).await.unwrap();
                    }
                })
            }));
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
            let mut stream = TcpStream::connect(address).await.unwrap();
            let extension = if offer { "Sec-WebSocket-Extensions: permessage-deflate; client_no_context_takeover; server_no_context_takeover\r\n" } else { "" };
            stream.write_all(format!("GET / HTTP/1.1\r\nHost: {address}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{extension}\r\n").as_bytes()).await.unwrap();
            let response = headers(&mut stream).await;
            assert!(response.starts_with("HTTP/1.1 101"));
            assert_eq!(response.contains("permessage-deflate"), enabled && offer);
            for _ in 0..2 {
                send_wire(&mut stream, enabled && offer, true).await;
                read_wire(&mut stream, enabled && offer, false).await;
            }
            server.abort();
        }
    }).await.unwrap();
}
