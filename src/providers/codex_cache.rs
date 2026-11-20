//! Go helps/derived_session.go and claude_code_session.go identity naming.
//! Caller context must be supplied by the gateway, never read from credential metadata.
use http::HeaderMap;
use serde_json::Value;

/// Derive a provider-scoped UUID without exposing the caller identity on the wire.
pub fn provider_session(execution: &str, derived: &str) -> Option<String> {
    let (kind, identity) = if !execution.trim().is_empty() {
        ("execution-session", execution.trim())
    } else {
        ("derived-session", derived.trim())
    };
    (!identity.is_empty()).then(|| name_uuid(&format!("cli-proxy-api\0codex\0{kind}\0{identity}")))
}

/// Responses keys are verbatim; Chat keys are trimmed and fall back to caller scope.
#[allow(clippy::too_many_arguments)]
pub fn key(
    source: &str,
    original: &Value,
    headers: &HeaderMap,
    model: &str,
    execution: &str,
    derived: &str,
    api_key: &str,
) -> Option<String> {
    let explicit = original.get("prompt_cache_key").and_then(Value::as_str);
    let value = match source {
        "claude" => claude_key(original, headers, model),
        "openai" => explicit
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        _ => explicit.filter(|s| !s.is_empty()).map(str::to_owned),
    };
    value
        .or_else(|| provider_session(execution, derived))
        .or_else(|| {
            (source == "openai" && !api_key.trim().is_empty()).then(|| {
                name_uuid(&format!(
                    "cli-proxy-api:codex:prompt-cache:{}",
                    api_key.trim()
                ))
            })
        })
}

fn claude_key(original: &Value, headers: &HeaderMap, model: &str) -> Option<String> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let user = original
        .pointer("/metadata/user_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let parsed: Option<Value> = user
        .starts_with('{')
        .then(|| serde_json::from_str(user).ok())
        .flatten();
    let session = header("x-claude-code-session-id")
        .or_else(|| {
            user.rsplit_once("_session_").map(|(_, s)| s).filter(|s| {
                !s.is_empty()
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
            })
        })
        .or_else(|| {
            parsed
                .as_ref()?
                .get("session_id")?
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })?;
    if model.trim().is_empty() {
        return None;
    }
    let agent = header("x-claude-code-agent-id").unwrap_or("main");
    Some(name_uuid(&format!(
        "cli-proxy-api:codex:claude-code\0{}\0claude:{session}:agent:{agent}",
        model.trim()
    )))
}

// UUIDv5 uses SHA-1 for naming only, not authentication. The uuid v5 feature is
// unavailable in the existing dependency configuration.
fn name_uuid(name: &str) -> String {
    let mut input = uuid::Uuid::NAMESPACE_OID.as_bytes().to_vec();
    input.extend_from_slice(name.as_bytes());
    let length = input.len() as u64 * 8;
    input.push(0x80);
    while input.len() % 64 != 56 {
        input.push(0);
    }
    input.extend_from_slice(&length.to_be_bytes());
    let mut h = [
        0x67452301u32,
        0xefcdab89,
        0x98badcfe,
        0x10325476,
        0xc3d2e1f0,
    ];
    for block in input.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
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
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        for (value, add) in h.iter_mut().zip([a, b, c, d, e]) {
            *value = value.wrapping_add(add);
        }
    }
    let mut bytes = [0u8; 16];
    for (chunk, word) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    bytes[6] = (bytes[6] & 15) | 0x50;
    bytes[8] = (bytes[8] & 63) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn uuid_matches_rfc_and_scopes_are_isolated() {
        assert_eq!(name_uuid(""), "0a68eb57-c88a-5f34-9e9d-27f85e68af4f");
        assert_ne!(provider_session("a", ""), provider_session("", "a"));
        assert_eq!(provider_session("a", "b"), provider_session("a", "c"));
        assert_eq!(provider_session("", ""), None);
    }
    #[test]
    fn explicit_and_claude_scope_precedence() {
        let mut headers = HeaderMap::new();
        let original = json!({"prompt_cache_key":" verbatim ","metadata":{"user_id":"bare-user"}});
        assert_eq!(
            key("openai-response", &original, &headers, "m", "", "", "").as_deref(),
            Some(" verbatim ")
        );
        assert_eq!(
            key("openai", &original, &headers, "m", "", "", "").as_deref(),
            Some("verbatim")
        );
        assert_eq!(key("claude", &original, &headers, "m", "", "", ""), None);
        headers.insert("x-claude-code-session-id", "root".parse().unwrap());
        let root = key("claude", &original, &headers, "m", "", "", "");
        headers.insert("x-claude-code-agent-id", "child".parse().unwrap());
        assert_ne!(root, key("claude", &original, &headers, "m", "", "", ""));
        assert_ne!(
            root,
            key("claude", &original, &headers, "other", "", "", "")
        );
    }
}
