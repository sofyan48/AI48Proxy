//! Async identity preparation for Home dispatch clones, before provider payload preparation.
//! Mirrors CPA's claude_credential_identity.go and claude_executor_auth.go. Home KV
//! stores device IDs only; account profile fields live on the request's auth clone.
use super::{access_token, text};
use crate::home::{
    client::{HomeClient, Selection},
    kv::{Kv, SetMode, SetOptions},
};
use anyhow::{Result, anyhow, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const POOL: &str = "claude_device_ids";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";

/// Call for each selected/refreshed credential before providers::prepare. Errors
/// intentionally omit transport causes, response bodies, credentials and KV data.
pub async fn prepare(home: &HomeClient, selection: &mut Selection) -> Result<()> {
    let config = home.config();
    let backend = HomeBackend {
        home,
        default_proxy: config.proxy_url.as_deref(),
    };
    prepare_with(&backend, selection).await
}

trait Backend {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    async fn set(&self, key: &str, value: &[u8], mode: SetMode) -> Result<bool>;
    async fn profile(&self, token: &str, auth: &Value) -> Result<Option<Value>>;
    fn device(&self) -> Result<String>;
}

struct HomeBackend<'a> {
    home: &'a HomeClient,
    default_proxy: Option<&'a str>,
}
impl Backend for HomeBackend<'_> {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Kv::new(self.home)
            .get(key)
            .await
            .map_err(|_| anyhow!("Claude identity Home KV read failed"))
    }
    async fn set(&self, key: &str, value: &[u8], mode: SetMode) -> Result<bool> {
        Kv::new(self.home)
            .set(key, value, SetOptions { mode, expiry: None })
            .await
            .map_err(|_| anyhow!("Claude identity Home KV write failed"))
    }
    async fn profile(&self, token: &str, auth: &Value) -> Result<Option<Value>> {
        let proxy = [
            text(auth, "/proxy_url"),
            text(auth, "/attributes/proxy_url"),
        ]
        .into_iter()
        .find(|p| !p.is_empty())
        .or(self.default_proxy);
        let client = profile_client(proxy)?;
        fetch_profile(&client, PROFILE_URL, token).await
    }
    fn device(&self) -> Result<String> {
        // CPA uses 32 cryptographically random bytes, not a UUID or token hash.
        let mut bytes = [0; 32];
        rustls::crypto::ring::default_provider()
            .secure_random
            .fill(&mut bytes)
            .map_err(|_| anyhow!("Claude device random source failed"))?;
        Ok(hex::encode(bytes))
    }
}

fn profile_client(proxy: Option<&str>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(30));
    // No whole-request/read timeout after connection establishment.
    if let Some(proxy) = proxy.filter(|p| *p != "direct") {
        let parsed = url::Url::parse(proxy).map_err(|_| anyhow!("invalid Claude profile proxy"))?;
        ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "unsupported Claude profile proxy"
        );
        builder = builder.proxy(
            reqwest::Proxy::all(proxy).map_err(|_| anyhow!("invalid Claude profile proxy"))?,
        );
    }
    builder
        .build()
        .map_err(|_| anyhow!("Claude profile client failed"))
}

async fn fetch_profile(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
) -> Result<Option<Value>> {
    let response = client
        .get(endpoint)
        .bearer_auth(token)
        .header("accept", "application/json, text/plain, */*")
        .header("content-type", "application/json")
        .header("user-agent", "axios/1.15.2")
        .header("cache-control", "no-cache")
        .header("connection", "close")
        // Advertise only the compression supported by this reqwest build.
        .header("accept-encoding", "gzip")
        .send()
        .await
        .map_err(|_| anyhow!("Claude OAuth profile transport failed"))?;
    if response.status() == reqwest::StatusCode::FORBIDDEN {
        return Ok(None);
    }
    ensure!(
        response.status().is_success(),
        "Claude OAuth profile HTTP status {}",
        response.status().as_u16()
    );
    let profile: Value = response
        .json()
        .await
        .map_err(|_| anyhow!("invalid Claude OAuth profile JSON"))?;
    ensure!(profile.is_object(), "invalid Claude OAuth profile shape");
    Ok(Some(profile))
}

async fn prepare_with(backend: &impl Backend, selection: &mut Selection) -> Result<()> {
    if !selection.provider.eq_ignore_ascii_case("claude") {
        return Ok(());
    }
    let auth = &selection.auth;
    let key = text(auth, "/attributes/api_key");
    let token = if key.is_empty() {
        access_token(auth)
    } else {
        key
    }
    .to_owned();
    // Same token classification as CPA isClaudeOAuthToken; API keys are untouched.
    if !token.contains("sk-ant-oat") {
        return Ok(());
    }
    ensure!(auth.is_object(), "invalid Claude Home credential");
    ensure!(
        auth.get("metadata")
            .is_none_or(|m| m.is_null() || m.is_object()),
        "invalid Claude credential metadata"
    );
    let device = ensure_pool(backend, selection).await?;
    let existing = account(&selection.auth).to_owned();
    let mut fields = serde_json::Map::new();
    if existing.is_empty() {
        let setup = setup_token(&selection.auth);
        let profile = if setup {
            None
        } else {
            backend.profile(&token, &selection.auth).await?
        };
        let resolved = profile
            .as_ref()
            .map(|p| text(p, "/account/uuid"))
            .unwrap_or("");
        let resolved = if resolved.is_empty() {
            stable_account(&identity_seed(selection, &token, setup))
        } else {
            resolved.to_owned()
        };
        fields.insert("account_uuid".into(), json!(resolved));
        if let Some(profile) = profile {
            for (key, path) in [
                ("email", "/account/email"),
                ("organization_uuid", "/organization/uuid"),
                ("organization_name", "/organization/name"),
            ] {
                let value = text(&profile, path);
                if !value.is_empty() {
                    fields.insert(key.into(), json!(value));
                }
            }
        }
        fields.insert(
            "claude_account_profile_checked_at".into(),
            json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        );
    }
    // Root account aliases from Home are authoritative too, but must be promoted
    // for the native provider's metadata-only reader. Never use generic `uuid`.
    else {
        fields.insert("account_uuid".into(), json!(existing));
    }
    fields.insert(POOL.into(), json!([device]));
    let auth = selection
        .auth
        .as_object_mut()
        .expect("validated credential object");
    let metadata = auth.entry("metadata").or_insert_with(|| json!({}));
    if metadata.is_null() {
        *metadata = json!({});
    }
    metadata
        .as_object_mut()
        .expect("validated metadata object")
        .extend(fields);
    Ok(())
}

fn account(auth: &Value) -> &str {
    [
        "/metadata/account_uuid",
        "/metadata/accountUuid",
        "/account_uuid",
        "/accountUuid",
    ]
    .into_iter()
    .map(|p| text(auth, p))
    .find(|v| !v.is_empty())
    .unwrap_or("")
}
fn normalize(raw: &Value) -> Option<String> {
    raw.as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
        .find(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}
fn canonical(raw: &Value, device: &str) -> bool {
    raw.as_array()
        .is_some_and(|a| a.len() == 1 && a[0].as_str() == Some(device))
}
fn pool_key(selection: &Selection) -> Result<String> {
    // Home dispatch AuthIndex is installed as Auth.Index by Go conductor_home.go.
    let identity = selection.auth_index.trim();
    let identity = if identity.is_empty() {
        text(&selection.auth, "/id")
    } else {
        identity
    };
    ensure!(!identity.is_empty(), "Claude credential identity missing");
    Ok(format!(
        "cpa:claude:credential-device-pool:{}",
        hex::encode(Sha256::digest(identity.as_bytes()))
    ))
}
async fn ensure_pool(backend: &impl Backend, selection: &Selection) -> Result<String> {
    let raw = &selection.auth["metadata"][POOL];
    let candidate = normalize(raw);
    if let Some(device) = &candidate
        && canonical(raw, device)
    {
        return Ok(device.clone());
    }
    let key = pool_key(selection)?;
    if let Some(raw) = backend.get(&key).await? {
        // Go decodes []string, so a mixed-type stored array is not accepted.
        if let Ok(stored) = serde_json::from_slice::<Vec<String>>(&raw) {
            let stored = json!(stored);
            if let Some(device) = normalize(&stored) {
                if !canonical(&stored, &device) {
                    ensure!(
                        backend
                            .set(&key, &serde_json::to_vec(&[&device])?, SetMode::Xx)
                            .await?,
                        "Claude pool canonicalization lost Home value"
                    );
                }
                return Ok(device);
            }
        }
    }
    let device = match candidate {
        Some(device) => device,
        None => backend.device()?,
    };
    backend
        .set(&key, &serde_json::to_vec(&[device])?, SetMode::Nx)
        .await?;
    // Even an NX winner rereads: another node's existing identity wins.
    let raw = backend
        .get(&key)
        .await?
        .ok_or_else(|| anyhow!("Claude Home pool missing after set"))?;
    let stored: Vec<String> =
        serde_json::from_slice(&raw).map_err(|_| anyhow!("invalid Claude Home pool JSON"))?;
    normalize(&json!(stored)).ok_or_else(|| anyhow!("invalid Claude Home device pool"))
}
fn setup_token(auth: &Value) -> bool {
    if ["skip_account_profile", "is_setup_token", "setup_token"]
        .iter()
        .any(|k| auth["metadata"][k].as_bool() == Some(true))
    {
        return true;
    }
    if matches!(
        text(auth, "/attributes/auth_kind")
            .to_ascii_lowercase()
            .as_str(),
        "setup_token" | "setup-token"
    ) {
        return true;
    }
    let scopes = text(auth, "/metadata/scopes");
    let scopes = if scopes.is_empty() {
        text(auth, "/metadata/scope")
    } else {
        scopes
    }
    .to_ascii_lowercase();
    !scopes.is_empty() && !scopes.contains("user:profile") && !scopes.contains("user:office")
}
fn identity_seed(selection: &Selection, token: &str, setup: bool) -> String {
    for (prefix, value) in [
        ("auth-id|", text(&selection.auth, "/id")),
        ("auth-index|", selection.auth_index.trim()),
    ] {
        if !value.is_empty() {
            return format!("{prefix}{value}");
        }
    }
    format!(
        "{}|{token}",
        if setup {
            "claude-setup-token"
        } else {
            "claude-oauth-fallback"
        }
    )
}

fn stable_account(seed: &str) -> String {
    // UUIDv5 compatibility, not a security hash. uuid's v5 feature is not enabled
    // in this crate; keep this SHA-1 implementation private to CPA identity naming.
    let mut input = uuid::Uuid::NAMESPACE_OID.as_bytes().to_vec();
    input.extend_from_slice(b"cpa-claude-code-cli-account|");
    input.extend_from_slice(seed.as_bytes());
    let hash = sha1_name(&input);
    let mut bytes: [u8; 16] = hash[..16].try_into().expect("SHA-1 length");
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}
fn sha1_name(input: &[u8]) -> [u8; 20] {
    let mut data = input.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&(input.len() as u64 * 8).to_be_bytes());
    let mut h = [
        0x67452301u32,
        0xefcdab89,
        0x98badcfe,
        0x10325476,
        0xc3d2e1f0,
    ];
    for chunk in data.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().enumerate().take(16) {
            let offset = i * 4;
            *word = u32::from_be_bytes(chunk[offset..offset + 4].try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5a827999u32),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut out = [0; 20];
    for (i, value) in h.into_iter().enumerate() {
        let offset = i * 4;
        out[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, sync::Mutex};
    struct Mock {
        reads: Mutex<VecDeque<Option<Vec<u8>>>>,
        writes: Mutex<Vec<(String, Value, SetMode)>>,
        profile: Option<Value>,
    }
    impl Backend for Mock {
        async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
            assert_eq!(key, pool_key(&selection()).unwrap());
            Ok(self
                .reads
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected KV read"))
        }
        async fn set(&self, key: &str, raw: &[u8], mode: SetMode) -> Result<bool> {
            self.writes.lock().unwrap().push((
                key.into(),
                serde_json::from_slice(raw).unwrap(),
                mode,
            ));
            Ok(mode != SetMode::Nx)
        }
        async fn profile(&self, _: &str, _: &Value) -> Result<Option<Value>> {
            Ok(self.profile.clone())
        }
        fn device(&self) -> Result<String> {
            Ok("a".repeat(64))
        }
    }
    fn selection() -> Selection {
        serde_json::from_value(json!({"model":"claude", "provider":"claude", "auth_index":"index", "auth":{"id":"credential", "metadata":{"access_token":"sk-ant-oat-fixture"}}})).unwrap()
    }
    fn mock(reads: Vec<Option<Value>>, profile: Option<Value>) -> Mock {
        Mock {
            reads: Mutex::new(
                reads
                    .into_iter()
                    .map(|v| v.map(|v| serde_json::to_vec(&v).unwrap()))
                    .collect(),
            ),
            writes: Mutex::default(),
            profile,
        }
    }
    #[tokio::test]
    async fn nx_rereads_winner_and_profile_uses_nested_account() {
        let winner = "b".repeat(64);
        let backend = mock(
            vec![None, Some(json!([winner]))],
            Some(json!({"account":{"uuid":"real-account"}})),
        );
        let mut s = selection();
        prepare_with(&backend, &mut s).await.unwrap();
        assert_eq!(s.auth["metadata"][POOL], json!([winner]));
        assert_eq!(account(&s.auth), "real-account");
        let writes = backend.writes.lock().unwrap();
        assert_eq!(
            writes[0].0,
            format!(
                "cpa:claude:credential-device-pool:{}",
                hex::encode(Sha256::digest(b"index"))
            )
        );
        assert_eq!(writes[0].1, json!(["a".repeat(64)]));
        assert_eq!(writes[0].2, SetMode::Nx);
    }
    #[tokio::test]
    async fn legacy_pool_is_first_valid_and_canonicalized_xx() {
        let backend = mock(
            vec![Some(json!([
                "bad",
                format!(" {} ", "B".repeat(64)),
                "c".repeat(64)
            ]))],
            None,
        );
        let mut s = selection();
        s.auth["account_uuid"] = json!("root-account");
        prepare_with(&backend, &mut s).await.unwrap();
        assert_eq!(s.auth["metadata"][POOL], json!(["b".repeat(64)]));
        assert_eq!(s.auth["metadata"]["account_uuid"], "root-account");
        assert_eq!(backend.writes.lock().unwrap()[0].2, SetMode::Xx);
    }
    #[tokio::test]
    async fn canonical_pool_skips_kv_and_missing_profile_falls_back_stably() {
        for profile in [None, Some(json!({"account":{}}))] {
            let backend = mock(vec![], profile);
            let mut s = selection();
            s.auth["metadata"][POOL] = json!(["a".repeat(64)]);
            prepare_with(&backend, &mut s).await.unwrap();
            let first = account(&s.auth).to_owned();
            s.auth["metadata"]
                .as_object_mut()
                .unwrap()
                .remove("account_uuid");
            s.auth["metadata"]["access_token"] = json!("sk-ant-oat-rotated");
            prepare_with(&backend, &mut s).await.unwrap();
            assert_eq!(account(&s.auth), first);
            assert_eq!(first, stable_account("auth-id|credential"));
            assert!(backend.writes.lock().unwrap().is_empty());
        }
    }
    #[tokio::test]
    async fn api_keys_and_other_providers_are_noops_and_corrupt_kv_fails_closed() {
        let backend = mock(vec![], None);
        let mut s = selection();
        s.auth["attributes"] = json!({"api_key":"sk-ant-api-fixture"});
        let before = s.auth.clone();
        prepare_with(&backend, &mut s).await.unwrap();
        assert_eq!(s.auth, before);
        s.provider = "codex".into();
        prepare_with(&backend, &mut s).await.unwrap();
        let backend = mock(vec![Some(json!([1])), Some(json!([1]))], None);
        assert!(prepare_with(&backend, &mut selection()).await.is_err());
    }
    #[tokio::test]
    async fn profile_http_shape_status_and_safe_errors() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (status, body) in [
            (200, r#"{"account":{"uuid":"upstream"}}"#),
            (403, "fixture-secret"),
            (401, "fixture-secret"),
            (200, "fixture-secret"),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!(
                "http://{}/api/oauth/profile",
                listener.local_addr().unwrap()
            );
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&chunk[..count]);
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                assert!(request.starts_with("get /api/oauth/profile http/1.1\r\n"));
                assert!(request.contains("authorization: bearer fixture-token\r\n"));
                assert!(request.contains("user-agent: axios/1.15.2\r\n"));
                assert!(!request.contains("x-api-key:"));
                let reply = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
            });
            let result =
                fetch_profile(&profile_client(None).unwrap(), &endpoint, "fixture-token").await;
            server.await.unwrap();
            match (status, body.starts_with('{')) {
                (403, _) => assert!(result.unwrap().is_none()),
                (200, true) => assert_eq!(result.unwrap().unwrap()["account"]["uuid"], "upstream"),
                _ => {
                    let error = format!("{:#}", result.unwrap_err());
                    assert!(!error.contains("fixture-secret"));
                    assert!(!error.contains("fixture-token"));
                }
            }
        }
    }
    #[test]
    fn normalization_seed_precedence_and_sha1_vectors() {
        assert_eq!(
            normalize(&json!([null, "bad", "A".repeat(64)])),
            Some("a".repeat(64))
        );
        assert_eq!(
            hex::encode(sha1_name(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex::encode(sha1_name(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            stable_account("auth-id|credential"),
            "c8a859b5-9aa7-540e-a15b-e2557eccc05f"
        );
        let mut s = selection();
        assert_eq!(identity_seed(&s, "fixture", false), "auth-id|credential");
        s.auth.as_object_mut().unwrap().remove("id");
        assert_eq!(identity_seed(&s, "fixture", false), "auth-index|index");
        s.auth_index.clear();
        assert_eq!(
            identity_seed(&s, "fixture", true),
            "claude-setup-token|fixture"
        );
        s.auth["metadata"]["scopes"] = json!("user:inference");
        assert!(setup_token(&s.auth));
        s.auth["metadata"]["scopes"] = json!("user:profile user:inference");
        assert!(!setup_token(&s.auth));
    }
}
