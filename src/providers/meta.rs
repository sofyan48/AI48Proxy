//! Meta Muse native Responses text executor. Subscription tool attestation is unsupported.
use super::{Credentials, Prepared, bearer, finish_credentials, insert, text, xai};
use crate::{config::Config, home::client::Selection, payload::ApplyContext};
use anyhow::{Result, ensure};
use http::HeaderMap;
use serde_json::Value;

pub(super) fn token(auth: &Value) -> &str {
    [
        "/attributes/api_key",
        "/attributes/access_token",
        "/metadata/api_key",
        "/metadata/access_token",
    ]
    .into_iter()
    .map(|p| text(auth, p))
    .find(|s| !s.is_empty() && !s.starts_with("dca:"))
    .unwrap_or("")
}

pub(super) fn credentials(selection: &Selection, protocol: &str) -> Result<Credentials> {
    ensure!(
        protocol == "openai-response",
        "Meta Chat translation/response bridge unsupported; native Responses input required"
    );
    let auth = &selection.auth;
    let api_key = token(auth);
    ensure!(
        !api_key.is_empty(),
        "Meta credential requires Home refresh to mint an API key from DCA"
    );
    let base = [
        "/attributes/base_url",
        "/metadata/base_url",
        "/metadata/api_base_url",
    ]
    .into_iter()
    .map(|p| text(auth, p))
    .find(|s| !s.is_empty())
    .unwrap_or("https://api.meta.ai/v1");
    let mut headers = HeaderMap::new();
    bearer(&mut headers, api_key)?;
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    insert(
        &mut headers,
        "user-agent",
        &format!(
            "muse-build/1.4.14 (non-interactive; {os}-{arch}; build dec7d7497fecdad135ec5835a8161dbc92e55b58)"
        ),
    )?;
    insert(&mut headers, "x-client-id", "tbh:exec")?;
    // UUIDv7 is a millisecond timestamp plus random bits. uuid's v7 feature is
    // not enabled in this workspace, so reuse v4's cryptographic random bytes.
    let mut bytes = *uuid::Uuid::new_v4().as_bytes();
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    bytes[..6].copy_from_slice(&(millis as u64).to_be_bytes()[2..]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    let session = uuid::Uuid::from_bytes(bytes).to_string();
    for name in [
        "x-tbh-session-id",
        "x-meta-ai-gateway-session-id",
        "x-tbh-thread-id",
        "x-tbh-root-session-id",
    ] {
        insert(&mut headers, name, &session)?;
    }
    insert(&mut headers, "x-tbh-agent-role", "main")?;
    insert(&mut headers, "cache-control", "no-cache")?;
    finish_credentials(
        selection,
        headers,
        base.into(),
        "meta",
        !text(auth, "/metadata/dca_token").is_empty(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare(
    config: &Config,
    selection: &Selection,
    path: &str,
    incoming: &HeaderMap,
    mut body: Value,
    requested_stream: bool,
    context: Option<ApplyContext<'_>>,
    mut creds: Credentials,
) -> Result<Prepared> {
    ensure!(
        path == "/v1/responses",
        "Meta compact/Chat route unsupported"
    );
    let original = body.clone();
    xai::validate_responses(&body)?;
    xai::normalize_tools(&mut body)?;
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut)
        && !tools.is_empty()
    {
        tools.push(muse_namespace());
    }
    let object = body
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Meta request must be an object"))?;
    object.insert("model".into(), selection.model.clone().into());
    object.insert("stream".into(), true.into());
    for key in [
        "generate",
        "safety_identifier",
        "stream_options",
        "client_metadata",
    ] {
        object.remove(key);
    }
    let retention = object
        .get("prompt_cache_retention")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if let Some(retention) = retention {
        object.insert("prompt_cache_retention".into(), retention.into());
    } else {
        object.remove("prompt_cache_retention");
    }
    if !object.contains_key("instructions") {
        object.insert("instructions".into(), "".into());
    }
    if selection
        .model
        .split('(')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .ends_with("-contributor")
    {
        insert(&mut creds.headers, "x-tbh-contributor-mode", "1")?;
    }
    xai::send(
        config,
        selection,
        path,
        incoming,
        body,
        &original,
        requested_stream,
        context,
        creds,
        "meta",
        true,
    )
}

// Exact attestation namespace from Go helps/meta_muse_tools.go.
pub(super) fn muse_namespace() -> Value {
    serde_json::json!({"type":"namespace","name":"muse","description":"Muse Code tool set.","tools":[{"type":"function","name":"submit_reminder_decision","description":"Submit this reminder child's explicit decision.","parameters":{"additionalProperties":false,"properties":{"advisory_text":{"description":"Short text to deliver to the main agent when reminding.","type":["null","string"]},"confidence":{"description":"Confidence in the decision.","enum":["low","medium","high"],"type":["null","string"]},"decision":{"enum":["remind","none"],"type":"string"},"priority":{"description":"Requested reminder priority.","enum":["low","normal","high"],"type":["null","string"]},"reason":{"description":"Brief reason for reminding or staying silent.","type":"string"},"skill_id":{"description":"The listed skill id to remind when needs_reminder is true.","type":["null","string"]},"visible_for_steps":{"description":"Requested ephemeral visibility length in main model steps.","maximum":8,"minimum":1,"type":["null","integer"]}},"required":["advisory_text","confidence","decision","priority","reason","skill_id","visible_for_steps"],"type":"object"},"strict":true}]})
}

#[cfg(test)]
#[path = "meta_tests.rs"]
mod tests;
