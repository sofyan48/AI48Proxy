//! One-shot enrollment using Home's plaintext CERTIFICATE REQUEST protocol.
//!
//! SECURITY: the enrollment secret is sent in cleartext. The returned CA's pin
//! authenticates the CA, NOT this exchange, and cannot prevent interception or a
//! race to consume the one-shot secret. Use only over an operator-protected path
//! (e.g. a private tunnel). This is not pinned-TLS enrollment or a TLS fallback.
//!
//! Source contract: AI48Home internal/protocolmux/mux.go::classifyConn routes raw
//! RESP even with cluster TLS configured; internal/respserver/server.go accepts
//! CERTIFICATE REQUEST before authentication. cmd/home/main.go leaves the cluster
//! listener unwrapped and passes its TLS config to the protocol mux. Home also
//! accepts TLS without a client certificate, but this implementation does not use
//! that branch. The separate non-cluster HTTPS-only listener is not supported.
//!
//! The JWT is a trusted provisioning capability, not a locally authenticated JWT:
//! its signing key is not available until Home returns the pinned CA. Never accept
//! tokens from an untrusted source. No TLS verification is disabled here.
use super::{resp, transport};
use anyhow::{Context, Result, ensure};
use base64::{
    Engine,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{io::BufReader, net::TcpStream};

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_JWT_BYTES: usize = 64 * 1024;
const METADATA: &str = "enrollment.json";
const SENT: &str = "enrollment-sent";

#[derive(Deserialize)]
struct Claims {
    certificate_id: String,
    cluster_id: String,
    ca_fingerprint: String,
    enrollment_secret: String,
    ip: String,
    port: u16,
    iat: i64,
    exp: Option<i64>,
    nbf: Option<i64>,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    typ: String,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct Metadata {
    version: u8,
    token_sha256: String,
    certificate_id: String,
    cluster_id: String,
    ca_sha256: String,
}

#[derive(Deserialize)]
struct EnrollmentResponse {
    ok: bool,
    certificate: String,
    ca: String,
}

fn decode_part(part: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(part)
        .or_else(|_| URL_SAFE.decode(part))
        .context("invalid Home JWT encoding")
}

fn fingerprint(raw: &str) -> Result<String> {
    let normalized: String = raw
        .chars()
        .filter(|c| *c != ':' && *c != ' ')
        .flat_map(char::to_lowercase)
        .collect();
    ensure!(
        normalized.len() == 64 && normalized.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid Home CA SHA-256 fingerprint"
    );
    Ok(normalized)
}

fn parse_claims(jwt: &str, now: i64) -> Result<Claims> {
    ensure!(jwt.len() <= MAX_JWT_BYTES, "Home JWT exceeds limit");
    let parts: Vec<_> = jwt.trim().split('.').collect();
    ensure!(
        parts.len() == 3 && parts.iter().all(|p| !p.is_empty()),
        "invalid Home JWT structure"
    );
    let header: Header =
        serde_json::from_slice(&decode_part(parts[0])?).context("invalid Home JWT header")?;
    ensure!(
        header.alg == "RS256" && header.typ == "JWT",
        "unsupported Home JWT header"
    );
    let signature = decode_part(parts[2])?;
    ensure!(
        (256..=1024).contains(&signature.len()),
        "invalid Home JWT RSA signature length"
    );
    let mut claims: Claims =
        serde_json::from_slice(&decode_part(parts[1])?).context("invalid Home JWT claims")?;
    for value in [
        &claims.certificate_id,
        &claims.cluster_id,
        &claims.enrollment_secret,
    ] {
        ensure!(
            !value.is_empty()
                && value.len() <= 4096
                && value == value.trim()
                && !value.chars().any(char::is_control),
            "invalid Home JWT identity claim"
        );
    }
    claims.ca_fingerprint = fingerprint(&claims.ca_fingerprint)?;
    transport::validate_target(&claims.ip, claims.port)?;
    ensure!(
        claims.iat > 0 && claims.iat <= now.saturating_add(300),
        "invalid Home JWT issued-at time"
    );
    ensure!(
        claims.exp.is_none_or(|exp| exp > now && exp > claims.iat),
        "Home JWT has expired or has invalid expiry"
    );
    ensure!(
        claims.nbf.is_none_or(|nbf| nbf <= now),
        "Home JWT is not yet valid"
    );
    Ok(claims)
}

fn check_regular(path: &Path, file: &File) -> Result<()> {
    let before = fs::symlink_metadata(path)?;
    let opened = file.metadata()?;
    ensure!(
        before.is_file() && opened.is_file(),
        "identity path must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            before.dev() == opened.dev() && before.ino() == opened.ino() && opened.nlink() == 1,
            "identity file changed or is linked"
        );
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    ensure!(
        opened.len() <= MAX_FILE_BYTES,
        "identity file exceeds size limit"
    );
    Ok(())
}

pub(crate) fn read_identity_file(path: &Path) -> Result<Vec<u8>> {
    ensure!(
        !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "identity symlinks are forbidden"
    );
    let file = File::open(path).context("open identity file")?;
    check_regular(path, &file)?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        "identity file exceeds size limit"
    );
    Ok(bytes)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .context("create identity file (existing paths are never overwritten)")?;
    file.write_all(bytes)?;
    file.sync_all()?;
    #[cfg(unix)]
    File::open(path.parent().context("identity file has no parent")?)?.sync_all()?;
    Ok(())
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

struct EnrollmentLock(PathBuf);
impl Drop for EnrollmentLock {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.0) {
            tracing::warn!(%error, "failed to remove Home enrollment lock");
        }
    }
}

fn verify_ca(pem: &[u8], expected: &str) -> Result<()> {
    let certs = transport::certificates(pem)?;
    ensure!(certs.len() == 1, "Home must return exactly one pinned CA");
    ensure!(
        hex::encode(Sha256::digest(certs[0].as_ref())) == expected,
        "Home CA fingerprint mismatch"
    );
    Ok(())
}

fn validate_identity(ca: &[u8], cert: &[u8], key: &[u8]) -> Result<()> {
    transport::identity_config(ca, cert, key)?;
    let ca_certs = transport::certificates(ca)?;
    let certs = transport::certificates(cert)?;
    let mut roots = rustls::RootCertStore::empty();
    for root in ca_certs {
        roots.add(root)?;
    }
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::new(rustls::crypto::ring::default_provider()),
    )
    .build()?;
    verifier
        .verify_client_cert(&certs[0], &certs[1..], rustls::pki_types::UnixTime::now())
        .context("Home client certificate is not valid under pinned CA")?;
    Ok(())
}

/// Enroll once, or reuse a complete identity bound to exactly this token.
/// Ambiguous network failures leave an enrollment-sent marker: manual recovery or
/// a new provisioning token/directory is required, never an automatic retry.
/// On Unix the directory is 0700 and files 0600. Other platforms need an OS ACL.
/// The identity directory's ancestors must be controlled by the operator.
pub async fn enroll(jwt: &str, identity_dir: &Path) -> Result<(String, u16)> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .try_into()?;
    let claims = parse_claims(jwt, now)?;
    fs::create_dir_all(identity_dir)?;
    let directory = fs::symlink_metadata(identity_dir)?;
    ensure!(
        directory.is_dir() && !directory.file_type().is_symlink(),
        "identity directory must not be a symlink"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(identity_dir, fs::Permissions::from_mode(0o700))?;
    }
    let lock_path = identity_dir.join("enrollment.lock");
    write_new(&lock_path, b"locked\n")
        .context("Home enrollment locked; inspect stale lock manually after a crash")?;
    let _lock = EnrollmentLock(lock_path);
    let expected = Metadata {
        version: 1,
        token_sha256: hex::encode(Sha256::digest(jwt.trim().as_bytes())),
        certificate_id: claims.certificate_id.clone(),
        cluster_id: claims.cluster_id.clone(),
        ca_sha256: claims.ca_fingerprint.clone(),
    };
    let meta_path = identity_dir.join(METADATA);
    let key_path = identity_dir.join("client-key.pem");
    let cert_path = identity_dir.join("client-crt.pem");
    let ca_path = identity_dir.join("home-ca-crt.pem");
    let sent_path = identity_dir.join(SENT);
    if exists(&meta_path)? {
        let saved: Metadata = serde_json::from_slice(&read_identity_file(&meta_path)?)
            .context("invalid enrollment metadata")?;
        ensure!(
            saved == expected,
            "identity directory belongs to a different provisioning token"
        );
    } else {
        for path in [&key_path, &cert_path, &ca_path, &sent_path] {
            ensure!(
                !exists(path)?,
                "unbound identity files exist; use a separate identity directory"
            );
        }
        write_new(&meta_path, &serde_json::to_vec(&expected)?)?;
    }
    if exists(&cert_path)? && exists(&ca_path)? {
        let ca = read_identity_file(&ca_path)?;
        verify_ca(&ca, &claims.ca_fingerprint)?;
        validate_identity(
            &ca,
            &read_identity_file(&cert_path)?,
            &read_identity_file(&key_path)?,
        )?;
        return Ok((claims.ip, claims.port));
    }
    ensure!(
        !exists(&sent_path)? && !exists(&cert_path)? && !exists(&ca_path)?,
        "enrollment was already attempted or identity is incomplete; automatic retry is unsafe"
    );
    let key = if exists(&key_path)? {
        KeyPair::from_pem(std::str::from_utf8(&read_identity_file(&key_path)?)?)?
    } else {
        // Home checks the CSR signature and signs csr.PublicKey without an RSA
        // subject-key restriction (internal/cluster/certificate.go). Its RSA CA
        // can therefore issue a certificate for this ECDSA P-256 client key.
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        write_new(&key_path, key.serialize_pem().as_bytes())?;
        key
    };
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, &claims.certificate_id);
    let csr = params.serialize_request(&key)?.pem()?;
    // Persist the fence before any bytes can consume the one-shot secret.
    write_new(
        &sent_path,
        b"one-shot enrollment attempted; do not automatically retry\n",
    )?;
    let response: EnrollmentResponse = tokio::time::timeout(Duration::from_secs(30), async {
        let socket = TcpStream::connect((claims.ip.as_str(), claims.port))
            .await
            .context("connect for Home enrollment")?;
        let mut stream = BufReader::new(socket);
        resp::write_command(
            stream.get_mut(),
            &[
                b"CERTIFICATE".to_vec(),
                b"REQUEST".to_vec(),
                claims.certificate_id.as_bytes().to_vec(),
                claims.enrollment_secret.as_bytes().to_vec(),
                csr.into_bytes(),
            ],
        )
        .await?;
        let raw = resp::read_frame(&mut stream).await?.bytes()?;
        ensure!(
            raw.len() as u64 <= MAX_FILE_BYTES,
            "Home enrollment response exceeds limit"
        );
        serde_json::from_slice(&raw).context("invalid Home enrollment response")
    })
    .await
    .context("Home enrollment timed out; request may have been consumed")??;
    ensure!(response.ok, "Home enrollment rejected");
    verify_ca(response.ca.as_bytes(), &claims.ca_fingerprint)?;
    validate_identity(
        response.ca.as_bytes(),
        response.certificate.as_bytes(),
        key.serialize_pem().as_bytes(),
    )?;
    write_new(&ca_path, response.ca.as_bytes())?;
    write_new(&cert_path, response.certificate.as_bytes())?;
    Ok((claims.ip, claims.port))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn token(claims: serde_json::Value, alg: &str) -> String {
        format!(
            "{}.{}.{}",
            URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&serde_json::json!({"alg":alg,"typ":"JWT"})).unwrap()),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap()),
            URL_SAFE_NO_PAD.encode([1; 256])
        )
    }
    fn claims() -> serde_json::Value {
        serde_json::json!({"certificate_id":"node", "cluster_id":"cluster", "ca_fingerprint":"ab".repeat(32),
            "enrollment_secret":"secret", "ip":"127.0.0.1", "port":8327, "iat":1000})
    }
    #[test]
    fn validates_jwt_claims_and_algorithm() {
        assert!(parse_claims(&token(claims(), "RS256"), 1000).is_ok());
        for alg in ["none", "HS256", "rs256"] {
            assert!(parse_claims(&token(claims(), alg), 1000).is_err());
        }
        for (field, value) in [
            ("port", serde_json::json!(65536)),
            ("port", serde_json::json!(0)),
            ("ip", serde_json::json!("https://host")),
            ("certificate_id", serde_json::json!(" ")),
            ("ca_fingerprint", serde_json::json!("ab")),
            ("exp", serde_json::json!(999)),
            ("nbf", serde_json::json!(1001)),
            ("iat", serde_json::json!(1301)),
        ] {
            let mut payload = claims();
            payload[field] = value;
            assert!(
                parse_claims(&token(payload, "RS256"), 1000).is_err(),
                "{field}"
            );
        }
        for raw in ["", "a.b", "a.b.c.d", "..", "e30.e30.AA"] {
            assert!(parse_claims(raw, 1000).is_err());
        }
    }
    #[test]
    fn pin_requires_exactly_the_expected_ca() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .unwrap()
            .cert;
        let pin = hex::encode(Sha256::digest(cert.der().as_ref()));
        assert!(verify_ca(cert.pem().as_bytes(), &pin).is_ok());
        assert!(verify_ca(cert.pem().as_bytes(), &"00".repeat(32)).is_err());
        assert!(verify_ca(format!("{}{}", cert.pem(), cert.pem()).as_bytes(), &pin).is_err());
    }
    #[tokio::test]
    async fn enrolls_once_reuses_identity_and_rejects_other_tokens() {
        use rcgen::{
            BasicConstraints, CertificateSigningRequestParams, IsCa, Issuer, KeyUsagePurpose,
        };
        use tokio::{io::AsyncWriteExt, net::TcpListener};
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca_key = KeyPair::generate().unwrap();
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let pin = hex::encode(Sha256::digest(ca.der().as_ref()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut payload = claims();
        payload["port"] = serde_json::json!(listener.local_addr().unwrap().port());
        payload["iat"] = serde_json::json!(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        );
        payload["ca_fingerprint"] = serde_json::json!(pin);
        let jwt = token(payload.clone(), "RS256");
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("client-key.pem");
        let sent_path = dir.path().join(SENT);
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(socket);
            let resp::Frame::Array(Some(args)) = resp::read_frame(&mut stream).await.unwrap()
            else {
                panic!("expected command")
            };
            let args: Vec<Vec<u8>> = args
                .into_iter()
                .map(|frame| frame.bytes().unwrap())
                .collect();
            assert_eq!(
                &args[..4],
                &[
                    b"CERTIFICATE".to_vec(),
                    b"REQUEST".to_vec(),
                    b"node".to_vec(),
                    b"secret".to_vec()
                ]
            );
            assert!(key_path.exists() && sent_path.exists());
            let persisted_key = KeyPair::from_pem(
                std::str::from_utf8(&read_identity_file(&key_path).unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(persisted_key.algorithm(), &rcgen::PKCS_ECDSA_P256_SHA256);
            // Reading the command directly from TCP also asserts plaintext RESP,
            // not TLS: this mock performs no TLS handshake.
            let csr =
                CertificateSigningRequestParams::from_pem(std::str::from_utf8(&args[4]).unwrap())
                    .unwrap();
            let cert = csr
                .signed_by(&Issuer::from_params(&ca_params, &ca_key))
                .unwrap();
            let response = serde_json::to_vec(
                &serde_json::json!({"ok":true,"certificate":cert.pem(),"ca":ca.pem()}),
            )
            .unwrap();
            stream
                .get_mut()
                .write_all(format!("${}\r\n", response.len()).as_bytes())
                .await
                .unwrap();
            stream.get_mut().write_all(&response).await.unwrap();
            stream.get_mut().write_all(b"\r\n").await.unwrap();
        });
        let target = enroll(&jwt, dir.path()).await.unwrap();
        server.await.unwrap();
        assert_eq!(enroll(&jwt, dir.path()).await.unwrap(), target);
        payload["certificate_id"] = serde_json::json!("different-node");
        assert!(enroll(&token(payload, "RS256"), dir.path()).await.is_err());
    }

    #[tokio::test]
    async fn attempted_enrollment_is_never_retried() {
        let dir = tempfile::tempdir().unwrap();
        let mut payload = claims();
        payload["iat"] = serde_json::json!(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        );
        // A closed local listener fails without sending a secret but is still conservatively fenced.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        payload["port"] = serde_json::json!(listener.local_addr().unwrap().port());
        drop(listener);
        let jwt = token(payload, "RS256");
        assert!(enroll(&jwt, dir.path()).await.is_err());
        assert!(dir.path().join("client-key.pem").exists());
        let error = enroll(&jwt, dir.path()).await.unwrap_err();
        assert!(error.to_string().contains("automatic retry is unsafe"));
    }

    #[test]
    fn secure_files_are_exclusive_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        write_new(&path, b"private").unwrap();
        assert!(write_new(&path, b"overwrite").is_err());
        assert_eq!(read_identity_file(&path).unwrap(), b"private");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let link = dir.path().join("link");
            symlink(&path, &link).unwrap();
            assert!(read_identity_file(&link).is_err());
        }
    }
}
