use super::*;
use serde_json::json;

pub(crate) fn selection(provider: &str, auth: Value) -> Selection {
    serde_json::from_value(
        json!({"provider":provider,"model":"text-model","auth_index":"selected","auth":auth}),
    )
    .unwrap()
}

pub(crate) fn config() -> Config {
    Config::parse(b"credential-concurrency:\n  lifecycle-config-revision: 7\n").unwrap()
}

pub(crate) fn body(request: &reqwest::Request) -> Value {
    serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap()
}

/// Exercise the actual reqwest transport without contacting a provider.
pub(crate) async fn mock(
    provider: &str,
    path: &'static str,
    payload: Value,
    reply: &'static str,
    sse: bool,
) -> Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/tenant/v1", listener.local_addr().unwrap());
    let selected = selection(
        provider,
        json!({"metadata":{"access_token":"fixture-token"},"attributes":{"base_url":base}}),
    );
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let (offset, length) = loop {
            let mut buffer = [0; 4096];
            let n = socket.read(&mut buffer).await.unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&buffer[..n]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = std::str::from_utf8(&bytes[..end])
                    .unwrap()
                    .to_ascii_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:").map(str::trim))
                    .unwrap()
                    .parse()
                    .unwrap();
                if bytes.len() >= end + 4 + length {
                    break (end + 4, length);
                }
            }
        };
        let head = std::str::from_utf8(&bytes[..offset])
            .unwrap()
            .to_ascii_lowercase();
        assert!(head.contains("authorization: bearer fixture-token"));
        let suffix = if path == "/v1/responses" {
            "responses"
        } else {
            "chat/completions"
        };
        assert!(head.starts_with(&format!("post /tenant/v1/{suffix} http/1.1")));
        let received: Value = serde_json::from_slice(&bytes[offset..offset + length]).unwrap();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            if sse {
                "text/event-stream"
            } else {
                "application/json"
            },
            reply.len(),
            reply
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        received
    });
    let prepared =
        crate::providers::prepare(&config(), &selected, path, &HeaderMap::new(), payload, sse)
            .unwrap();
    let response = prepared.request.send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), reply);
    server.await.unwrap()
}

#[test]
fn oauth_base_identity_and_fail_closed() {
    let selected = selection(
        "xai",
        json!({"metadata":{"access_token":"fixture","auth_kind":"oauth","base_url":API}}),
    );
    let creds = crate::providers::resolve(&selected, "openai-response").unwrap();
    assert_eq!(creds.base_url, CLI);
    assert_eq!(creds.headers["x-xai-token-auth"], "xai-grok-cli");
    assert_eq!(creds.headers["x-grok-client-version"], CLIENT_VERSION);
    assert!(creds.headers["authorization"].is_sensitive());
    assert!(crate::providers::resolve(&selected, "openai").is_err());
    for input in [
        json!({"input":[],"tools":[{"type":"namespace"}]}),
        json!({"input":[{"type":"reasoning","encrypted_content":"fixture"}]}),
        json!({"input":[],"previous_response_id":"r"}),
    ] {
        assert!(
            crate::providers::prepare(
                &config(),
                &selected,
                "/v1/responses",
                &HeaderMap::new(),
                input,
                false
            )
            .is_err()
        );
    }
}

#[test]
fn inject_x_search_follows_home_config() {
    let selected = selection("xai", json!({"attributes":{"api_key":"fixture"}}));
    let config = Config::parse(
        b"credential-concurrency:\n  lifecycle-config-revision: 7\nxai:\n  inject-x-search: true\n",
    )
    .unwrap();
    assert!(config.unsupported_features().is_empty());
    let input = json!({"input":"hi","tools":[{"type":"function","name":"f","parameters":{"type":"object"}}],"tool_choice":"auto"});
    let prepared = crate::providers::prepare(
        &config,
        &selected,
        "/v1/responses",
        &HeaderMap::new(),
        input,
        true,
    )
    .unwrap();
    let sent = body(&prepared.request.build().unwrap());
    assert_eq!(sent["tools"][1], json!({"type":"x_search"}));
    let mut allowed = json!({"tool_choice":{"type":"allowed_tools","tools":[]}});
    super::inject_x_search(&mut allowed);
    assert_eq!(
        allowed["tool_choice"]["tools"],
        json!([{"type":"x_search"}])
    );
    // Idempotent and absent when disabled.
    let mut again = sent.clone();
    super::inject_x_search(&mut again);
    assert_eq!(again, sent);
    let prepared = crate::providers::prepare(
        &self::config(),
        &selected,
        "/v1/responses",
        &HeaderMap::new(),
        json!({"input":"hi"}),
        true,
    )
    .unwrap();
    assert!(
        body(&prepared.request.build().unwrap())
            .get("tools")
            .is_none()
    );
}

#[test]
fn payload_rules_are_last_and_cannot_enable_unported_tools() {
    let selected = selection("xai", json!({"attributes":{"api_key":"fixture"}}));
    let config = Config::parse(b"credential-concurrency:\n  lifecycle-config-revision: 7\npayload:\n  override:\n    - models:\n        - name: '*'\n          protocol: openai-response\n      params:\n        model: configured\n        instructions: configured\n").unwrap();
    let prepared = crate::providers::prepare(
        &config,
        &selected,
        "/v1/responses",
        &HeaderMap::new(),
        json!({"input":"hello"}),
        false,
    )
    .unwrap();
    let request = prepared.request.build().unwrap();
    assert_eq!(body(&request)["model"], "configured");
    assert_eq!(body(&request)["instructions"], "configured");
    assert_eq!(prepared.response_mode, ResponseMode::CodexSse);
    let config = Config::parse(b"credential-concurrency:\n  lifecycle-config-revision: 7\npayload:\n  override:\n    - models:\n        - name: '*'\n          protocol: openai-response\n      params:\n        stream: false\n").unwrap();
    assert!(
        crate::providers::prepare(
            &config,
            &selected,
            "/v1/responses",
            &HeaderMap::new(),
            json!({"input":"hello"}),
            false
        )
        .is_err()
    );
}

#[test]
fn function_tools_normalize_before_final_payload_and_advanced_tools_fail_closed() {
    for provider in ["xai", "meta"] {
        let selected = selection(provider, json!({"attributes":{"api_key":"fixture"}}));
        let protocol = if provider == "meta" {
            "meta"
        } else {
            "openai-response"
        };
        let rules = format!(
            "credential-concurrency:\n  lifecycle-config-revision: 7\npayload:\n  filter:\n    - models:\n        - name: '*'\n          protocol: {protocol}\n      params: [tools]\n"
        );
        let config = Config::parse(rules.as_bytes()).unwrap();
        let prepared = crate::providers::prepare(
            &config,
            &selected,
            "/v1/responses",
            &HeaderMap::new(),
            json!({"input":"hello","tools":[{"type":"function","name":"lookup"}]}),
            false,
        )
        .unwrap();
        assert!(
            body(&prepared.request.build().unwrap())
                .get("tools")
                .is_none()
        );
        for tool in [
            json!({"type":"function","name":"lookup","parameters":{"$ref":"#/$defs/A"}}),
            json!({"type":"custom","name":"lookup"}),
            json!({"type":"namespace","name":"client","tools":[]}),
        ] {
            assert!(
                crate::providers::prepare(
                    &super::tests::config(),
                    &selected,
                    "/v1/responses",
                    &HeaderMap::new(),
                    json!({"input":"hello","tools":[tool]}),
                    false
                )
                .is_err()
            );
        }
    }
}

#[tokio::test]
async fn native_responses_mock_stream() {
    let received = mock("xai", "/v1/responses", json!({"input":"hello","stop":"end","safety_identifier":"id"}), "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[],\"usage\":{\"input_tokens\":2,\"output_tokens\":3}}}\n\n", true).await;
    assert_eq!(received["stream"], true);
    assert!(received.get("stop").is_none());
    assert!(received.get("safety_identifier").is_none());
}
