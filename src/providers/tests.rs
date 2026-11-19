use super::*;
use serde_json::json;

fn selection(provider: &str, auth: Value) -> Selection {
    serde_json::from_value(json!({"model":"test-model", "provider":provider,
        "auth_index":"selected", "auth":auth}))
    .unwrap()
}

#[test]
fn codex_oauth_fixture() {
    let selected = selection(
        "codex",
        json!({"metadata":{"access_token":"fixture-token", "account_id":"account"}}),
    );
    let creds = resolve(&selected, "openai-response").unwrap();
    assert_eq!(creds.base_url, "https://chatgpt.com/backend-api/codex");
    assert_eq!(creds.headers["authorization"], "Bearer fixture-token");
    assert!(creds.headers["authorization"].is_sensitive());
    assert_eq!(creds.headers["originator"], "codex-tui");
    assert_eq!(creds.headers["chatgpt-account-id"], "account");
    assert!(resolve(&selected, "openai").is_err());
}

#[test]
fn codex_normalization_fixture() {
    let mut body = json!({"input":"hello", "stream":false, "max_output_tokens":2,
        "stream_options":{"include_usage":true}, "parallel_tool_calls":true,
        "instructions":null, "store":true});
    codex::normalize_request(&mut body, "gpt-fixture").unwrap();
    assert_eq!(
        body,
        json!({"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}], "stream":true, "store":false,
        "include":["reasoning.encrypted_content"], "max_output_tokens":16, "instructions":"", "model":"gpt-fixture"})
    );
    for mut invalid in [
        json!({"messages":[]}),
        json!({"input":[],"previous_response_id":"resp_x"}),
        json!({"input":[], "tools":["invalid"]}),
    ] {
        assert!(codex::normalize_request(&mut invalid, "model").is_err());
    }
}

#[test]
fn codex_terminal_fixture() {
    assert_eq!(
        codex::completed_response(&json!({"type":"response.completed",
        "response":{"id":"r", "output":[]}}))
        .unwrap(),
        Some(json!({"id":"r","output":[]}))
    );
    assert!(codex::completed_response(&json!({"type":"response.failed"})).is_err());
    assert!(codex::completed_response(&json!({"type":"response.completed"})).is_err());
    assert!(
        codex::completed_response(&json!({"type":"response.created"}))
            .unwrap()
            .is_none()
    );
}

#[test]
fn claude_selected_identity_fixture() {
    let auth = json!({"metadata":{"access_token":"sk-ant-oat-fixture",
        "account_uuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "claude_device_ids":["bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]}});
    let selected = selection("claude", auth.clone());
    let mut creds = resolve(&selected, "claude").unwrap();
    assert_eq!(creds.headers["authorization"], "Bearer sk-ant-oat-fixture");
    assert!(!creds.headers.contains_key("x-api-key"));
    assert_eq!(
        creds.headers["user-agent"],
        "claude-cli/2.1.321 (external, cli)"
    );
    let mut body = json!({"messages":[{"role":"user","content":"Hi"}],
        "metadata":{"user_id":"caller-identity","other":"keep"}});
    claude::prepare_body(
        &mut body,
        &mut creds.headers,
        &auth,
        &HeaderMap::new(),
        true,
    )
    .unwrap();
    let identity: Value =
        serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(identity["account_uuid"], auth["metadata"]["account_uuid"]);
    assert_eq!(
        identity["device_id"],
        auth["metadata"]["claude_device_ids"][0]
    );
    assert_eq!(body["metadata"]["other"], "keep");
    assert!(
        creds.headers["anthropic-beta"]
            .to_str()
            .unwrap()
            .contains("oauth-2025-04-20")
    );
    assert!(
        claude::prepare_body(
            &mut body,
            &mut creds.headers,
            &json!({}),
            &HeaderMap::new(),
            true
        )
        .is_err()
    );
}

#[test]
fn key_and_bearer_fixtures() {
    let selected = selection(
        "openai",
        json!({"attributes":{"api_key":"nested","base_url":"https://compat.example/v1"},"proxy_url":"direct"}),
    );
    let creds = resolve(&selected, "openai").unwrap();
    assert_eq!(creds.headers["authorization"], "Bearer nested");
    assert_eq!(creds.proxy_url.as_deref(), Some("direct"));
    let selected = selection("claude", json!({"attributes":{"api_key":"key"}}));
    assert_eq!(
        resolve(&selected, "claude").unwrap().headers["x-api-key"],
        "key"
    );
    let selected = selection(
        "claude",
        json!({"attributes":{"api_key":"key", "base_url":"https://gateway.example"}}),
    );
    assert_eq!(
        resolve(&selected, "claude").unwrap().headers["authorization"],
        "Bearer key"
    );
}

#[test]
fn unsupported_and_injection_fail_closed() {
    for provider in ["gemini-cli", "antigravity", "vertex", "qwen", "unknown"] {
        assert!(
            resolve(
                &selection(provider, json!({"metadata":{"access_token":"token"}})),
                "gemini"
            )
            .is_err()
        );
    }
    for auth in [
        json!({"disabled":true}),
        json!({"attributes":{"api_key":"bad\r\nvalue"}}),
        json!({"attributes":{"api_key":"key","base_url":"https://user:secret@example.com"}}),
        json!({"attributes":{"api_key":"key","header:authorization":"Bearer attacker"}}),
    ] {
        assert!(resolve(&selection("openai", auth), "openai").is_err());
    }
}

#[test]
fn prepared_request_fixture() {
    let config =
        Config::parse(b"credential-concurrency: {lifecycle-config-revision: 7}\n").unwrap();
    let selected = selection("codex", json!({"metadata":{"access_token":"token"}}));
    let prepared = prepare(
        &config,
        &selected,
        "/v1/responses",
        &HeaderMap::new(),
        json!({"input":"hello"}),
        false,
    )
    .unwrap();
    assert!(prepared.stream);
    assert_eq!(prepared.response_mode, ResponseMode::CodexSse);
    let request = prepared.request.build().unwrap();
    assert_eq!(
        request.url().as_str(),
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(request.headers()["accept"], "text/event-stream");
    let body: Value = serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(body["stream"], true);
    assert_eq!(body["model"], "test-model");
}

#[test]
fn refresh_fixtures() {
    use sha2::{Digest, Sha256};
    let selected = selection("codex", json!({"metadata":{"accessToken":" token "}}));
    let request = refresh::refresh_request(&selected).unwrap();
    assert_eq!(request["type"], "refresh");
    assert_eq!(
        request["access_token_sha256"],
        hex::encode(Sha256::digest(b"token"))
    );
    assert!(request.get("access_token").is_none());
    let updated = refresh::parse_refresh(
        json!({"auth_index":"new-index",
        "auth":{"metadata":{"access_token":"rotated"}}}),
        "old-index",
    )
    .unwrap();
    assert_eq!(updated.auth_index, "new-index");
    for invalid in [
        json!({"error":{"message":"secret"}}),
        json!({}),
        json!({"metadata":{"access_token":"new"},"status":"disabled"}),
    ] {
        assert!(refresh::parse_refresh(invalid, "old").is_err());
    }
}
