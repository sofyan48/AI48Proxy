use super::*;
#[path = "gateway_client_tests.rs"]
mod gateway_client_tests;
#[path = "membership_tests.rs"]
mod membership_tests;
#[path = "rebalance_tests.rs"]
mod rebalance_tests;
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
    // Bound by wall-clock time, not yield count: real socket I/O can lag on loaded CI runners.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
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
        client.membership.managed.store(true, Ordering::Release);
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
