//! Native Cloud Code envelope. Home must supply both token and prepared project ID.
use super::*;
use sha2::{Digest, Sha256};

#[path = "antigravity_diagnostic.rs"]
mod diagnostic;
pub use diagnostic::{ForbiddenDiagnostic, forbidden_diagnostic};
#[cfg(test)]
#[path = "antigravity_fidelity_tests.rs"]
mod fidelity_tests;

pub(super) const BASE: &str = "https://daily-cloudcode-pa.googleapis.com";
const USER_AGENT: &str =
    "antigravity/hub/2.29.0 (aidev_client; os_type=darwin; arch=arm64; cl=1013060670)";

pub(super) fn credentials(auth: &Value, protocol: &str, headers: &mut HeaderMap) -> Result<String> {
    ensure!(
        protocol == "gemini",
        "Antigravity requires native Gemini input"
    );
    bearer(headers, access_token(auth))?;
    ensure!(
        !text(auth, "/metadata/project_id").is_empty(),
        "Antigravity requires Home-prepared project_id; local project discovery disabled"
    );
    let ua = [
        text(auth, "/attributes/user_agent"),
        text(auth, "/metadata/user_agent"),
    ]
    .into_iter()
    .find(|v| !v.is_empty())
    .unwrap_or(USER_AGENT);
    insert(headers, "user-agent", request_user_agent(ua))?;
    Ok([
        text(auth, "/attributes/base_url"),
        text(auth, "/metadata/base_url"),
    ]
    .into_iter()
    .find(|v| !v.is_empty())
    .unwrap_or(BASE)
    .to_owned())
}

// Match misc.AntigravityRequestUserAgent: the Node client suffix belongs only
// to onboardUser, not generation, model listing, or loadCodeAssist requests.
fn request_user_agent(configured: &str) -> &str {
    let configured = configured.trim();
    if configured.is_empty() {
        return USER_AGENT;
    }
    let lower = configured.to_ascii_lowercase();
    if lower.starts_with("antigravity/")
        && let Some(index) = lower.find(" google-api-nodejs-client/")
    {
        return configured[..index].trim();
    }
    configured
}

pub(super) fn normalize(body: &mut Value, selection: &Selection) -> Result<()> {
    ensure!(!selection.model.is_empty(), "Antigravity model missing");
    // Image, schema conversion and reasoning replay need their actual implementations,
    // not a generic bearer request that merely looks like provider support.
    ensure!(
        !selection.model.contains("image"),
        "Antigravity image generation unsupported"
    );
    ensure!(
        body.get("contents").is_some_and(Value::is_array),
        "Antigravity requires native Gemini contents"
    );
    for key in ["request", "tools", "toolConfig", "cachedContent"] {
        ensure!(
            body.get(key).is_none(),
            "Antigravity tools, cache and prewrapped envelopes unsupported"
        );
    }
    for key in ["responseSchema", "responseJsonSchema", "responseMimeType"] {
        ensure!(
            body.pointer(&format!("/generationConfig/{key}")).is_none(),
            "Antigravity structured output conversion unsupported"
        );
    }
    for content in body["contents"].as_array().unwrap() {
        if let Some(parts) = content.get("parts").and_then(Value::as_array) {
            for part in parts {
                ensure!(
                    part.get("functionCall").is_none()
                        && part.get("functionResponse").is_none()
                        && part.get("thoughtSignature").is_none()
                        && part.get("thought").and_then(Value::as_bool) != Some(true),
                    "Antigravity tool/signature replay unsupported"
                );
            }
        }
    }
    let session = body
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| stable_session(body));
    let mut request = std::mem::take(body);
    let map = request.as_object_mut().unwrap();
    for key in ["model", "stream", "safetySettings", "session_id"] {
        map.remove(key);
    }
    request["sessionId"] = session.into();
    if selection.model.contains("claude") {
        if let (Some(maximum), Some(requested)) = (
            selection
                .model_info
                .as_ref()
                .and_then(|info| info.get("max_completion_tokens"))
                .and_then(Value::as_u64)
                .filter(|max| *max > 0),
            request
                .pointer("/generationConfig/maxOutputTokens")
                .and_then(Value::as_u64),
        ) && requested > maximum
        {
            request["generationConfig"]["maxOutputTokens"] = maximum.into();
        }
        request["toolConfig"] = serde_json::json!({"functionCallingConfig":{"mode":"VALIDATED"}});
    } else if let Some(config) = request
        .get_mut("generationConfig")
        .and_then(Value::as_object_mut)
    {
        config.remove("maxOutputTokens");
    }
    *body = serde_json::json!({
        "model":selection.model, "project":text(&selection.auth, "/metadata/project_id"),
        "userAgent":"antigravity", "requestType":"agent",
        "requestId":format!("agent-{}", uuid::Uuid::new_v4()), "request":request
    });
    Ok(())
}

pub(super) fn url(base: &str, stream: bool) -> Result<Url> {
    let mut url = Url::parse(base)?;
    let root = url.path().trim_end_matches('/');
    url.set_path(&format!(
        "{root}/v1internal:{}",
        if stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        }
    ));
    if stream {
        url.query_pairs_mut().append_pair("alt", "sse");
    }
    Ok(url)
}

/// Apply to each parsed upstream JSON response / SSE data object before Gemini
/// decoding, usage accounting or downstream serialization. SSE framing is gateway-owned.
/// Includes usage-only trailing events; missing usage is not fabricated as zero.
pub fn unwrap_response(envelope: &Value) -> Result<Value> {
    ensure!(
        envelope.is_object(),
        "invalid Antigravity response envelope"
    );
    ensure!(
        envelope.get("error").is_none(),
        "Antigravity returned an error envelope"
    );
    let mut response = match envelope.get("response") {
        Some(value) => {
            ensure!(value.is_object(), "invalid Antigravity response field");
            value.clone()
        }
        None => {
            ensure!(
                envelope.get("candidates").is_some() || usage_metadata(envelope).is_some(),
                "missing Antigravity response"
            );
            envelope.clone()
        }
    };
    ensure!(
        response.get("error").is_none(),
        "Antigravity returned a response error"
    );
    if response.get("usageMetadata").is_none()
        && let Some(usage) = usage_metadata(envelope)
    {
        response["usageMetadata"] = usage.clone();
    }
    Ok(response)
}

/// Raw provider usage, preserving cached/reasoning counts for the existing
/// Gemini token-accounting parser. This is a snapshot, not an additive delta.
pub fn usage_metadata(envelope: &Value) -> Option<&Value> {
    [
        "/usageMetadata",
        "/usage_metadata",
        "/response/usageMetadata",
        "/response/usage_metadata",
    ]
    .into_iter()
    .filter_map(|path| envelope.pointer(path))
    .find(|value| value.is_object())
}

fn stable_session(body: &Value) -> String {
    if let Some(contents) = body.get("contents").and_then(Value::as_array) {
        for content in contents {
            if content.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            if let Some(text) = content
                .pointer("/parts/0/text")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let hash = Sha256::digest(text.as_bytes());
                let number =
                    u64::from_be_bytes(hash[..8].try_into().unwrap()) & 0x7fff_ffff_ffff_ffff;
                return format!("-{number}");
            }
        }
    }
    let bytes = uuid::Uuid::new_v4();
    let number =
        u64::from_be_bytes(bytes.as_bytes()[..8].try_into().unwrap()) % 9_000_000_000_000_000_000;
    format!("-{number}")
}
