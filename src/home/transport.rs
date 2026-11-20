//! Home's custom RESP transport: mTLS only, no Redis initialization or HTTP ALPN.
use super::resp::{self, Frame};
use anyhow::{Context, Result, ensure};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use std::{io::BufReader as StdBufReader, path::Path, sync::Arc, time::Duration};
use tokio::{io::BufReader, net::TcpStream};
use tokio_rustls::{TlsConnector, client::TlsStream};

#[derive(Clone)]
pub struct Connector {
    host: String,
    port: u16,
    tls: Arc<ClientConfig>,
}

pub struct Connection {
    stream: BufReader<TlsStream<TcpStream>>,
    // Set before each await: errors and cancellation permanently fence this stream.
    poisoned: bool,
}

pub(crate) fn validate_target(host: &str, port: u16) -> Result<()> {
    ensure!(
        port != 0 && !host.is_empty() && host == host.trim(),
        "invalid Home target"
    );
    ensure!(
        !host.chars().any(|c| c.is_whitespace() || c.is_control())
            && !host.contains(['/', '@', '[', ']']),
        "invalid Home host"
    );
    ServerName::try_from(host.to_owned()).context("invalid Home TLS server name")?;
    Ok(())
}

pub(crate) fn certificates(pem: &[u8]) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let certs =
        rustls_pemfile::certs(&mut StdBufReader::new(pem)).collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        !certs.is_empty(),
        "certificate PEM contains no certificates"
    );
    Ok(certs)
}

pub(crate) fn identity_config(ca: &[u8], cert: &[u8], key: &[u8]) -> Result<ClientConfig> {
    let mut roots = RootCertStore::empty();
    for certificate in certificates(ca)? {
        roots
            .add(certificate)
            .context("invalid Home CA certificate")?;
    }
    let key = rustls_pemfile::private_key(&mut StdBufReader::new(key))?
        .context("client PEM contains no private key")?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_client_auth_cert(certificates(cert)?, key)
        .context("invalid or mismatched Home client identity")?;
    config.alpn_protocols.clear();
    Ok(config)
}

impl Connector {
    pub async fn from_identity(host: String, port: u16, identity_dir: &Path) -> Result<Self> {
        validate_target(&host, port)?;
        let ca = super::bootstrap::read_identity_file(&identity_dir.join("home-ca-crt.pem"))?;
        let cert = super::bootstrap::read_identity_file(&identity_dir.join("client-crt.pem"))?;
        let key = super::bootstrap::read_identity_file(&identity_dir.join("client-key.pem"))?;
        Ok(Self {
            host,
            port,
            tls: Arc::new(identity_config(&ca, &cert, &key)?),
        })
    }

    pub(super) fn node(&self) -> super::cluster::Target {
        super::cluster::Target {
            ip: self.host.clone(),
            port: self.port,
        }
    }
    pub fn target(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub async fn for_target(&self, host: String, port: u16) -> Result<Self> {
        validate_target(&host, port)?;
        Ok(Self {
            host,
            port,
            tls: self.tls.clone(),
        })
    }

    pub async fn connect(&self) -> Result<Connection> {
        // Bound acquisition only; subscriptions and established operations have no deadlines.
        tokio::time::timeout(Duration::from_secs(30), async {
            let socket = TcpStream::connect((self.host.as_str(), self.port))
                .await
                .context("connect to Home")?;
            socket.set_nodelay(true)?;
            let name = ServerName::try_from(self.host.clone())?;
            let stream = TlsConnector::from(self.tls.clone())
                .connect(name, socket)
                .await
                .context("Home mTLS handshake")?;
            Ok(Connection {
                stream: BufReader::new(stream),
                poisoned: false,
            })
        })
        .await
        .context("Home connection acquisition timed out")?
    }
}

impl Connection {
    pub(super) fn split(
        self,
    ) -> (
        impl tokio::io::AsyncBufRead + Unpin + Send,
        impl tokio::io::AsyncWrite + Unpin + Send,
    ) {
        let (read, write) = tokio::io::split(self.stream);
        (BufReader::new(read), write)
    }
    pub async fn command(&mut self, args: &[Vec<u8>]) -> Result<Frame> {
        self.send(args).await?;
        self.receive().await
    }

    pub async fn receive(&mut self) -> Result<Frame> {
        ensure!(
            !self.poisoned,
            "Home connection is fenced after interrupted I/O"
        );
        self.poisoned = true;
        let frame = resp::read_frame(&mut self.stream).await?;
        self.poisoned = false;
        Ok(frame)
    }

    pub async fn send(&mut self, args: &[Vec<u8>]) -> Result<()> {
        ensure!(
            !self.poisoned,
            "Home connection is fenced after interrupted I/O"
        );
        self.poisoned = true;
        resp::write_command(self.stream.get_mut(), args).await?;
        self.poisoned = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mtls_has_no_alpn_or_redis_initialization_and_cancellation_fences() {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
        use tokio::io::AsyncWriteExt;
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
        for (name, value) in [
            ("home-ca-crt.pem", ca.pem()),
            ("client-crt.pem", cert.pem()),
            ("client-key.pem", key.serialize_pem()),
        ] {
            std::fs::write(dir.path().join(name), value).unwrap();
        }
        let mut roots = RootCertStore::empty();
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
                vec![cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (release, wait) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let stream = tokio_rustls::TlsAcceptor::from(Arc::new(server))
                .accept(socket)
                .await
                .unwrap();
            assert!(stream.get_ref().1.alpn_protocol().is_none());
            assert!(stream.get_ref().1.peer_certificates().is_some());
            let mut stream = BufReader::new(stream);
            assert_eq!(
                resp::read_frame(&mut stream).await.unwrap(),
                Frame::Array(Some(vec![Frame::Bulk(Some(b"PING".to_vec()))]))
            );
            stream.get_mut().write_all(b"+PONG\r\n").await.unwrap();
            wait.await.unwrap();
        });
        let connector = Connector::from_identity("127.0.0.1".into(), port, dir.path())
            .await
            .unwrap();
        assert_eq!(connector.target(), format!("127.0.0.1:{port}"));
        assert_eq!(
            connector
                .for_target("::1".into(), port)
                .await
                .unwrap()
                .target(),
            format!("[::1]:{port}")
        );
        let mut connection = connector.connect().await.unwrap();
        assert_eq!(
            connection.command(&[b"PING".to_vec()]).await.unwrap(),
            Frame::Simple("PONG".into())
        );
        {
            let receive = connection.receive();
            tokio::pin!(receive);
            assert!(futures_util::poll!(receive.as_mut()).is_pending());
        }
        assert!(connection.receive().await.is_err());
        assert!(connection.send(&[b"PING".to_vec()]).await.is_err());
        release.send(()).unwrap();
        task.await.unwrap();
    }

    #[test]
    fn targets_are_tls_names_not_urls() {
        for host in ["localhost", "home.example.com", "127.0.0.1", "::1"] {
            assert!(validate_target(host, 8327).is_ok());
        }
        for host in [
            "",
            " home",
            "https://home",
            "home/path",
            "user@home",
            "[::1]",
            "home\n",
        ] {
            assert!(validate_target(host, 8327).is_err());
        }
        assert!(validate_target("home", 0).is_err());
    }
}
