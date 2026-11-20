use super::*;
use crate::providers::xai::tests::{body, config, mock, selection};

#[test]
fn models_credentials_and_temperature() {
    for (input, expected) in [
        ("KIMI-K2.8[1m]", "kimi-for-coding"),
        ("kimi-k3[1m]", "k3"),
        ("kimi-k2.7-code-highspeed", "kimi-for-coding-highspeed"),
    ] {
        assert_eq!(normalize_model(input), expected);
    }
    let mut selected = selection(
        "kimi",
        json!({"metadata":{"access_token":"oauth","device_id":"dispatched-device","domain":"kimi.ai"},"attributes":{"api_key":"ignored"}}),
    );
    selected.model = "kimi-k3[1m]".into();
    let creds = crate::providers::resolve(&selected, "openai").unwrap();
    assert_eq!(creds.base_url, "https://api.kimi.ai/coding/v1");
    assert_eq!(creds.headers["authorization"], "Bearer oauth");
    assert_eq!(creds.headers["x-msh-device-id"], "dispatched-device");
    assert_eq!(creds.headers["user-agent"], "CLIProxyAPI/dev");
    let prepared = crate::providers::prepare(&config(), &selected, "/v1/chat/completions", &HeaderMap::new(), json!({"messages":[{"role":"assistant","content":" "},{"role":"user","content":"hi"}],"temperature":0.6}), false).unwrap();
    let request = prepared.request.build().unwrap();
    let body = body(&request);
    assert_eq!(body["model"], "k3");
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    assert!(body.get("temperature").is_none());
    assert!(crate::providers::resolve(&selected, "claude").is_err());
    assert!(
        crate::providers::prepare(
            &config(),
            &selected,
            "/v1/chat/completions",
            &HeaderMap::new(),
            json!({"messages":[{"role":"tool","content":"result"}]}),
            false
        )
        .is_err()
    );
}

#[test]
fn attribute_access_token_can_be_refreshed_via_home() {
    let selected = selection("kimi", json!({"attributes":{"access_token":"fixture"}}));
    let request = crate::providers::refresh::refresh_request(&selected).unwrap();
    use sha2::{Digest, Sha256};
    assert_eq!(
        request["access_token_sha256"],
        hex::encode(Sha256::digest(b"fixture"))
    );
    assert!(request.get("access_token").is_none());
}

#[tokio::test]
async fn chat_json_and_responses_sse_mock() {
    let received = mock("kimi", "/v1/chat/completions", json!({"messages":[{"role":"user","content":"hello"}],"thinking":{"type":"disabled"},"temperature":0.6}), "{\"choices\":[{\"message\":{\"content\":\"hello\"}}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}", false).await;
    assert_eq!(received["temperature"], 0.6);
    let received = mock(
        "kimi",
        "/v1/responses",
        json!({"input":"hello"}),
        "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n",
        true,
    )
    .await;
    assert_eq!(received["stream"], true);
}
