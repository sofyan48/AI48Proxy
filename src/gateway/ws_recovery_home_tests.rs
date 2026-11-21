//! Real downstream + upstream sockets and Home mTLS/RESP recovery regression.
use super::super::*;
use crate::{
    home::{
        resp::{self, Frame},
        transport::Connector,
    },
    reporting::ReportingQueue,
};
use ::tokio_tungstenite::{accept_async, connect_async};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};

type Peer = BufReader<TlsStream<TcpStream>>;
const CONFIG: &str = "credential-concurrency:\n  lifecycle-config-revision: 7\n  cpa-heartbeat-timeout: 1h\n  release-flush-interval: 1h\n  release-max-backoff: 1h\n";
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
        let tls = rustls::ServerConfig::builder_with_provider(provider)
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
            acceptor: TlsAcceptor::from(Arc::new(tls)),
            connector,
            _identity: identity,
        }
    }
    async fn accept(&self) -> Peer {
        let (tcp, _) = self.listener.accept().await.unwrap();
        BufReader::new(self.acceptor.accept(tcp).await.unwrap())
    }
    async fn registration(&self) -> Peer {
        let mut bootstrap = self.accept().await;

        command(&mut bootstrap).await;
        bulk(&mut bootstrap, CONFIG.as_bytes()).await;
        drop(bootstrap);

        let mut subscription = self.accept().await;

        command(&mut subscription).await;
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
    async fn reply(&self, kind: &str, value: Value) {
        let mut peer = self.accept().await;
        let args = command(&mut peer).await;
        let request: Value = serde_json::from_slice(&args[1]).unwrap();
        assert_eq!(request["type"], kind);
        bulk(&mut peer, &serde_json::to_vec(&value).unwrap()).await;
    }
    async fn authorize(&self) {
        self.reply(
            "auth-validate",
            json!({"authenticated":true,"principal":"user"}),
        )
        .await;
        self.reply("models", json!({"codex":[{"id":"m"}]})).await;
    }
}
async fn command(peer: &mut Peer) -> Vec<Vec<u8>> {
    let Frame::Array(Some(args)) = resp::read_frame(peer).await.unwrap() else {
        panic!("RESP array expected")
    };
    args.into_iter()
        .map(|arg| match arg {
            Frame::Bulk(Some(bytes)) => bytes,
            _ => panic!("bulk expected"),
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

#[tokio::test]
async fn ws_recovery_keeps_downstream_pinned_and_reauthorizes_without_lease_transfer() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mock = Home::new().await;
        let queue = Arc::new(ReportingQueue::default());
        let (client, _subscription) = tokio::try_join!(HomeClient::start(mock.connector.clone(), queue.clone()), async { Ok::<_, anyhow::Error>(mock.registration().await) }).unwrap();
        let home = client;

        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", upstream.local_addr().unwrap());
        let downstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = downstream.local_addr().unwrap();
        let session_home = home.clone();
        let app = axum::Router::new().route("/responses", axum::routing::get(move |ws: WebSocketUpgrade| async move {
            ws.on_upgrade(false, 100000, move |socket| async move {
                run(session_home, HeaderMap::new(), "user".into(), socket, 100000).await;
            })
        }));
        let server = tokio::spawn(async move { axum::serve(downstream, app).await.unwrap(); });
        let exchange = async {
            mock.authorize().await;
            mock.reply("auth", json!({"provider":"codex","model":"m","auth_index":"idx","user_api_key":"user",
                "auth":{"id":"credential","attributes":{"api_key":"test-upstream","base_url":base,"websockets":true}},
                "concurrency":{"accounted":true,"credential_id":"credential","model":"m"}})).await;
            let (tcp, _) = upstream.accept().await.unwrap();
            let mut first = accept_async(tcp).await.unwrap();
            let sent: Value = serde_json::from_str(first.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(sent["event_id"], "c1");
            first.send(UpMessage::Text(json!({"type":"response.created","response":{"id":"r1"}}).to_string().into())).await.unwrap();
            drop(first);
            // Only the new explicit input is authorized. A stale continuation
            // is answered locally and must not dispatch or acquire another lease.
            mock.authorize().await;
            let (tcp, _) = upstream.accept().await.unwrap();
            let mut second = accept_async(tcp).await.unwrap();
            let sent: Value = serde_json::from_str(second.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(sent["event_id"], "c2");
            assert!(sent.get("previous_response_id").is_none());
            for event in [json!({"type":"response.created","response":{"id":"r2"}}),
                json!({"type":"response.completed","response":{"id":"r2","output":[]}})] {
                second.send(UpMessage::Text(event.to_string().into())).await.unwrap();
            }
            second.next().await; // downstream close releases the epoch
        };
        let browser = async {
            let (mut socket, _) = connect_async(format!("ws://{address}/responses")).await.unwrap();
            socket.send(UpMessage::Text(json!({"type":"response.create","model":"m","input":[],"event_id":"c1"}).to_string().into())).await.unwrap();
            let created: Value = serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(created["response"]["id"], "r1");
            let lost: Value = serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(lost["error"]["event_id"], "c1");
            assert_eq!(lost["error"]["code"], "upstream_websocket_lost");
            assert_eq!(home.registry.active_count(), 1);
            assert_eq!(queue.len(), 1, "abandoned response reported once before recovery");
            assert!(home.registry.pending_releases().is_empty());
            socket.send(UpMessage::Text(json!({"type":"response.create","previous_response_id":"r1","input":[]}).to_string().into())).await.unwrap();
            let invalid: Value = serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(invalid["error"]["code"], "previous_response_not_found");
            socket.send(UpMessage::Text(json!({"type":"response.create","input":[],"event_id":"c2"}).to_string().into())).await.unwrap();
            for kind in ["response.created", "response.completed"] {
                let event: Value = serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(event["type"], kind); // no duplicate loss or session-ended frame
            }
            assert_eq!(home.registry.active_count(), 1);
            socket.close(None).await.unwrap();
        };
        tokio::join!(exchange, browser);
        for _ in 0..100 {
            if home.registry.active_count() == 0 { break; }
            tokio::task::yield_now().await;
        }
        assert_eq!(home.registry.active_count(), 0);
        assert_eq!(home.registry.pending_releases().len(), 1);
        assert_eq!(queue.len(), 2, "one abandoned and one completed response report");
        server.abort();
    }).await.expect("recovery session fixture timed out");
}
