//! Native WS preparation: Go codex_websockets_request.go and stream.go.
use crate::{config::Config, home::client::Selection, payload::ApplyContext};
use anyhow::{Result, ensure};
use http::HeaderMap;
use serde_json::Value;

/// Attributes take precedence over metadata, including an explicit false.
pub fn enabled(auth: &Value) -> bool {
    fn boolean(value: &Value) -> Option<bool> {
        value.as_bool().or_else(|| match value.as_str()?.trim() {
            "true" | "TRUE" | "True" | "1" | "t" | "T" => Some(true),
            "false" | "FALSE" | "False" | "0" | "f" | "F" => Some(false),
            _ => None,
        })
    }
    auth.pointer("/attributes/websockets")
        .and_then(boolean)
        .or_else(|| auth.pointer("/metadata/websockets").and_then(boolean))
        .unwrap_or(false)
}

/// Reuse HTTP built-ins with an empty ruleset, then run the real rules exactly
/// once after restoring WS-only continuation state. Only framing follows this.
pub fn prepare(
    config: &Config,
    selection: &Selection,
    incoming: &HeaderMap,
    original: Value,
) -> Result<reqwest::Request> {
    let mut builtins = config.clone();
    builtins.payload = Default::default();
    let prepared = crate::providers::prepare(
        &builtins,
        selection,
        "/v1/responses",
        incoming,
        original.clone(),
        true,
    )?;
    let mut request = prepared.request.build()?;
    let bytes = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .ok_or_else(|| anyhow::anyhow!("request body unavailable"))?;
    let mut body: Value = serde_json::from_slice(bytes)?;
    for field in ["previous_response_id", "event_id"] {
        if let Some(value) = original.get(field) {
            body[field] = value.clone();
        }
    }
    body.as_object_mut()
        .expect("normalized object")
        .remove("stream");
    config.payload.apply_context(
        &mut body,
        ApplyContext {
            model: &selection.model,
            protocol: "codex",
            from_protocol: "openai-response",
            headers: Some(incoming),
            requested_model: original["model"].as_str().unwrap_or(&selection.model),
            original: Some(&original),
            root: "",
        },
    )?;
    ensure!(body.is_object(), "final WS payload must be an object");
    ensure!(
        body.get("type").is_none_or(|v| v == "response.create"),
        "conflicting websocket event type"
    );
    body["type"] = "response.create".into();
    *request.body_mut() = Some(serde_json::to_vec(&body)?.into());
    let headers = request.headers_mut();
    if let Some(key) = original
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        super::super::insert(headers, "session_id", key)?;
        super::super::insert(headers, "conversation_id", key)?;
    }
    for name in [
        "session_id",
        "conversation_id",
        "x-responsesapi-include-timing-metrics",
    ] {
        if let Some(value) = incoming.get(name).filter(|v| !v.as_bytes().is_empty()) {
            headers.insert(name, value.clone());
        }
    }
    super::super::custom_headers(headers, &selection.auth)?;
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn selected_auth_flag_precedence() {
        assert!(!enabled(&json!({})));
        assert!(enabled(&json!({"metadata":{"websockets":"true"}})));
        assert!(!enabled(
            &json!({"attributes":{"websockets":"false"},"metadata":{"websockets":true}})
        ));
    }
    #[test]
    fn continuation_is_present_before_final_rules() {
        let config = Config::parse(br#"{"credential-concurrency":{"lifecycle-config-revision":1},"payload":{"filter":[{"models":[{"name":"*","protocol":"codex"}],"params":["previous_response_id"]}],"override":[{"models":[{"name":"*","protocol":"codex"}],"params":{"stream":false,"store":true}}]}}"#).unwrap();
        let selection = serde_json::from_value(json!({"model":"m","provider":"codex","auth_index":"a","user_api_key":"u","auth":{"attributes":{"api_key":"test"}}})).unwrap();
        let request = prepare(
            &config,
            &selection,
            &HeaderMap::new(),
            json!({"model":"m","input":[],"previous_response_id":"r"}),
        )
        .unwrap();
        let body: Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert!(body.get("previous_response_id").is_none());
        assert_eq!(body["stream"], false);
        assert_eq!(body["store"], true);
    }
}
