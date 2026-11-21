use super::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

// tungstenite's handshake callback fixes the error type to an unboxed HTTP response.
#[allow(clippy::result_large_err)]
#[tokio::test]
async fn native_upstream_handshake_and_responses_messages() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket =
            tokio_tungstenite::accept_hdr_async(stream, |request: &http::Request<()>, response| {
                assert_eq!(request.uri().path(), "/responses");
                assert_eq!(request.headers()["authorization"], "Bearer upstream-test");
                assert_eq!(
                    request.headers()["openai-beta"],
                    "responses_websockets=2026-02-06"
                );
                Ok(response)
            })
            .await
            .unwrap();
        let value: Value =
            serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(value["type"], "response.create");
        assert_eq!(value["input"], json!([]));
        socket.send(Message::Text(json!({"type":"response.completed","response":{"id":"r1","output":[],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}).to_string().into())).await.unwrap();
    });
    let config =
        Config::parse(b"credential-concurrency:\n  lifecycle-config-revision: 1\n").unwrap();
    let selection: Selection = serde_json::from_value(json!({"model":"m","provider":"codex","auth_index":"idx","user_api_key":"user","auth":{"id":"a","attributes":{"api_key":"upstream-test","base_url":format!("http://{address}")}}})).unwrap();
    let prepared = prepare(
        &config,
        &selection,
        &HeaderMap::new(),
        json!({"model":"m","input":[]}),
    )
    .unwrap();
    assert!(
        prepared
            .handshake
            .headers()
            .contains_key("sec-websocket-extensions")
    );
    let disabled = Config::parse(b"codex: {websocket-compression: false}\ncredential-concurrency:\n  lifecycle-config-revision: 1\n").unwrap();
    let plain = prepare(
        &disabled,
        &selection,
        &HeaderMap::new(),
        json!({"model":"m","input":[]}),
    )
    .unwrap();
    assert!(
        !plain
            .handshake
            .headers()
            .contains_key("sec-websocket-extensions")
    );
    let (mut socket, _) =
        super::super::ws_deflate::connect_async_with_config(prepared.handshake, None, true)
            .await
            .unwrap();
    socket
        .send(Message::Text(prepared.message.into()))
        .await
        .unwrap();
    let terminal: Value =
        serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(terminal["type"], "response.completed");
    assert_eq!(terminal["response"]["usage"]["total_tokens"], 5);
    server.await.unwrap();
}
