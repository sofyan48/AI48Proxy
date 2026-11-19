use super::*;
use crate::home::resp;
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use std::{future::Future, time::Duration};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};

type Peer = BufReader<TlsStream<TcpStream>>;

// Keep maintenance outside the virtual-time heartbeat window. No live Home is used.
const CONFIG: &str = "host: initial\nport: 8317\ncredential-concurrency:\n  lifecycle-config-revision: 7\n  cpa-heartbeat-timeout: 10s\n  release-flush-interval: 1h\n  release-max-backoff: 1h\n";

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("local Home exchange timed out")
}

async fn observed(mut predicate: impl FnMut() -> bool) {
    // Yield rather than advancing the paused clock while waiting for socket delivery.
    for _ in 0..10_000 {
        if predicate() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("Home client did not observe the scripted event");
}

struct Home {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    connector: Connector,
    _identity: tempfile::TempDir,
}

impl Home {
    async fn new() -> Self {
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca_key = KeyPair::generate().unwrap();
        let ca = params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::from_params(&params, &ca_key);
        let server_key = KeyPair::generate().unwrap();
        let server_cert = CertificateParams::new(vec!["127.0.0.1".into()])
            .unwrap()
            .signed_by(&server_key, &issuer)
            .unwrap();
        let client_key = KeyPair::generate().unwrap();
        let client_cert = CertificateParams::new(vec!["mock-cpa".into()])
            .unwrap()
            .signed_by(&client_key, &issuer)
            .unwrap();
        let identity = tempfile::tempdir().unwrap();
        for (name, value) in [
            ("home-ca-crt.pem", ca.pem()),
            ("client-crt.pem", client_cert.pem()),
            ("client-key.pem", client_key.serialize_pem()),
        ] {
            std::fs::write(identity.path().join(name), value).unwrap();
        }
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca.der().clone()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            provider.clone(),
        )
        .build()
        .unwrap();
        let server = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                vec![server_cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(server_key.serialize_der()).into(),
            )
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connector = Connector::from_identity(
            "127.0.0.1".into(),
            listener.local_addr().unwrap().port(),
            identity.path(),
        )
        .await
        .unwrap();
        Self {
            listener,
            acceptor: TlsAcceptor::from(Arc::new(server)),
            connector,
            _identity: identity,
        }
    }

    async fn accept(&self) -> Peer {
        let (socket, _) = self.listener.accept().await.unwrap();
        socket.set_nodelay(true).unwrap();
        let stream = self.acceptor.accept(socket).await.unwrap();
        assert!(stream.get_ref().1.alpn_protocol().is_none());
        assert_eq!(stream.get_ref().1.peer_certificates().unwrap().len(), 1);
        BufReader::new(stream)
    }

    async fn registration(&self, snapshot: &str) -> Peer {
        let mut bootstrap = self.accept().await;
        assert_eq!(
            command(&mut bootstrap).await,
            [b"GET".to_vec(), b"config".to_vec()]
        );
        bulk(&mut bootstrap, CONFIG.as_bytes()).await;
        drop(bootstrap);
        let mut subscription = self.accept().await;
        let args = command(&mut subscription).await;
        assert_eq!(args.len(), 4);
        assert_eq!(
            &args[..3],
            &[b"SUBSCRIBE".to_vec(), b"config".to_vec(), b"7".to_vec()]
        );
        uuid::Uuid::parse_str(std::str::from_utf8(&args[3]).unwrap()).unwrap();
        subscription
            .get_mut()
            .write_all(b"*3\r\n$9\r\nsubscribe\r\n$6\r\nconfig\r\n:1\r\n")
            .await
            .unwrap();
        publish(&mut subscription, "config", snapshot.as_bytes()).await;
        // This must be a new command connection, not bootstrap or subscription reuse.
        let mut worker = self.accept().await;
        assert_eq!(command(&mut worker).await, [b"PING".to_vec()]);
        worker.get_mut().write_all(b"+PONG\r\n").await.unwrap();
        subscription
    }

    async fn start(&self) -> (Arc<HomeClient>, Peer) {
        let snapshot = format!("{CONFIG}request-retry: 2\n");
        let (client, subscription) = bounded(async {
            tokio::join!(
                HomeClient::start(self.connector.clone(), Arc::new(ReportingQueue::default())),
                self.registration(&snapshot)
            )
        })
        .await;
        let client = client.unwrap();
        assert!(client.ready());
        assert!(!client.cancellation().is_cancelled());
        assert_eq!(
            client.config().request_retry,
            2,
            "must install subscription snapshot, not GET"
        );
        (client, subscription)
    }
}

async fn command(peer: &mut Peer) -> Vec<Vec<u8>> {
    let Frame::Array(Some(args)) = resp::read_frame(peer).await.unwrap() else {
        panic!("expected RESP command array");
    };
    args.into_iter()
        .map(|arg| match arg {
            Frame::Bulk(Some(bytes)) => bytes,
            other => panic!("expected bulk argument, got {other:?}"),
        })
        .collect()
}

async fn bulk(peer: &mut Peer, bytes: &[u8]) {
    peer.get_mut()
        .write_all(format!("${}\r\n", bytes.len()).as_bytes())
        .await
        .unwrap();
    peer.get_mut().write_all(bytes).await.unwrap();
    peer.get_mut().write_all(b"\r\n").await.unwrap();
    peer.get_mut().flush().await.unwrap();
}

async fn publish(peer: &mut Peer, channel: &str, bytes: &[u8]) {
    resp::write_command(
        peer.get_mut(),
        &[
            b"message".to_vec(),
            channel.as_bytes().to_vec(),
            bytes.to_vec(),
        ],
    )
    .await
    .unwrap();
}

async fn marker(peer: &mut Peer, client: &HomeClient, retry: u32) {
    publish(
        peer,
        "config",
        format!("{CONFIG}request-retry: {retry}\n").as_bytes(),
    )
    .await;
    observed(|| client.config().request_retry == retry).await;
}

#[tokio::test]
async fn registration_dispatch_rejection_accounted_selection_and_integer_release_ack() {
    let home = Home::new().await;
    let (client, _subscription) = home.start().await;
    let request = serde_json::json!({"model": "public-model", "request_id": "test-request"});
    let rejection =
        serde_json::json!({"error": {"code": "credential_busy", "retry_after_ms": 250}});
    let (result, ()) = bounded(async {
        tokio::join!(client.dispatch(request.clone()), async {
            let mut peer = home.accept().await;
            let args = command(&mut peer).await;
            assert_eq!(args.len(), 2);
            assert_eq!(args[0], b"RPOP");
            assert_eq!(serde_json::from_slice::<Value>(&args[1]).unwrap(), request);
            bulk(&mut peer, &serde_json::to_vec(&rejection).unwrap()).await;
        })
    })
    .await;
    let error = result.err().expect("structured dispatch rejection");
    assert_eq!(error.downcast_ref::<Rejection>().unwrap().0, rejection);
    assert!(client.ready());
    assert!(!client.cancellation().is_cancelled());

    let selection = serde_json::json!({
        "model": "upstream-model", "provider": "openai", "auth_index": "index-1",
        "user_api_key": "test-user", "auth": {"id": "credential-1"},
        "model_info": {"id": "upstream-model"}, "request_retry": 3,
        "concurrency": {"accounted": true, "credential_id": "credential-1", "model": "ledger-model"}
    });
    let (result, ()) = bounded(async {
        tokio::join!(client.dispatch(request.clone()), async {
            let mut peer = home.accept().await;
            let args = command(&mut peer).await;
            assert_eq!(
                args,
                vec![b"RPOP".to_vec(), serde_json::to_vec(&request).unwrap()]
            );
            bulk(&mut peer, &serde_json::to_vec(&selection).unwrap()).await;
        })
    })
    .await;
    let selected = result.unwrap();
    assert_eq!(selected.model, "upstream-model");
    assert_eq!(selected.provider, "openai");
    assert_eq!(selected.auth_index, "index-1");
    assert_eq!(selected.user_api_key, "test-user");
    assert_eq!(selected.auth, selection["auth"]);
    assert_eq!(selected.model_info, Some(selection["model_info"].clone()));
    assert_eq!(selected.request_retry, 3);
    let accounted = selected.concurrency.unwrap();
    assert!(accounted.accounted);
    assert_eq!(accounted.credential_id, "credential-1");
    assert_eq!(accounted.model, "ledger-model");
    let mut execution = client.registry.begin(
        accounted.credential_id,
        accounted.model,
        accounted.accounted,
        "test-request".into(),
        "chat".into(),
    );
    let release = execution.finish().unwrap();
    assert_eq!(client.registry.pending_releases(), vec![release.clone()]);
    let (result, ()) = bounded(async {
        tokio::join!(client.flush_release(&release), async {
            let mut peer = home.accept().await;
            let args = command(&mut peer).await;
            assert_eq!(args.len(), 3);
            assert_eq!(
                &args[..2],
                &[b"LPUSH".to_vec(), b"concurrency-release".to_vec()]
            );
            assert_eq!(
                serde_json::from_slice::<ReleaseFrame>(&args[2]).unwrap(),
                release
            );
            assert!(
                !client.registry.pending_releases().is_empty(),
                "sending is not acknowledgement"
            );
            peer.get_mut().write_all(b":1\r\n").await.unwrap();
        })
    })
    .await;
    result.unwrap();
    assert!(client.registry.pending_releases().is_empty());
    assert!(client.registry.is_drained());
    assert!(client.ready());
    client.fence();
}

#[tokio::test]
async fn subscription_close_fences_membership_and_cancels_active_execution() {
    let home = Home::new().await;
    let (client, mut subscription) = home.start().await;
    let execution = client.registry.begin(
        "credential".into(),
        "model".into(),
        true,
        "request".into(),
        "chat".into(),
    );
    let cancellation = execution.cancellation();
    subscription.get_mut().shutdown().await.unwrap();
    drop(subscription);
    bounded(cancellation.cancelled()).await;
    assert!(!client.ready());
    assert!(client.cancellation().is_cancelled());
    assert_eq!(
        client.registry.active_count(),
        1,
        "cancellation is not completion"
    );
    assert!(client.dispatch(serde_json::json!({})).await.is_err());
    assert!(client.command(vec![b"PING".to_vec()]).await.is_err());
    drop(execution);
    assert_eq!(client.registry.pending_releases().len(), 1);
}

#[tokio::test]
async fn invalid_configuration_updates_fence_without_installing_them() {
    for update in [
        "credential-concurrency: {lifecycle-config-revision: 0}\n".to_owned(),
        CONFIG.replace("revision: 7", "revision: 8"),
        format!("{CONFIG}plugins: {{enabled: true}}\n"),
        "[invalid yaml".to_owned(),
    ] {
        let home = Home::new().await;
        let (client, mut subscription) = home.start().await;
        let old = client.config();
        publish(&mut subscription, "config", update.as_bytes()).await;
        bounded(client.cancellation().cancelled()).await;
        assert!(!client.ready(), "{update}");
        assert!(
            Arc::ptr_eq(&old, &client.config()),
            "invalid snapshot was installed"
        );
    }
}

#[tokio::test]
async fn only_subscription_pong_renews_heartbeat_not_arbitrary_messages() {
    let home = Home::new().await;
    let (client, mut subscription) = home.start().await;
    tokio::time::pause();
    // Ensure subscription has established its initial heartbeat instant.
    marker(&mut subscription, &client, 3).await;
    tokio::time::advance(Duration::from_secs(6)).await;
    resp::write_command(subscription.get_mut(), &[b"pong".to_vec(), Vec::new()])
        .await
        .unwrap();
    marker(&mut subscription, &client, 4).await;
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(
        client.ready(),
        "valid pong must extend health past the original deadline"
    );
    // Neither valid config traffic, unrelated messages, nor malformed pong shapes count.
    publish(&mut subscription, "rebalance", b"{}").await;
    publish(&mut subscription, "other", b"pong").await;
    subscription
        .get_mut()
        .write_all(b"+PONG\r\n")
        .await
        .unwrap();
    resp::write_command(
        subscription.get_mut(),
        &[b"pong".to_vec(), b"not-empty".to_vec()],
    )
    .await
    .unwrap();
    marker(&mut subscription, &client, 5).await;
    tokio::time::advance(Duration::from_secs(6)).await;
    observed(|| client.cancellation().is_cancelled()).await;
    assert!(
        !client.ready(),
        "non-heartbeat traffic must not renew health"
    );
}

// Synthetic keys stay out of assertion diagnostics, including request/usage dumps.
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
