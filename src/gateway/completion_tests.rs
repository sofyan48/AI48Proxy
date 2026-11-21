//! Real mTLS Home dispatch and loopback upstream through the public gateway router.
use super::*;
use crate::home::{
    resp::{self, Frame},
    transport::Connector,
};
use axum::body::Body;
use futures_util::StreamExt;
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;

type Peer = BufReader<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>;
async fn command(peer: &mut Peer) -> Vec<Vec<u8>> {
    let Frame::Array(Some(args)) = resp::read_frame(peer).await.unwrap() else {
        panic!("expected array")
    };
    args.into_iter()
        .map(|a| match a {
            Frame::Bulk(Some(b)) => b,
            _ => panic!("expected bulk"),
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
}
async fn accept(listener: &tokio::net::TcpListener, acceptor: &TlsAcceptor) -> Peer {
    BufReader::new(
        acceptor
            .accept(listener.accept().await.unwrap().0)
            .await
            .unwrap(),
    )
}
async fn fixture() -> (
    Arc<HomeClient>,
    tokio::task::JoinHandle<()>,
    tokio::sync::mpsc::Receiver<()>,
    tokio::sync::oneshot::Sender<u8>,
) {
    fixture_for("openai", None).await
}

async fn fixture_for(
    provider_name: &'static str,
    terminal_gate: Option<tokio::sync::oneshot::Receiver<()>>,
) -> (
    Arc<HomeClient>,
    tokio::task::JoinHandle<()>,
    tokio::sync::mpsc::Receiver<()>,
    tokio::sync::oneshot::Sender<u8>,
) {
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
    let server = CertificateParams::new(vec!["127.0.0.1".into()])
        .unwrap()
        .signed_by(&server_key, &issuer)
        .unwrap();
    let client_key = KeyPair::generate().unwrap();
    let client_cert = CertificateParams::new(vec!["worker".into()])
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
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.der().clone()).unwrap();
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .build()
    .unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![server.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(server_key.serialize_der()).into(),
        )
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let connector = Connector::from_identity(
        "127.0.0.1".into(),
        listener.local_addr().unwrap().port(),
        identity.path(),
    )
    .await
    .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", upstream_listener.local_addr().unwrap());
    let (seen_tx, seen_rx) = tokio::sync::mpsc::channel(1);
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let config = b"credential-concurrency: {lifecycle-config-revision: 7, cpa-heartbeat-timeout: 1h, release-flush-interval: 1h, release-max-backoff: 1h}\nrequest-log: true\nrequests: {streaming: {keepalive-seconds: 1, send-keepalive-before-ft: true}}\n";
        let mut bootstrap = accept(&listener, &acceptor).await;
        assert_eq!(command(&mut bootstrap).await[0], b"GET");
        bulk(&mut bootstrap, config).await;
        drop(bootstrap);
        let mut subscription = accept(&listener, &acceptor).await;
        assert_eq!(command(&mut subscription).await[0], b"SUBSCRIBE");
        subscription
            .get_mut()
            .write_all(b"*3\r\n$9\r\nsubscribe\r\n$6\r\nconfig\r\n:1\r\n")
            .await
            .unwrap();
        resp::write_command(
            subscription.get_mut(),
            &[b"message".to_vec(), b"config".to_vec(), config.to_vec()],
        )
        .await
        .unwrap();
        let mut ping = accept(&listener, &acceptor).await;
        assert_eq!(command(&mut ping).await[0], b"PING");
        ping.get_mut().write_all(b"+PONG\r\n").await.unwrap();
        let mut dispatch = accept(&listener, &acceptor).await;
        assert_eq!(command(&mut dispatch).await[0], b"RPOP");
        bulk(&mut dispatch, json!({"model":"resolved", "provider":provider_name, "auth_index":"index", "auth":{"id":"credential","attributes":{"api_key":"PRIVATE_TOKEN","base_url":base}},"concurrency":{"accounted":true,"credential_id":"credential","model":"resolved"}}).to_string().as_bytes()).await;
        let (mut socket, _) = upstream_listener.accept().await.unwrap();
        use tokio::io::AsyncReadExt;
        let mut buf = [0; 8192];
        assert!(socket.read(&mut buf).await.unwrap() > 0);
        seen_tx.send(()).await.unwrap();
        let Ok(outcome) = reply_rx.await else {
            // Dropping the downstream while waiting must close the upstream socket.
            loop {
                if socket.read(&mut buf).await.unwrap() == 0 {
                    break;
                }
            }
            seen_tx.send(()).await.unwrap();
            std::future::pending::<()>().await;
            return;
        };
        if let Some(gate) = terminal_gate {
            let start = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fixture\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n";
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
            socket
                .write_all(format!("{:x}\r\n{start}\r\n", start.len()).as_bytes())
                .await
                .unwrap();
            if gate.await.is_ok() {
                let delta = "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n";
                let stop = "data: {\"type\":\"message_stop\"}\n\n";
                let tail = match outcome {
                    1 => format!("{delta}{stop}"),
                    2 => delta.to_owned(), // Valid finish reason, missing message_stop.
                    3 => format!("{delta}{stop}data: {{"), // Partial event after terminal.
                    4 => stop.to_owned(),  // Missing finish reason.
                    5 => format!(
                        "{delta}data: {{\"type\":\"ping\",\"error\":{{\"message\":\"failed\"}}}}\n\n{stop}"
                    ),
                    _ => "data: invalid-json\n\n".to_owned(),
                };
                socket
                    .write_all(format!("{:x}\r\n{tail}\r\n", tail.len()).as_bytes())
                    .await
                    .unwrap();
                if outcome == 2 {
                    socket.write_all(b"0\r\n\r\n").await.unwrap();
                }
            }
            // Never send EOF on success: completion must close this connection.
            loop {
                if socket.read(&mut buf).await.unwrap() == 0 {
                    break;
                }
            }
            seen_tx.send(()).await.unwrap();
            std::future::pending::<()>().await;
            return;
        }
        let (status, content_type, body) = if outcome > 0 {
            (
                "200 OK",
                "text/event-stream",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            )
        } else {
            (
                "502 Bad Gateway",
                "application/json",
                "PRIVATE_UPSTREAM_BODY",
            )
        };
        let length = body.len() + if outcome == 2 { 100 } else { 0 };
        socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{body}").as_bytes()).await.unwrap();
        drop(socket);
        std::future::pending::<()>().await;
        drop(subscription);
        drop(identity);
    });
    let client = HomeClient::start(
        connector,
        Arc::new(crate::reporting::ReportingQueue::default()),
    )
    .await
    .unwrap();
    (client, task, seen_rx, reply_tx)
}

#[path = "claude_sse_completion_tests.rs"]
mod claude_sse_completion_tests;

#[tokio::test]
async fn gateway_log_completes_once_for_success_failure_and_pre_header_disconnect() {
    tokio::time::timeout(Duration::from_secs(15), async {
        for outcome in [Some(1), Some(0), Some(2), None] {
            let (client, task, mut seen, reply) = fixture().await;
            let mut logs = crate::observability::capture_requests(&client);
            let request = http::Request::builder().method("POST").uri("/v1/chat/completions")
                .header("content-type", "application/json").header("authorization", "Bearer PRIVATE_CALLER")
                .body(Body::from(r#"{"model":"alias","stream":true,"messages":[]}"#)).unwrap();
            let response = router(client.clone()).oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let mut output = response.into_body().into_data_stream();
            assert_eq!(output.next().await.unwrap().unwrap(), crate::config::keepalive::SSE_COMMENT);
            // Polling resumes execution after the first comment.
            let mut next = Box::pin(output.next());
            tokio::select! { _ = seen.recv() => {}, _ = &mut next => panic!("unexpected token before upstream headers") }
            assert!(logs.try_recv().is_err());
            assert_eq!(client.registry.active_count(), 1);
            if let Some(success) = outcome {
                reply.send(success).unwrap();
                let first = next.await.unwrap().unwrap();
                assert!(logs.try_recv().is_err());
                if success > 0 {
                    for _ in 0..20 { tokio::task::yield_now().await; }
                    assert_eq!(client.registry.active_count(), 1, "producer EOF cannot release before downstream completion");
                    assert!(client.reporting.is_empty());
                }
                let mut bytes = first.to_vec();
                while let Some(chunk) = output.next().await { bytes.extend(chunk.unwrap()); }
                if success != 1 { assert!(std::str::from_utf8(&bytes).unwrap().contains("\"error\"")); }
                assert!(!std::str::from_utf8(&bytes).unwrap().contains("PRIVATE"));
            } else {
                drop(next);
                drop(output);
                drop(reply);
                seen.recv().await.unwrap();
            }
            for _ in 0..100 { if client.registry.active_count() == 0 { break; } tokio::task::yield_now().await; }
            assert_eq!(client.registry.active_count(), 0);
            assert_eq!(client.reporting.len(), 1);
            assert_eq!(client.registry.pending_releases().len(), 1);
            let log = logs.recv().await.unwrap();
            let text = &log.request_log;
            assert!(text.contains("Provider: openai"), "{text}");
            let status = match outcome { Some(1) => 200, Some(_) => 502, None => 499 };
            assert!(text.contains(&format!("Status: {status}")), "{text}");
            assert!(text.contains(&format!("Error: {}", status >= 400)));
            assert!(!text.contains("PRIVATE"));
            assert!(logs.try_recv().is_err());
            crate::observability::stop_capture_requests(&client);
            client.fence();
            task.abort();
            let _ = task.await;
        }
    }).await.expect("gateway lifecycle fixture timed out");
}
