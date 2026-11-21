use super::super::*;
use super::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn ws_recovery_classifies_only_transport_loss() {
    for kind in [
        io::ErrorKind::UnexpectedEof,
        io::ErrorKind::BrokenPipe,
        io::ErrorKind::ConnectionReset,
    ] {
        assert!(transport(axum::Error::new(io::Error::from(kind))).is::<Lost>());
    }
    for error in [
        yawc::WebSocketError::FrameTooLarge,
        yawc::WebSocketError::InvalidUTF8,
        yawc::WebSocketError::InvalidStatusCode(429),
    ] {
        assert!(!transport(axum::Error::new(error)).is::<Lost>());
    }
}

#[test]
fn ws_recovery_settles_once_and_fences_history() {
    struct Reporter(Arc<AtomicUsize>);
    impl Drop for Reporter {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let count = Arc::new(AtomicUsize::new(0));
    let mut owners = ws_codex::Owners::default();
    owners.pending.push_back(ws_codex::Owner {
        request: json!({"event_id":"c1"}),
        flags: Default::default(),
        automatic: false,
        turn: Reporter(count.clone()),
    });
    owners
        .active
        .insert("r1".into(), owners.pending.pop_front().unwrap());
    let mut steering = ws_steering::Steering::new(true);
    steering.submitted("r1", "s1");
    steering.enqueue(json!({"event_id":"c2"})).unwrap();
    let losses = abandon(&mut owners, &mut steering, None);
    assert_eq!(losses.len(), 3);
    assert_eq!(losses[2]["error"]["code"], "upstream_websocket_not_sent");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(abandon(&mut owners, &mut steering, None).is_empty());
    let mut history = ws_state::History::default();
    history.native = true;
    let body = json!({"model":"m","input":[]});
    history.submitted(&body);
    history.created("r1");
    history.disconnected();
    for frame in [
        json!({"type":"response.create","previous_response_id":"r1","input":[]}),
        json!({"type":"response.append","input":[]}),
    ] {
        assert!(history.hydrate(frame, 10000).is_err());
    }
    let fresh = history
        .hydrate(json!({"type":"response.create","input":[]}), 10000)
        .unwrap();
    assert_eq!(fresh["model"], "m");
    assert!(fresh.get("previous_response_id").is_none());
    history.submitted(&fresh);
    assert!(
        history
            .hydrate(
                json!({"type":"response.create","previous_response_id":"r1","input":[]}),
                10000
            )
            .is_err()
    );
    history.created("r2");
    assert!(
        history
            .hydrate(
                json!({"type":"response.create","previous_response_id":"r2","input":[]}),
                10000
            )
            .is_ok()
    );
}

#[tokio::test]
async fn ws_recovery_local_socket_transport_loss_is_not_replayed() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = ::tokio_tungstenite::accept_async(tcp).await.unwrap();
            let first = socket.next().await.unwrap().unwrap();
            assert!(first.to_text().unwrap().contains("c1"));
            drop(socket); // no close frame: Go's recoverable EOF classification
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = ::tokio_tungstenite::accept_async(tcp).await.unwrap();
            let next = socket.next().await.unwrap().unwrap();
            assert!(next.to_text().unwrap().contains("c2"));
            socket
                .send(UpMessage::Text(
                    json!({"type":"response.completed","response":{"id":"r2","output":[]}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        });
        let request = || {
            use ::tokio_tungstenite::tungstenite::client::IntoClientRequest;
            format!("ws://{address}/responses")
                .into_client_request()
                .unwrap()
        };
        let (mut socket, _) = ws_deflate::connect_async_with_config(request(), None, true)
            .await
            .unwrap();
        socket
            .send(UpMessage::Text("{\"event_id\":\"c1\"}".into()))
            .await
            .unwrap();
        let error = socket.next().await.unwrap().unwrap_err();
        let classified = transport(error);
        assert!(classified.is::<Lost>(), "{classified:?}");
        drop(socket);
        // A separate explicit create, not c1, is the only input on the new socket.
        let (mut socket, _) = ws_deflate::connect_async_with_config(request(), None, true)
            .await
            .unwrap();
        socket
            .send(UpMessage::Text("{\"event_id\":\"c2\"}".into()))
            .await
            .unwrap();
        assert!(
            socket
                .next()
                .await
                .unwrap()
                .unwrap()
                .to_text()
                .unwrap()
                .contains("r2")
        );
        server.await.unwrap();
    })
    .await
    .unwrap();
}
