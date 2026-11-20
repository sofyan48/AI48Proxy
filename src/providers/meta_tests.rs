use super::*;
use crate::providers::xai::tests::{body, config, mock, selection};
use serde_json::json;

#[test]
fn credential_precedence_dca_rejection_and_identity() {
    let selected = selection(
        "meta",
        json!({"metadata":{"api_key":"minted","base_url":"https://api.meta.ai/v1","dca_token":"dca:fixture"}}),
    );
    let creds = crate::providers::resolve(&selected, "openai-response").unwrap();
    assert_eq!(creds.headers["authorization"], "Bearer minted");
    assert_eq!(creds.headers["x-client-id"], "tbh:exec");
    let session = creds.headers["x-tbh-session-id"].to_str().unwrap();
    assert_eq!(uuid::Uuid::parse_str(session).unwrap().get_version_num(), 7);
    for name in [
        "x-meta-ai-gateway-session-id",
        "x-tbh-thread-id",
        "x-tbh-root-session-id",
    ] {
        assert_eq!(creds.headers[name], session);
    }
    assert!(
        creds.headers["user-agent"]
            .to_str()
            .unwrap()
            .starts_with("muse-build/1.4.14 (non-interactive;")
    );
    let selected = selection("meta", json!({"metadata":{"access_token":"dca:fixture"}}));
    assert!(crate::providers::resolve(&selected, "openai-response").is_err());
}

#[test]
fn dca_refresh_request_uses_dca_hash_without_exposing_token() {
    let selected = selection(
        "meta",
        json!({"metadata":{"dca_token":"dca:fixture"},"attributes":{}}),
    );
    let request = crate::providers::refresh::refresh_request(&selected).unwrap();
    assert_eq!(request["type"], "refresh");
    assert!(request.get("dca_token").is_none());
    assert!(request.get("access_token").is_none());
    assert!(
        !request["access_token_sha256"]
            .as_str()
            .unwrap()
            .contains("dca")
    );
}

#[test]
fn retention_cleanup_contributor_and_replay_rejection() {
    let mut selected = selection("meta", json!({"metadata":{"api_key":"minted"}}));
    selected.model = "muse-wonder-1.0-contributor".into();
    let prepared = crate::providers::prepare(
        &config(),
        &selected,
        "/v1/responses",
        &HeaderMap::new(),
        json!({"input":"hello","prompt_cache_retention":" 24h ","client_metadata":{"foo":true}}),
        false,
    )
    .unwrap();
    let request = prepared.request.build().unwrap();
    assert_eq!(request.headers()["x-tbh-contributor-mode"], "1");
    assert_eq!(body(&request)["prompt_cache_retention"], "24h");
    assert!(body(&request).get("client_metadata").is_none());
    assert!(
        crate::providers::prepare(
            &config(),
            &selected,
            "/v1/responses",
            &HeaderMap::new(),
            json!({"input":[],"tools":[{"type":"custom","name":"tool"}]}),
            false
        )
        .is_err()
    );
}

#[tokio::test]
async fn responses_mock() {
    let received = mock(
        "meta",
        "/v1/responses",
        json!({"input":"hello","prompt_cache_retention":null}),
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[]}}\n\n",
        true,
    )
    .await;
    assert_eq!(received["stream"], true);
    assert!(received.get("prompt_cache_retention").is_none());
}
