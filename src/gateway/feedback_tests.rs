//! Wired as a child of routes so observations exercise its private response boundary.
use super::{lifecycle, observe_result};
use crate::{
    home::{
        client::HomeClient,
        resp::{self, Frame},
        transport::Connector,
    },
    reporting::ReportingQueue,
};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use serde_json::{Value, json};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};

type Peer = BufReader<TlsStream<TcpStream>>;
const CONFIG: &str = "credential-concurrency:\n  lifecycle-config-revision: 7\n  cpa-heartbeat-timeout: 1h\n  release-flush-interval: 1h\n  release-max-backoff: 1h\n";
const SECRET: &str = "PRIVATE_UPSTREAM_TOKEN";

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("loopback feedback fixture timed out")
}

// Client tests keep their fixture private; use the same real certificate/RESP handshake here.
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
                CONFIG.as_bytes().to_vec(),
            ],
        )
        .await
        .unwrap();
        let mut worker = self.accept().await;
        assert_eq!(command(&mut worker).await, [b"PING".to_vec()]);
        worker.get_mut().write_all(b"+PONG\r\n").await.unwrap();
        subscription
    }

    async fn start(&self) -> (Arc<HomeClient>, Peer) {
        let (client, subscription) = bounded(async {
            tokio::try_join!(
                HomeClient::start(self.connector.clone(), Arc::new(ReportingQueue::default())),
                async { Ok::<_, anyhow::Error>(self.registration().await) }
            )
        })
        .await
        .expect("mock Home registration failed");
        assert!(client.ready());
        (client, subscription)
    }

    async fn attempt(&self, client: Arc<HomeClient>, provider: &str) -> lifecycle::Attempt {
        let dispatch = lifecycle::dispatch(
            client.clone(),
            json!({"type":"auth", "model":"public-alias"}),
            uuid::Uuid::new_v4().to_string(),
            false,
        );
        let reply = async {
            let mut peer = self.accept().await;
            let args = command(&mut peer).await;
            assert_eq!(args[0], b"RPOP");
            let request: Value = serde_json::from_slice(&args[1]).unwrap();
            assert_eq!(request["model"], "public-alias");
            bulk(&mut peer, &serde_json::to_vec(&json!({
                "model":"resolved-model", "provider":provider, "auth_index":"index",
                "user_api_key":"caller-principal", "auth":{"id":"credential", "attributes":{"api_key":SECRET}},
                "concurrency":{"accounted":true,"credential_id":"credential","model":"ledger-model"}
            })).unwrap()).await;
        };
        let (result, ()) = bounded(async { tokio::join!(dispatch, reply) }).await;
        let (_, attempt) = result.unwrap();
        assert_eq!(client.registry.active_count(), 1);
        assert_eq!(client.registry.pending_admissions(), 0);
        attempt
    }
}

async fn command(peer: &mut Peer) -> Vec<Vec<u8>> {
    let Frame::Array(Some(args)) = resp::read_frame(peer).await.unwrap() else {
        panic!("expected RESP command")
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

async fn upstream(status: u16, body: &str) -> reqwest::Response {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let serve = async {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(socket.read_u8().await.unwrap());
            assert!(headers.len() < 8192);
        }
        socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    };
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let (response, ()) = bounded(async { tokio::join!(client.get(url).send(), serve) }).await;
    response.unwrap()
}

fn finish(client: &HomeClient, mut attempt: lifecycle::Attempt) -> Value {
    assert!(
        client.reporting.is_empty(),
        "headers must not enqueue completion"
    );
    assert!(client.registry.pending_releases().is_empty());
    let frame = attempt.finish().expect("accounted execution must release");
    assert_eq!(frame.credential_id, "credential");
    assert_eq!(frame.model, "ledger-model");
    assert_eq!(frame.release_seq, 1);
    assert!(attempt.finish().is_none(), "completion must be idempotent");
    drop(attempt);
    assert_eq!(client.registry.active_count(), 0);
    assert_eq!(client.registry.pending_releases(), vec![frame]);
    assert_eq!(client.reporting.len(), 1, "Drop must enqueue exactly once");
    let batch = client.reporting.pending_batch(2, 256 * 1024);
    let payload: Value = serde_json::from_slice(&batch[0].payload).unwrap();
    assert_eq!(payload["api_key"], "caller-principal");
    assert_eq!(payload["alias"], "public-alias");
    assert_eq!(payload["model"], "resolved-model");
    assert!(!payload.to_string().contains(SECRET));
    payload
}

#[tokio::test]
async fn feedback_success_headers_remain_request_scoped_until_body_completion() {
    let home = Home::new().await;
    let (client, _subscription) = home.start().await;
    let mut attempt = home.attempt(client.clone(), "openai").await;
    let mut result = Ok(upstream(200, r#"{"choices":[]}"#).await);
    bounded(observe_result(&mut result, &mut attempt)).await;
    assert!(attempt.record.failed);
    assert_eq!(attempt.record.status_code, 200);
    assert_eq!(attempt.record.fail_code, "request_scoped");
    assert!(attempt.record.fail_body.is_empty());
    assert_eq!(client.registry.active_count(), 1);
    // Dropping an unconsumed successful response is not proof of completion.
    drop(result);
    let payload = finish(&client, attempt);
    assert_eq!(payload["failed"], true);
    assert_eq!(payload["fail"]["status_code"], 200);
    assert_eq!(payload["fail"]["code"], "request_scoped");
    client.fence();
}

#[tokio::test]
async fn feedback_429_consumes_body_and_reports_only_allowlisted_quota_hints() {
    let home = Home::new().await;
    let (client, _subscription) = home.start().await;
    let mut attempt = home.attempt(client.clone(), "codex").await;
    let body = json!({"error":{"type":"usage_limit_reached", "resets_at":2000000000, "resets_in_seconds":60, "message":SECRET, "token":SECRET}, "raw_payload":SECRET});
    let mut result = Ok(upstream(429, &body.to_string()).await);
    bounded(observe_result(&mut result, &mut attempt)).await;
    assert!(
        bounded(result.as_mut().unwrap().chunk())
            .await
            .unwrap()
            .is_none(),
        "error body must be consumed before retry discards it"
    );
    assert!(attempt.record.failed);
    assert_eq!(attempt.record.status_code, 429);
    assert!(
        attempt.record.fail_code.is_empty(),
        "genuine upstream HTTP failure must not be request-scoped"
    );
    let expected = json!({"error":{"type":"usage_limit_reached", "resets_at":2000000000, "resets_in_seconds":60}});
    assert_eq!(
        serde_json::from_str::<Value>(&attempt.record.fail_body).unwrap(),
        expected
    );
    let payload = finish(&client, attempt);
    assert_eq!(payload["failed"], true);
    assert_eq!(payload["fail"]["status_code"], 429);
    assert_eq!(payload["fail"]["code"], "");
    assert_eq!(
        serde_json::from_str::<Value>(payload["fail"]["body"].as_str().unwrap()).unwrap(),
        expected
    );
    client.fence();
}

#[tokio::test]
async fn feedback_local_connect_failure_is_request_scoped() {
    let home = Home::new().await;
    let (client, _subscription) = home.start().await;
    let mut attempt = home.attempt(client.clone(), "codex").await;
    // Close a listening socket before connecting: on macOS a bound socket that
    // never listened can silently drop SYNs instead of returning connection refused.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/?token={SECRET}", listener.local_addr().unwrap());
    drop(listener);
    let mut result = bounded(
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(url)
            .send(),
    )
    .await;
    assert!(result.as_ref().unwrap_err().is_connect());
    bounded(observe_result(&mut result, &mut attempt)).await;
    assert!(attempt.record.failed);
    assert_eq!(attempt.record.status_code, 502);
    assert_eq!(attempt.record.fail_code, "request_scoped");
    assert!(attempt.record.fail_body.is_empty());
    let payload = finish(&client, attempt);
    assert_eq!(payload["failed"], true);
    assert_eq!(
        payload["fail"],
        json!({"status_code":502,"code":"request_scoped","body":""})
    );
    client.fence();
}
