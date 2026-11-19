use super::*;
use crate::{
    execution::Registry,
    home::resp::{self, Frame},
};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use tokio::io::{AsyncWriteExt, BufReader};

#[test]
fn jitter_is_deterministic_bounded_and_resets_ceiling() {
    let mut a = Backoff::new(42);
    let mut b = Backoff::new(42);
    for n in 0..100 {
        let delay = a.next_delay();
        assert_eq!(delay, b.next_delay());
        assert!(delay > Duration::ZERO);
        assert!(delay <= Duration::from_millis((250u64 << n.min(7)).min(30_000)));
    }
    a.reset();
    assert!(a.next_delay() <= Duration::from_millis(250));
}

#[test]
fn late_release_cannot_acknowledge_fresh_registry() {
    let old = Registry::new(CancellationToken::new());
    let new = Registry::new(CancellationToken::new());
    let mut guard = old.begin(
        "credential".into(),
        "model".into(),
        true,
        "request".into(),
        "request".into(),
    );
    let release = guard.finish().unwrap();
    new.acknowledge(&release);
    assert!(new.is_drained());
    assert!(!old.is_drained());
    old.acknowledge(&release);
    assert!(old.is_drained());
}

#[tokio::test]
async fn supervisor_recovers_normal_registration_with_stable_instance_and_new_registry() {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca_key = KeyPair::generate().unwrap();
    let ca = params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::from_params(&params, &ca_key);
    let key = KeyPair::generate().unwrap();
    let cert = CertificateParams::new(vec!["127.0.0.1".into()])
        .unwrap()
        .signed_by(&key, &issuer)
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    for (name, contents) in [
        ("home-ca-crt.pem", ca.pem()),
        ("client-crt.pem", cert.pem()),
        ("client-key.pem", key.serialize_pem()),
    ] {
        std::fs::write(dir.path().join(name), contents).unwrap();
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
            vec![cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        )
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let connector = Connector::from_identity(
        "127.0.0.1".into(),
        listener.local_addr().unwrap().port(),
        dir.path(),
    )
    .await
    .unwrap();
    let supervisor = Supervisor::new(connector, Arc::new(ReportingQueue::default()));
    let script = tokio::spawn(async move {
        let mut instances = Vec::new();
        for _ in 0..2 {
            let config = b"port: 8317\ncredential-concurrency:\n  lifecycle-config-revision: 7\n  cpa-heartbeat-timeout: 10s\n  release-flush-interval: 1h\n  release-max-backoff: 1h\n";
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(acceptor.accept(socket).await.unwrap());
            assert_eq!(
                resp::read_frame(&mut stream).await.unwrap(),
                Frame::Array(Some(vec![
                    Frame::Bulk(Some(b"GET".to_vec())),
                    Frame::Bulk(Some(b"config".to_vec()))
                ]))
            );
            stream
                .get_mut()
                .write_all(format!("${}\r\n", config.len()).as_bytes())
                .await
                .unwrap();
            stream.get_mut().write_all(config).await.unwrap();
            stream.get_mut().write_all(b"\r\n").await.unwrap();
            drop(stream);
            let (socket, _) = listener.accept().await.unwrap();
            let mut sub = BufReader::new(acceptor.accept(socket).await.unwrap());
            let Frame::Array(Some(args)) = resp::read_frame(&mut sub).await.unwrap() else {
                panic!("subscribe");
            };
            assert_eq!(args.len(), 4, "normal registration only; never takeover");
            assert_eq!(args[0], Frame::Bulk(Some(b"SUBSCRIBE".to_vec())));
            instances.push(args[3].clone());
            sub.get_mut().write_all(b"*3\r\n$9\r\nsubscribe\r\n$6\r\nconfig\r\n:1\r\n*3\r\n$7\r\nmessage\r\n$6\r\nconfig\r\n").await.unwrap();
            sub.get_mut()
                .write_all(format!("${}\r\n", config.len()).as_bytes())
                .await
                .unwrap();
            sub.get_mut().write_all(config).await.unwrap();
            sub.get_mut().write_all(b"\r\n").await.unwrap();
            let (socket, _) = listener.accept().await.unwrap();
            let mut worker = BufReader::new(acceptor.accept(socket).await.unwrap());
            let _ = resp::read_frame(&mut worker).await.unwrap();
            worker.get_mut().write_all(b"+PONG\r\n").await.unwrap();
            if instances.len() == 2 {
                assert_eq!(instances[0], instances[1]);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            // Closing the first subscription triggers automatic recovery.
            drop(sub);
        }
    });
    let first = supervisor.connect().await.unwrap();
    let run = tokio::spawn(supervisor.clone().run());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(current) = supervisor.current()
                && !Arc::ptr_eq(&first, &current)
            {
                assert!(!Arc::ptr_eq(&first.registry, &current.registry));
                assert!(first.retired.is_cancelled());
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    supervisor.stop.cancel();
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    script.await.unwrap();
}
