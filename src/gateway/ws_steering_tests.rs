use super::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

fn selection(base: &str) -> Selection {
    serde_json::from_value(json!({"model":"m","provider":"codex","auth_index":"idx","user_api_key":"user",
        "auth":{"id":"a","attributes":{"api_key":"upstream-test","base_url":base,"websockets":true}}}))
    .unwrap()
}

fn config(extra: &str) -> Config {
    Config::parse(
        format!("credential-concurrency:\n  lifecycle-config-revision: 1\n{extra}").as_bytes(),
    )
    .unwrap()
}

#[test]
fn accepted_steer_owns_successor_and_blocks_creates_until_it_ends() {
    let mut s = Steering::new(true);
    s.created(&json!({"response":{"id":"r1"}}), true).unwrap();
    s.submitted("r1", "e1");
    assert!(!s.ready(true), "unacknowledged steer blocks creates");
    s.observe(
        &json!({"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}),
    );
    assert!(!s.ready(true), "accepted steer awaits its successor");
    assert!(!s.create_parent_ok("other"));
    s.created(
        &json!({"response":{"id":"r2","previous_response_id":"r1"}}),
        false,
    )
    .unwrap();
    assert!(!s.ready(true) && s.ready(false));
    s.ended("r2");
    assert!(s.accepted.is_empty());
}

#[test]
fn unowned_automatic_successor_and_failed_steer_fail_closed() {
    let mut s = Steering::new(true);
    s.created(&json!({"response":{"id":"r1"}}), true).unwrap();
    assert!(
        s.created(
            &json!({"response":{"id":"rx","previous_response_id":"r1"}}),
            false
        )
        .is_err()
    );
    s.submitted("r1", "e1");
    // An id-less ack falls back to the latest response as its parent.
    s.observe(&json!({"type":"response.steer.failed","steer":{"id":"s1"}}));
    assert!(s.ready(false));
}

#[test]
fn pending_steer_allows_only_the_waiting_parent_continuation() {
    let mut s = Steering::new(true);
    s.created(&json!({"response":{"id":"r1"}}), true).unwrap();
    s.submitted("r1", "e1");
    s.observe(
        &json!({"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}),
    );
    s.observe(
        &json!({"type":"response.steer.pending","steer":{"id":"s1","previous_response_id":"r1"}}),
    );
    assert!(s.ready(false));
    assert!(s.create_parent_ok("r1") && !s.create_parent_ok("r0"));
}

#[test]
fn queue_is_bounded_and_disabled_steering_never_pops() {
    let mut s = Steering::new(true);
    for _ in 0..MAX_QUEUED {
        s.enqueue(json!({"type":"response.create"})).unwrap();
    }
    assert!(s.enqueue(json!({})).is_err());
    assert!(s.pop_ready(true).is_none());
    assert!(s.pop_ready(false).is_some());
    let mut off = Steering::new(false);
    off.enqueue(json!({})).unwrap();
    assert!(off.pop_ready(true).is_none());
}

#[test]
fn steer_frame_keeps_unknown_fields_and_applies_final_rules_once() {
    let config = config(
        "payload:\n  override:\n    - models: [{name: '*', protocol: codex}]\n      params: {custom_final: 7}\n",
    );
    let (message, parent) = prepare(
        &config,
        &selection("http://127.0.0.1:9"),
        &HeaderMap::new(),
        "m",
        json!({"type":"response.steer","previous_response_id":"r1","event_id":"e1","input":[{"role":"user","content":"steer"}],"future":true}),
        1 << 20,
    )
    .unwrap();
    let value: Value = serde_json::from_str(&message).unwrap();
    assert_eq!(parent, "r1");
    assert_eq!(value["type"], "response.steer");
    assert_eq!(value["event_id"], "e1");
    assert_eq!(value["future"], true);
    assert_eq!(value["custom_final"], 7);
    assert!(value.get("stream").is_none() && value.get("store").is_none());
    for bad in [
        json!({"type":"response.create"}),
        json!({"type":"response.steer","previous_response_id":1}),
    ] {
        assert!(
            prepare(
                &config,
                &selection("http://x"),
                &HeaderMap::new(),
                "m",
                bad,
                1 << 20
            )
            .is_err()
        );
    }
}

#[test]
fn steering_settings_default_off() {
    assert!(!config("").providers.codex_response_steering);
    assert!(config("").providers.codex_websocket_compression);
    assert!(
        config("codex: {response-steering: true}\n")
            .providers
            .codex_response_steering
    );
}

// tungstenite's handshake callback fixes the error type to an unboxed HTTP response.
#[allow(clippy::result_large_err)]
#[tokio::test]
async fn duplex_steer_and_successor_share_one_upstream_socket() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket =
            tokio_tungstenite::accept_hdr_async(stream, |request: &http::Request<()>, response| {
                // The worker never offers permessage-deflate (Go websocket-compression: false).
                assert!(!request.headers().contains_key("sec-websocket-extensions"));
                Ok(response)
            })
            .await
            .unwrap();
        let mut next = async || -> Value {
            serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap()
        };
        assert_eq!(next().await["type"], "response.create");
        let steer = next().await;
        assert_eq!(steer["type"], "response.steer");
        assert_eq!(steer["previous_response_id"], "r1");
        for event in [
            json!({"type":"response.created","response":{"id":"r1"}}),
            json!({"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}),
            json!({"type":"response.completed","response":{"id":"r1","output":[]}}),
            json!({"type":"response.created","response":{"id":"r2","previous_response_id":"r1"}}),
            json!({"type":"response.completed","response":{"id":"r2","output":[]}}),
        ] {
            socket
                .send(Message::Text(event.to_string().into()))
                .await
                .unwrap();
        }
    });
    let config = config("codex: {response-steering: true, websocket-compression: false}\n");
    let selection = selection(&format!("http://{address}"));
    let create = super::super::ws_transport::prepare(
        &config,
        &selection,
        &HeaderMap::new(),
        json!({"model":"m","input":[]}),
    )
    .unwrap();
    let (mut upstream, _) = tokio_tungstenite::connect_async(create.handshake)
        .await
        .unwrap();
    upstream
        .send(Message::Text(create.message.into()))
        .await
        .unwrap();
    let mut steering = Steering::new(config.providers.codex_response_steering);
    let (steer, parent) = prepare(
        &config,
        &selection,
        &HeaderMap::new(),
        "m",
        json!({"type":"response.steer","previous_response_id":"r1","input":[]}),
        1 << 20,
    )
    .unwrap();
    steering.submitted(&parent, "");
    upstream.send(Message::Text(steer.into())).await.unwrap();
    steering
        .enqueue(json!({"type":"response.create","input":[]}))
        .unwrap();
    let mut explicit = true;
    let mut ended = Vec::new();
    while let Some(Ok(Message::Text(text))) = upstream.next().await {
        let event: Value = serde_json::from_str(text.as_str()).unwrap();
        match event["type"].as_str().unwrap() {
            kind if kind.starts_with("response.steer.") => steering.observe(&event),
            "response.created" => {
                steering.created(&event, explicit).unwrap();
                explicit = false;
            }
            _ => {
                let id = event["response"]["id"].as_str().unwrap().to_owned();
                steering.ended(&id);
                // The queued create stays held until the successor has ended.
                assert_eq!(steering.pop_ready(id == "r1").is_some(), id == "r2");
                ended.push(id);
            }
        }
    }
    assert_eq!(ended, ["r1", "r2"]);
    server.await.unwrap();
}
