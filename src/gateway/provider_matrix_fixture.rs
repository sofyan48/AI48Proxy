//! Private loopback-only Home mTLS/RESP and HTTP fixtures for the provider matrix.
use crate::{
    home::{
        client::HomeClient,
        resp::{self, Frame},
        transport::Connector,
    },
    reporting::ReportingQueue,
};
use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use serde_json::{Value, json};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};
use tower::ServiceExt;

#[path = "codex_bootstrap_failover_tests.rs"]
mod codex_bootstrap_failover_tests;

type Peer = BufReader<TlsStream<TcpStream>>;
const CONFIG: &str = "request-retry: 2\ncredential-concurrency:\n  lifecycle-config-revision: 7\n  cpa-heartbeat-timeout: 1h\n  release-flush-interval: 1h\n  release-max-backoff: 1h\n";
pub(crate) const SECRET: &str = "matrix-upstream-key-not-a-real-credential";
pub(crate) const CALLER: &str = "matrix-downstream-key-not-a-real-credential";

pub(crate) async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("provider matrix loopback exchange timed out")
}

// Copied minimal handshake from home/client_tests; no shared fixture visibility changes.
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
        assert_eq!(stream.get_ref().1.peer_certificates().unwrap().len(), 1);
        assert!(stream.get_ref().1.alpn_protocol().is_none());
        BufReader::new(stream)
    }

    async fn registration(&self) -> Peer {
        self.registration_with(CONFIG).await
    }

    async fn registration_with(&self, config: &str) -> Peer {
        let mut bootstrap = self.accept().await;
        assert_eq!(
            command(&mut bootstrap).await,
            [b"GET".to_vec(), b"config".to_vec()]
        );
        bulk(&mut bootstrap, config.as_bytes()).await;
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
        resp::write_command(
            subscription.get_mut(),
            &[
                b"message".to_vec(),
                b"config".to_vec(),
                config.as_bytes().to_vec(),
            ],
        )
        .await
        .unwrap();
        let mut worker = self.accept().await;
        assert_eq!(command(&mut worker).await, [b"PING".to_vec()]);
        worker.get_mut().write_all(b"+PONG\r\n").await.unwrap();
        subscription
    }

    async fn select(&self, provider: &str, base: &str) -> Value {
        let mut peer = self.accept().await;
        let args = command(&mut peer).await;
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], b"RPOP");
        let dispatch: Value = serde_json::from_slice(&args[1]).unwrap();
        assert_eq!(dispatch["type"], "auth");
        assert_eq!(dispatch["model"], "public-alias");
        assert_eq!(dispatch["retry_round"], 0);
        assert_eq!(dispatch["excluded_auth_ids"], json!([]));
        assert_eq!(dispatch["concurrency_protocol"], 1);
        assert_eq!(
            dispatch["headers"]["authorization"],
            json!([format!("Bearer {CALLER}")])
        );
        bulk(&mut peer, &serde_json::to_vec(&json!({
            "provider":provider, "model":"resolved-model", "auth_index":"matrix-index",
            "user_api_key":"home-authenticated-principal", "request_retry":2,
            "auth":{"id":"matrix-credential", "attributes":{"api_key":SECRET,"base_url":base}},
            "concurrency":{"accounted":true,"credential_id":"matrix-credential","model":"ledger-model"}
        })).unwrap()).await;
        dispatch
    }
}

async fn command(peer: &mut Peer) -> Vec<Vec<u8>> {
    let Frame::Array(Some(args)) = resp::read_frame(peer).await.unwrap() else {
        panic!("expected RESP command array")
    };
    args.into_iter()
        .map(|arg| match arg {
            Frame::Bulk(Some(bytes)) => bytes,
            _ => panic!("expected bulk argument"),
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

pub(crate) struct Captured {
    pub target: String,
    pub headers: HeaderMap,
    pub body: Value,
}

async fn upstream(listener: &TcpListener, media: &str, wire: &str) -> Captured {
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(socket.read_u8().await.unwrap());
        assert!(bytes.len() < 32 * 1024);
    }
    let text = String::from_utf8(bytes).unwrap();
    let mut lines = text.split("\r\n");
    let start = lines.next().unwrap().split_whitespace().collect::<Vec<_>>();
    assert_eq!(start[0], "POST");
    let target = start[1].to_owned();
    let mut headers = HeaderMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').unwrap();
        headers.append(
            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.trim().parse().unwrap(),
        );
    }
    let length = headers["content-length"]
        .to_str()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert!(length < 128 * 1024);
    let mut bytes = vec![0; length];
    socket.read_exact(&mut bytes).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap();
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {media}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{wire}", wire.len()).as_bytes()).await.unwrap();
    Captured {
        target,
        headers,
        body,
    }
}

pub(crate) struct Observed {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
    pub upstream: Captured,
    pub usage: Value,
}

/// Executes the actual gateway router, consumes its body, and verifies one ACKed release.
/// All URLs originate from this fixture's ephemeral IPv4 loopback listeners.
pub(crate) async fn run(
    provider: &str,
    path: &str,
    body: Value,
    media: &str,
    wire: &str,
) -> Observed {
    bounded(async {
        let home = Home::new().await;
        let (client, _subscription) = tokio::join!(
            HomeClient::start(home.connector.clone(), Arc::new(ReportingQueue::default())),
            home.registration(),
        );
        let client = client.unwrap();
        assert!(client.ready());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/tenant/v1", listener.local_addr().unwrap());
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {CALLER}"))
            .header("origin", "http://localhost:3000")
            .header("x-upstream-url", "http://127.0.0.1:1/untrusted")
            .body(Body::from(body.to_string()))
            .unwrap();
        let (response, _, captured) = tokio::join!(
            super::super::router(client.clone()).oneshot(request),
            home.select(provider, &base),
            upstream(&listener, media, wire),
        );
        let response = response.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        assert_eq!(headers["access-control-allow-origin"], "*");
        assert!(!headers.contains_key("access-control-allow-credentials"));
        let body = String::from_utf8(
            to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        // Streaming completion owns a spawned producer; wait for its synchronous Drop reporting.
        while client.reporting.is_empty() {
            tokio::task::yield_now().await;
        }
        assert_eq!(client.reporting.len(), 1);
        assert_eq!(client.registry.active_count(), 0);
        assert_eq!(client.registry.pending_admissions(), 0);
        let releases = client.registry.pending_releases();
        assert_eq!(releases.len(), 1);
        let release = &releases[0];
        assert_eq!(release.credential_id, "matrix-credential");
        assert_eq!(release.model, "ledger-model");
        assert_eq!(release.release_seq, 1);
        let (ack, ()) = tokio::join!(client.flush_release(release), async {
            let mut peer = home.accept().await;
            let args = command(&mut peer).await;
            assert_eq!(args.len(), 3);
            assert_eq!(
                &args[..2],
                &[b"LPUSH".to_vec(), b"concurrency-release".to_vec()]
            );
            assert_eq!(
                serde_json::from_slice::<Value>(&args[2]).unwrap(),
                serde_json::to_value(release).unwrap()
            );
            peer.get_mut().write_all(b":1\r\n").await.unwrap();
        });
        ack.unwrap();
        assert!(client.registry.pending_releases().is_empty());
        let batch = client.reporting.pending_batch(2, 256 * 1024);
        let usage: Value = serde_json::from_slice(&batch[0].payload).unwrap();
        assert_eq!(usage["provider"], provider);
        assert_eq!(usage["model"], "resolved-model");
        assert_eq!(usage["alias"], "public-alias");
        assert_eq!(usage["api_key"], "home-authenticated-principal");
        assert_eq!(usage["auth_index"], "matrix-index");
        assert!(!usage.to_string().contains(SECRET));
        assert!(!usage.to_string().contains(CALLER));
        assert!(!body.contains(SECRET));
        assert!(!captured.headers.contains_key("x-upstream-url"));
        assert!(!captured.headers.contains_key("origin"));
        client.fence();
        client.retire().await.unwrap();
        Observed {
            status,
            headers,
            body,
            upstream: captured,
            usage,
        }
    })
    .await
}
