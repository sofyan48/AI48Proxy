use super::*;

const CALLER_KEY: &str = "local-caller-key";
const PROVIDER_KEY: &str = "local-provider-key";

async fn gateway_dispatch(home: &Home, base_url: &str) {
    let mut peer = home.accept().await;
    let args = command(&mut peer).await;
    assert_eq!(args.len(), 2);
    assert_eq!(args[0], b"RPOP");
    let request: Value = serde_json::from_slice(&args[1]).unwrap();
    assert_eq!(request["type"], "auth");
    assert_eq!(request["model"], "public-model");
    assert_eq!(request["concurrency_protocol"], 1);
    assert!(
        request
            .pointer("/headers/authorization/0")
            .and_then(Value::as_str)
            == Some(format!("Bearer {CALLER_KEY}").as_str()),
        "caller authentication was not passed to Home"
    );
    let selection = serde_json::json!({
        "model": "resolved-model", "provider": "openai", "auth_index": "local-index",
        "user_api_key": "caller-principal", "auth": {"id": "local-credential", "attributes": {
            "api_key": PROVIDER_KEY, "base_url": base_url
        }},
        "concurrency": {"accounted": true, "credential_id": "local-credential", "model": "ledger-model"}
    });
    bulk(&mut peer, &serde_json::to_vec(&selection).unwrap()).await;
}

async fn deliver_gateway_completion(home: &Home, client: &HomeClient, streaming: bool) {
    // Wake the real maintenance task without waiting for a wall-clock flush interval.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3600)).await;
    tokio::time::resume();
    bounded(async {
        for channel in ["concurrency-release", "usage", "in-flight-snapshot"] {
            let mut peer = home.accept().await;
            let mut args = command(&mut peer).await;
            if args == vec![b"CLUSTER".to_vec(), b"NODES".to_vec()] {
                bulk(&mut peer, br#"{"ok":true,"nodes":[]}"#).await;
                peer = home.accept().await;
                args = command(&mut peer).await;
            }
            assert_eq!(args.len(), 3);
            assert_eq!(args[0], b"LPUSH");
            assert_eq!(args[1], channel.as_bytes());
            let payload: Value = serde_json::from_slice(&args[2]).unwrap();
            if channel == "concurrency-release" {
                assert_eq!(payload["credential_id"], "local-credential");
                assert_eq!(payload["model"], "ledger-model");
                assert_eq!(payload["release_seq"], 1);
                assert_eq!(client.registry.pending_releases().len(), 1);
            } else if channel == "usage" {
                assert_eq!(client.reporting.len(), 1, "delivery requires an ACK");
                assert_eq!(payload["model"], "resolved-model");
                assert_eq!(payload["alias"], "public-model");
                assert_eq!(payload["provider"], "openai");
                assert_eq!(payload["auth_index"], "local-index");
                assert_eq!(payload["stream"], streaming);
                assert_eq!(payload["failed"], streaming);
                assert_eq!(payload["accounting_version"], 2);
                assert!(payload["api_key"].as_str() == Some("caller-principal"));
                for id in ["request_id", "execution_id"] {
                    assert!(
                        payload[id]
                            .as_str()
                            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
                    );
                }
                let encoded = payload.to_string();
                assert!(
                    !encoded.contains(CALLER_KEY),
                    "usage exposed caller authentication"
                );
                assert!(
                    !encoded.contains(PROVIDER_KEY),
                    "usage exposed upstream authentication"
                );
                assert!(payload.get("auth").is_none());
                assert!(payload.get("headers").is_none());
                if !streaming {
                    // Authoritative OpenAI source counts populate nonoverlapping v2 buckets.
                    assert_eq!(payload["tokens"]["input_tokens"], 11);
                    assert_eq!(payload["tokens"]["output_tokens"], 7);
                    assert_eq!(payload["tokens"]["total_tokens"], 18);
                    assert_eq!(payload["token_breakdown"]["quality"], "complete");
                    assert_eq!(payload["token_breakdown"]["unclassified_tokens"], 0);
                }
            }
            peer.get_mut().write_all(b":1\r\n").await.unwrap();
        }
        observed(|| client.reporting.is_empty() && client.registry.is_drained()).await;
    })
    .await;
    assert!(client.ready());
}

async fn gateway_round_trip(streaming: bool) {
    use axum::{
        body::Body,
        http::{HeaderMap, Request, StatusCode},
        response::IntoResponse,
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let home = Home::new().await;
    let (client, mut subscription) = home.start().await;
    // Keep health valid across the virtual maintenance tick, using the public update path.
    let snapshot = format!(
        "{}request-retry: 4\n",
        CONFIG.replace("timeout: 10s", "timeout: 2h")
    );
    publish(&mut subscription, "config", snapshot.as_bytes()).await;
    observed(|| client.config().request_retry == 4).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let expected = serde_json::json!({"id":"local-completion", "object":"chat.completion",
        "model":"resolved-model", "choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}});
    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::channel(1);
    let (chunks_tx, chunks_rx) =
        tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(1);
    let chunks = Arc::new(tokio::sync::Mutex::new(Some(chunks_rx)));
    let reply = expected.clone();
    let upstream = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(
            move |headers: HeaderMap, axum::Json(body): axum::Json<Value>| {
                let seen = seen_tx.clone();
                let chunks = chunks.clone();
                let reply = reply.clone();
                async move {
                    // Send only a boolean across the observation channel, never raw headers.
                    let authenticated = headers.get("authorization").and_then(|v| v.to_str().ok())
                        == Some(format!("Bearer {PROVIDER_KEY}").as_str());
                    seen.send((authenticated, body)).await.unwrap();
                    if streaming {
                        let rx = chunks.lock().await.take().unwrap();
                        (
                            [("content-type", "text/event-stream")],
                            Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)),
                        )
                            .into_response()
                    } else {
                        axum::Json(reply).into_response()
                    }
                }
            },
        ),
    );
    let stop = CancellationToken::new();
    let shutdown = stop.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, upstream)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .unwrap();
    });
    let first =
        bytes::Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n");
    chunks_tx.send(Ok(first.clone())).await.unwrap();
    let body = serde_json::json!({"model":"public-model", "messages":[{"role":"user","content":"hi"}], "stream":streaming});
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {CALLER_KEY}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let (response, ()) = bounded(async {
        tokio::join!(
            crate::gateway::router(client.clone()).oneshot(request),
            gateway_dispatch(&home, &base_url)
        )
    })
    .await;
    let response = response.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let (authenticated, received) = bounded(seen_rx.recv()).await.unwrap();
    assert!(
        authenticated,
        "upstream must receive only the selected provider authentication"
    );
    assert_eq!(received["model"], "resolved-model");
    assert_eq!(received["messages"], body["messages"]);
    assert_eq!(received["stream"], streaming);
    assert_eq!(client.registry.pending_admissions(), 0);
    if streaming {
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let mut downstream = response.into_body();
        let frame = bounded(downstream.frame()).await.unwrap().unwrap();
        assert_eq!(frame.into_data().unwrap(), first);
        assert_eq!(client.registry.active_count(), 1);
        assert!(client.registry.pending_releases().is_empty());
        assert!(client.reporting.is_empty());
        // The upstream channel is still open and has never sent EOF or [DONE].
        drop(downstream);
        observed(|| client.registry.active_count() == 0 && client.reporting.len() == 1).await;
        bounded(chunks_tx.closed()).await;
    } else {
        assert_eq!(response.headers()["content-type"], "application/json");
        let bytes = bounded(response.into_body().collect())
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), expected);
        assert_eq!(client.registry.active_count(), 0);
        assert_eq!(client.reporting.len(), 1);
    }
    assert_eq!(client.registry.pending_releases().len(), 1);
    deliver_gateway_completion(&home, &client, streaming).await;
    client.fence();
    drop(chunks_tx);
    stop.cancel();
    bounded(server).await.unwrap();
}

#[tokio::test]
async fn gateway_authenticated_openai_nonstream_delivers_usage_and_release() {
    gateway_round_trip(false).await;
}

#[tokio::test]
async fn gateway_sse_downstream_disconnect_completes_and_releases() {
    gateway_round_trip(true).await;
}
