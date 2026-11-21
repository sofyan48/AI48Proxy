//! Actual-router Codex stream bootstrap: in-stream overload fails over before commit.
use super::*;

const BUFFERING: &str = "request-retry: 2\ncodex: {stream-bootstrap-buffering: true, stream-bootstrap-timeout: 30s}\ncredential-concurrency:\n  lifecycle-config-revision: 7\n  cpa-heartbeat-timeout: 1h\n  release-flush-interval: 1h\n  release-max-backoff: 1h\n";

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect()
}

fn overloaded() -> String {
    sse(&[
        json!({"type":"response.created","response":{"id":"resp_a","object":"response","status":"in_progress"}}),
        json!({"type":"response.in_progress","response":{"id":"resp_a","object":"response","status":"in_progress"}}),
        json!({"type":"error","error":{"type":"service_unavailable_error","code":"server_is_overloaded","message":SECRET}}),
    ])
}

fn generated() -> String {
    sse(&[
        json!({"type":"response.created","response":{"id":"resp_b","object":"response","status":"in_progress"}}),
        json!({"type":"response.output_text.delta","delta":"hello"}),
        json!({"type":"response.completed","response":{"id":"resp_b","object":"response","status":"completed",
            "output":[],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}),
    ])
}

async fn serve(listener: &TcpListener, wire: &str) {
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(socket.read_u8().await.unwrap());
    }
    let length = String::from_utf8(bytes)
        .unwrap()
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.unwrap();
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{wire}", wire.len()).as_bytes()).await.unwrap();
}

async fn select(home: &Home, base: &str, credential: &str, round: u64, excluded: Value) {
    let mut peer = home.accept().await;
    let args = command(&mut peer).await;
    assert_eq!(args[0], b"RPOP");
    let dispatch: Value = serde_json::from_slice(&args[1]).unwrap();
    assert_eq!(dispatch["retry_round"], round);
    assert_eq!(dispatch["excluded_auth_ids"], excluded);
    bulk(
        &mut peer,
        &serde_json::to_vec(&json!({
            "provider":"codex", "model":"resolved-model", "auth_index":credential,
            "user_api_key":"home-authenticated-principal", "request_retry":2,
            "auth":{"id":credential, "attributes":{"api_key":SECRET,"base_url":base}},
            "concurrency":{"accounted":true,"credential_id":credential,"model":"ledger-model"}
        }))
        .unwrap(),
    )
    .await;
}

async fn ack_release(home: &Home, credential: &str) {
    let mut peer = home.accept().await;
    let args = command(&mut peer).await;
    assert_eq!(
        &args[..2],
        &[b"LPUSH".to_vec(), b"concurrency-release".to_vec()]
    );
    let release: Value = serde_json::from_slice(&args[2]).unwrap();
    assert_eq!(release["credential_id"], credential);
    peer.get_mut().write_all(b":1\r\n").await.unwrap();
}

fn request() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {CALLER}"))
        .body(Body::from(
            json!({"model":"public-alias","input":"hi","stream":true}).to_string(),
        ))
        .unwrap()
}

#[tokio::test]
async fn bootstrap_overload_fails_over_to_next_credential_before_headers() {
    bounded(async {
        let home = Home::new().await;
        let (client, _subscription) = tokio::join!(
            HomeClient::start(home.connector.clone(), Arc::new(ReportingQueue::default())),
            home.registration_with(BUFFERING),
        );
        let client = client.unwrap();
        assert!(client.config().providers.codex_stream_bootstrap_buffering);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/tenant/v1", listener.local_addr().unwrap());
        let home_exchange = async {
            select(&home, &base, "first", 0, json!([])).await;
            ack_release(&home, "first").await;
            select(&home, &base, "second", 1, json!(["first"])).await;
        };
        let upstream_exchange = async {
            serve(&listener, &overloaded()).await;
            serve(&listener, &generated()).await;
        };
        let (response, (), ()) = tokio::join!(
            crate::gateway::router(client.clone()).oneshot(request()),
            home_exchange,
            upstream_exchange,
        );
        let response = response.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        // Nothing from the rejected attempt reaches the client.
        assert!(!body.contains("resp_a") && !body.contains(SECRET));
        assert!(body.contains("resp_b") && body.contains("hello"));
        while client.reporting.len() < 2 {
            tokio::task::yield_now().await;
        }
        let batch = client.reporting.pending_batch(4, usize::MAX);
        assert_eq!(batch.len(), 2, "exactly one record per attempt");
        let first: Value = serde_json::from_slice(&batch[0].payload).unwrap();
        let second: Value = serde_json::from_slice(&batch[1].payload).unwrap();
        assert_eq!(first["auth_index"], "first");
        assert_eq!(first["failed"], true);
        assert_eq!(first["fail"]["status_code"], 503);
        assert_eq!(first["fail"]["code"], "", "overload is not request-scoped");
        assert_eq!(second["auth_index"], "second");
        assert_eq!(second["failed"], false);
        assert!(!String::from_utf8_lossy(&batch[0].payload).contains(SECRET));
        assert_eq!(client.registry.active_count(), 0);
        client.fence();
        client.retire().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn disabled_buffering_delivers_overload_in_stream_without_failover() {
    bounded(async {
        let home = Home::new().await;
        let (client, _subscription) = tokio::join!(
            HomeClient::start(home.connector.clone(), Arc::new(ReportingQueue::default())),
            home.registration(),
        );
        let client = client.unwrap();
        assert!(!client.config().providers.codex_stream_bootstrap_buffering);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/tenant/v1", listener.local_addr().unwrap());
        let wire = overloaded();
        let (response, (), ()) = tokio::join!(
            crate::gateway::router(client.clone()).oneshot(request()),
            select(&home, &base, "first", 0, json!([])),
            serve(&listener, &wire),
        );
        let response = response.unwrap();
        // Headers are committed on arrival; the failure surfaces inside the stream.
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        assert!(to_bytes(response.into_body(), 1 << 20).await.is_err());
        while client.reporting.is_empty() {
            tokio::task::yield_now().await;
        }
        let batch = client.reporting.pending_batch(4, usize::MAX);
        assert_eq!(batch.len(), 1);
        let usage: Value = serde_json::from_slice(&batch[0].payload).unwrap();
        assert_eq!(usage["failed"], true);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err(),
            "no failover attempt"
        );
        client.fence();
        client.retire().await.unwrap();
    })
    .await;
}
