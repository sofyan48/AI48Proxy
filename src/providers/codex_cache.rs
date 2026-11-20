//! Go helps/derived_session.go and claude_code_session.go identity naming.
//! Caller context must be supplied by the gateway, never read from credential metadata.
use http::HeaderMap;
use serde_json::Value;

#[derive(Default)]
pub(crate) struct Context {
    pub execution: String,
    pub derived: String,
    pub api_key: String,
}

tokio::task_local! {
    static CONTEXT: Context;
}

/// Only execution-owned context may supply these identities, never payload metadata.
pub(crate) fn with_context<T>(context: Context, prepare: impl FnOnce() -> T) -> T {
    CONTEXT.sync_scope(context, prepare)
}

pub(crate) fn http_key(
    source: &str,
    original: &Value,
    headers: &HeaderMap,
    model: &str,
    api_key: &str,
) -> Option<String> {
    let resolve = |context: &Context| {
        key(
            source,
            original,
            headers,
            model,
            &context.execution,
            &context.derived,
            if context.api_key.is_empty() {
                api_key
            } else {
                &context.api_key
            },
        )
    };
    CONTEXT
        .try_with(resolve)
        .unwrap_or_else(|_| resolve(&Context::default()))
}

/// Go session.Enrich derives from the original conversation root only when no
/// explicit identity is present. Caller scope is irreversible and key-isolated.
pub(crate) fn derived_session(
    source: &str,
    original: &Value,
    headers: &HeaderMap,
    api_key: &str,
) -> String {
    use sha2::{Digest, Sha256};
    let valid = |value: &str| {
        !value.trim().is_empty()
            && value.trim().len() <= 256
            && !value.chars().any(char::is_control)
    };
    if [
        "x-claude-code-session-id",
        "x-claude-code-agent-id",
        "x-claude-code-parent-agent-id",
        "session-id",
        "session_id",
        "x-codex-parent-thread-id",
        "x-codex-turn-metadata",
        "x-openai-subagent",
        "x-http-session-id",
        "x-session-id",
        "x-session-affinity",
        "x-parent-session-id",
        "x-parent-session-affinity",
        "x-parent-id",
        "x-slot-session-id",
        "x-parent-slot-session-id",
        "x-task-id",
        "x-parent-task-id",
        "x-conversation-id",
        "x-parent-conversation-id",
        "x-thread-id",
        "x-parent-thread-id",
        "thread-id",
        "x-client-request-id",
    ]
    .iter()
    .any(|name| {
        headers
            .get_all(*name)
            .iter()
            .any(|v| v.to_str().is_ok_and(valid))
    }) || [
        "session_id",
        "sessionId",
        "sessionID",
        "child_session_id",
        "childSessionId",
        "task_id",
        "taskId",
        "taskID",
        "action_id",
        "actionId",
        "cachedContent",
        "cached_content",
        "thread_id",
        "threadId",
        "conversation_id",
        "conversationId",
        "chat_id",
        "chatId",
        "prompt_cache_key",
        "promptCacheKey",
        "parent_session_id",
        "parentSessionId",
        "parent_thread_id",
        "parentThreadId",
        "parent_id",
        "parentId",
        "parentID",
        "parent_task_id",
        "parentTaskId",
        "parent_action_id",
        "parentActionId",
        "parent_session",
        "parentSession",
        "parent_subagent_id",
        "previousSessionId",
        "forked_from_thread_id",
        "forked_from_id",
    ]
    .iter()
    .any(|name| {
        original
            .get(*name)
            .and_then(Value::as_str)
            .is_some_and(valid)
    }) || [
        "/metadata/session_id",
        "/metadata/sessionId",
        "/metadata/task_id",
        "/metadata/taskId",
        "/metadata/thread_id",
        "/metadata/conversation_id",
        "/metadata/parent_id",
        "/metadata/parent_task_id",
        "/metadata/parent_agent_id",
        "/metadata/user_id",
        "/extra_body/session_id",
        "/extra_body/task_id",
        "/extra_body/parent_id",
        "/extra_body/parent_task_id",
        "/forkSource/sessionId",
        "/conversation/id",
        "/conversation",
    ]
    .iter()
    .any(|path| {
        original
            .pointer(path)
            .and_then(Value::as_str)
            .is_some_and(valid)
    }) {
        return String::new();
    }
    let mut instructions = Vec::new();
    let mut user = Vec::new();
    let mut instruction = |value: &Value| {
        let text = parts(value)
            .iter()
            .filter(|p| p.kind == "text")
            .map(|p| p.value.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            instructions.push(text.chars().take(50).collect::<String>());
        }
    };
    if matches!(source, "claude" | "openai-response")
        && let Some(value) = original.get(if source == "claude" {
            "system"
        } else {
            "instructions"
        })
    {
        instruction(value);
    }
    if source == "openai-response"
        && let Some(input) = original.get("input").filter(|v| v.is_string())
    {
        user = parts(input);
    } else if let Some(items) = original
        .get(if source == "openai-response" {
            "input"
        } else {
            "messages"
        })
        .and_then(Value::as_array)
    {
        for item in items {
            match item
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
                .as_str()
            {
                "system" | "developer" => instruction(&item["content"]),
                "user" => {
                    user = parts(&item["content"]);
                    if !user.is_empty() {
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    if user.is_empty() {
        return String::new();
    }
    #[derive(serde::Serialize)]
    struct Root<'a> {
        version: &'static str,
        format: &'a str,
        caller_scope: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        instructions: Vec<String>,
        user: Vec<Part>,
    }
    let caller_scope = if api_key.trim().is_empty() {
        String::new()
    } else {
        hex::encode(Sha256::digest(format!(
            "cli-proxy-api:caller-scope:v1\0{}",
            api_key.trim()
        )))
    };
    let root = Root {
        version: "cpa-session-root-v1",
        format: source,
        caller_scope,
        instructions,
        user,
    };
    // Struct field order matches Go canonicalRoot (map ordering is not equivalent).
    format!(
        "ctx:v1:{}",
        hex::encode(Sha256::digest(
            go_json(&serde_json::to_string(&root).expect("serializable session root")).as_bytes()
        ))
    )
}

#[derive(serde::Serialize)]
struct Part {
    kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    mime: String,
    value: String,
}

fn parts(value: &Value) -> Vec<Part> {
    let part = |kind: &str, mime: &str, value: &str| Part {
        kind: kind.into(),
        mime: mime.into(),
        value: value.into(),
    };
    match value {
        Value::Null => Vec::new(),
        Value::String(s) => {
            if s.is_empty() {
                Vec::new()
            } else {
                vec![part("text", "", s)]
            }
        }
        Value::Array(items) => items.iter().flat_map(parts).collect(),
        Value::Object(object) => {
            for name in ["text", "content", "parts"] {
                if let Some(nested) = object.get(name) {
                    return parts(nested);
                }
            }
            for (names, kind) in [
                (&["image_url"][..], "image"),
                (&["inlineData", "inline_data"][..], "inline_data"),
                (&["fileData", "file_data"][..], "file"),
                (&["source"][..], ""),
            ] {
                if let Some(media) = names.iter().find_map(|name| object.get(*name)) {
                    let kind = if kind.is_empty() {
                        object
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("media")
                            .trim()
                    } else {
                        kind
                    };
                    let mime = ["mimeType", "mime_type", "media_type"]
                        .iter()
                        .find_map(|name| {
                            media
                                .get(*name)
                                .and_then(Value::as_str)
                                .filter(|s| !s.trim().is_empty())
                        })
                        .unwrap_or_else(|| {
                            object
                                .get("media_type")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                        });
                    let value = media
                        .as_str()
                        .or_else(|| {
                            ["url", "uri", "fileUri", "file_uri", "data"]
                                .iter()
                                .find_map(|name| {
                                    media
                                        .get(*name)
                                        .and_then(Value::as_str)
                                        .filter(|s| !s.trim().is_empty())
                                })
                        })
                        .unwrap_or("");
                    return if value.is_empty() {
                        Vec::new()
                    } else {
                        vec![part(kind, mime, value)]
                    };
                }
            }
            let mut normalized = value.clone();
            strip_cache_control(&mut normalized);
            vec![part("json", "", &go_json(&normalized.to_string()))]
        }
        _ => vec![part("json", "", &go_json(&value.to_string()))],
    }
}

fn go_json(text: &str) -> String {
    text.replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn strip_cache_control(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.retain(|key, _| !key.trim().eq_ignore_ascii_case("cache_control"));
            object.values_mut().for_each(strip_cache_control);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_cache_control),
        _ => {}
    }
}

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
        "openai-response" => explicit.filter(|s| !s.is_empty()).map(str::to_owned),
        _ => None,
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
    fn http_cache_uses_original_source_trusted_scope_and_final_payload_barrier() {
        use crate::{config::Config, home::client::Selection, payload::ApplyContext, providers};
        let selection: Selection = serde_json::from_value(json!({"model":"m", "provider":"codex", "auth_index":"i", "user_api_key":"caller", "auth":{"attributes":{"api_key":"upstream"}, "metadata":{"execution_session_id":"untrusted", "derived_session_id":"untrusted"}}})).unwrap();
        for source in ["openai", "openai-response", "claude"] {
            for compact in [false, true] {
                for final_rule in ["", "override", "filter"] {
                    let payload = match final_rule {
                        "override" => {
                            json!({"override":[{"models":[{"name":"*"}],"params":{"prompt_cache_key":"final"}}]})
                        }
                        "filter" => {
                            json!({"filter":[{"models":[{"name":"*"}],"params":["prompt_cache_key"]}]})
                        }
                        _ => json!({}),
                    };
                    let config = Config::parse(&serde_json::to_vec(&json!({"credential-concurrency":{"lifecycle-config-revision":1}, "payload":payload})).unwrap()).unwrap();
                    let mut headers = HeaderMap::new();
                    headers.insert("x-claude-code-session-id", "claude-root".parse().unwrap());
                    let original = json!({"model":"alias", "prompt_cache_key":" caller-key "});
                    let body =
                        json!({"model":"m", "input":[], "prompt_cache_key":"translated-untrusted"});
                    let expected = key(
                        source,
                        &original,
                        &headers,
                        "m",
                        "execution",
                        "derived",
                        "caller",
                    )
                    .unwrap();
                    let prepared = with_context(
                        Context {
                            execution: "execution".into(),
                            derived: "derived".into(),
                            api_key: "caller".into(),
                        },
                        || {
                            providers::prepare_context(
                                &config,
                                &selection,
                                if compact {
                                    "/v1/responses/compact"
                                } else {
                                    "/v1/responses"
                                },
                                &headers,
                                body,
                                false,
                                Some(ApplyContext {
                                    from_protocol: source,
                                    original: Some(&original),
                                    ..Default::default()
                                }),
                            )
                        },
                    )
                    .unwrap();
                    let request = prepared.request.build().unwrap();
                    assert_eq!(request.headers()["session-id"], expected);
                    let body: Value =
                        serde_json::from_slice(request.body().unwrap().as_bytes().unwrap())
                            .unwrap();
                    match final_rule {
                        "override" => assert_eq!(body["prompt_cache_key"], "final"),
                        "filter" => assert!(body.get("prompt_cache_key").is_none()),
                        _ => assert_eq!(body["prompt_cache_key"], expected),
                    }
                }
            }
        }
        // Scope cannot leak out of the synchronous preparation boundary.
        let headers = HeaderMap::new();
        assert_eq!(
            http_key("openai-response", &json!({"input":[]}), &headers, "m", ""),
            None
        );
        let key_a = http_key("openai", &json!({}), &headers, "m", "a");
        assert_ne!(key_a, http_key("openai", &json!({}), &headers, "m", "b"));
        assert_eq!(key_a, key("openai", &json!({}), &headers, "m", "", "", "a"));
    }

    #[test]
    fn derived_http_identity_is_stable_caller_scoped_and_suppressed_by_explicit_sessions() {
        let original = json!({"messages":[{"role":"system","content":"policy"},{"role":"user","content":"first"}]});
        let headers = HeaderMap::new();
        let derived = derived_session("openai", &original, &headers, "caller");
        assert!(derived.starts_with("ctx:v1:"));
        let mut continued = original.clone();
        continued["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"user","content":"later"}));
        assert_eq!(
            derived,
            derived_session("openai", &continued, &headers, "caller")
        );
        assert_ne!(
            derived,
            derived_session("openai", &original, &headers, "other")
        );
        for name in ["session_id", "session-id", "x-conversation-id"] {
            let mut headers = headers.clone();
            headers.insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                "explicit".parse().unwrap(),
            );
            assert!(derived_session("openai", &original, &headers, "caller").is_empty());
        }
        for name in ["session_id", "conversation_id", "prompt_cache_key"] {
            let mut original = original.clone();
            original[name] = "explicit".into();
            assert!(derived_session("openai", &original, &headers, "caller").is_empty());
        }
        let expected = provider_session("execution", "");
        assert_eq!(
            with_context(
                Context {
                    execution: "execution".into(),
                    derived,
                    api_key: "caller".into()
                },
                || http_key("openai-response", &json!({}), &headers, "m", "")
            ),
            expected
        );
    }

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
