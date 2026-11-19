//! Offline HTTP/provider fixtures, not live-provider or signature parity tests.
use super::{
    tests::{config, selection},
    *,
};

fn selected(provider: &str) -> crate::home::client::Selection {
    let mut selected = selection(provider);
    selected.auth["metadata"] = json!({"access_token":"home-token","project_id":"home-project"});
    selected
}

#[test]
fn native_google_and_vertex_openapi_routing_keep_payload_last() {
    for provider in ["vertex", "vertexai", "antigravity"] {
        let prepared = upstream::prepare(&config("payload:\n  override:\n    - models: [{name: resolved-model}]\n      params: {generationConfig.maxOutputTokens: 123}\n  filter:\n    - models: [{name: resolved-model}]\n      params: [sessionId]\n"), &selected(provider), "/v1beta/models/original-alias:generateContent", &HeaderMap::new(), json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}]}), false).unwrap();
        assert_eq!(prepared.response_model, "original-alias");
        assert_eq!(prepared.format, upstream::Format::Gemini);
        let request = prepared.request.build().unwrap();
        let body: Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        let native = if provider == "antigravity" {
            assert_eq!(prepared.response_kind, upstream::ResponseKind::Antigravity);
            assert_eq!(request.url().path(), "/v1/v1internal:generateContent");
            assert_eq!(body["project"], "home-project");
            &body["request"]
        } else {
            assert!(
                request
                    .url()
                    .path()
                    .ends_with("/publishers/google/models/resolved-model:generateContent")
            );
            &body
        };
        assert_eq!(native["generationConfig"]["maxOutputTokens"], 123);
        assert!(native.get("sessionId").is_none());
    }
    let mut selected = selected("vertex");
    selected.auth["attributes"]["vertex_model_openapi"] =
        json!([{"name":"resolved-model","alias":"chat-alias","openapi":true}])
            .to_string()
            .into();
    let prepared = upstream::prepare(
        &config(""),
        &selected,
        "/v1/chat/completions",
        &HeaderMap::new(),
        json!({"model":"chat-alias","messages":[]}),
        false,
    )
    .unwrap();
    let request = prepared.request.build().unwrap();
    assert!(
        request
            .url()
            .path()
            .ends_with("/endpoints/openapi/chat/completions")
    );
    let body: Value = serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(body["model"], "google/resolved-model");
    assert!(
        upstream::prepare(
            &config(""),
            &selected,
            "/v1/chat/completions",
            &HeaderMap::new(),
            json!({"model":"other-alias","messages":[]}),
            false
        )
        .is_err()
    );
}

#[test]
fn unsupported_google_translation_and_replay_fail_closed() {
    for provider in ["vertex", "antigravity"] {
        assert!(
            upstream::prepare(
                &config(""),
                &selected(provider),
                "/v1/chat/completions",
                &HeaderMap::new(),
                json!({"model":"alias","messages":[]}),
                false
            )
            .is_err()
        );
    }
    for extra in [
        json!({"tools":[]}),
        json!({"contents":[{"parts":[{"thoughtSignature":"not-supported"}]}]}),
    ] {
        let mut body = json!({"contents":[]});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(
            upstream::prepare(
                &config(""),
                &selected("antigravity"),
                "/v1beta/models/alias:generateContent",
                &HeaderMap::new(),
                body,
                false
            )
            .is_err()
        );
    }
}

#[test]
fn antigravity_split_coalesced_multiline_events_and_snapshot_usage() {
    let wire = b": ping\r\n\r\ndata: {\"response\":\r\ndata: {\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"text\":\"hello\"}]}}]}}\r\n\r\ndata: {\"usageMetadata\":{\"promptTokenCount\":10,\"cachedContentTokenCount\":2,\"candidatesTokenCount\":3,\"thoughtsTokenCount\":1,\"totalTokenCount\":14}}\n\ndata: {\"response\":{},\"usageMetadata\":{\"promptTokenCount\":10,\"cachedContentTokenCount\":2,\"candidatesTokenCount\":3,\"thoughtsTokenCount\":1,\"totalTokenCount\":14}}\n\n";
    for size in [1, 7, wire.len()] {
        let mut events = antigravity::Events::default();
        let mut usage = usage::Usage::new(upstream::Format::Gemini);
        let mut output = Vec::new();
        for chunk in wire.chunks(size) {
            output.extend(events.feed(chunk, &mut usage).unwrap());
        }
        assert!(events.complete());
        let text = String::from_utf8(output).unwrap();
        assert!(!text.contains("\"response\""));
        assert!(text.contains("hello"));
        assert_eq!(text.matches("usageMetadata").count(), 2);
        let breakdown = usage.breakdown();
        assert_eq!(breakdown.total_tokens, 14);
        assert_eq!(breakdown.input.cache_read_tokens, 2);
        assert_eq!(breakdown.output.reasoning_tokens, 1);
    }
}

#[test]
fn antigravity_invalid_truncated_empty_and_oversized_fail() {
    for wire in [
        b"data: {\"error\":{\"message\":\"secret\"}}\n\n".as_slice(),
        b"data: {}\n\n",
        b"data: [DONE]\n\n",
        b"data: nope\n\n",
    ] {
        let mut events = antigravity::Events::default();
        assert!(
            events
                .feed(wire, &mut usage::Usage::new(upstream::Format::Gemini))
                .is_err()
        );
    }
    let mut events = antigravity::Events::default();
    let mut usage = usage::Usage::new(upstream::Format::Gemini);
    events
        .feed(b"data: {\"response\":{\"candidates\":[]}}\n", &mut usage)
        .unwrap();
    assert!(!events.complete());
    assert!(
        events
            .feed(&vec![b'x'; 1024 * 1024 + 1], &mut usage)
            .is_err()
    );
    let value = response::decode_json(
        json!({"response":{"candidates":[]},"usageMetadata":{"promptTokenCount":9}}),
        upstream::ResponseKind::Antigravity,
    )
    .unwrap();
    assert!(value.get("response").is_none());
    assert_eq!(value["usageMetadata"]["promptTokenCount"], 9);
}

#[tokio::test]
async fn antigravity_http_sse_request_and_envelope_fixture() {
    use futures_util::StreamExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/v1internal:streamGenerateContent", post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["authorization"], "Bearer home-token");
            assert_eq!(body["model"], "resolved-model");
            assert_eq!(body["project"], "home-project");
            assert!(body["request"]["contents"].is_array());
            ([("content-type", "text/event-stream")], "data: {\"response\":{\"candidates\":[{\"finishReason\":\"STOP\"}]}}\n\ndata: {\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2,\"totalTokenCount\":7}}\n\n")
        }))).await.unwrap();
    });
    let mut selected = selected("antigravity");
    selected.auth["attributes"]
        .as_object_mut()
        .unwrap()
        .remove("api_key");
    selected.auth["attributes"]["base_url"] = format!("http://{address}").into();
    let prepared = upstream::prepare(
        &config(""),
        &selected,
        "/v1beta/models/alias:streamGenerateContent",
        &HeaderMap::new(),
        json!({"contents":[]}),
        true,
    )
    .unwrap();
    assert_eq!(prepared.response_kind, upstream::ResponseKind::Antigravity);
    let response = prepared.request.send().await.unwrap();
    assert!(response.status().is_success());
    let mut source = response.bytes_stream();
    let mut events = antigravity::Events::default();
    let mut usage = usage::Usage::new(upstream::Format::Gemini);
    let mut output = Vec::new();
    while let Some(chunk) = source.next().await {
        output.extend(events.feed(&chunk.unwrap(), &mut usage).unwrap());
    }
    assert!(events.complete());
    assert!(!String::from_utf8(output).unwrap().contains("\"response\""));
    assert_eq!(usage.breakdown().total_tokens, 7);
    server.abort();
}

#[test]
fn missing_usage_stays_absent_and_count_scope_is_explicit() {
    let value = response::decode_json(
        json!({"response":{"candidates":[]}}),
        upstream::ResponseKind::Antigravity,
    )
    .unwrap();
    assert!(value.get("usageMetadata").is_none());
    assert!(
        response::decode_json(
            json!({"input_tokens":-1}),
            upstream::ResponseKind::CountTokens
        )
        .is_err()
    );
    assert!(
        response::decode_json(
            json!({"error":{"message":"secret"}}),
            upstream::ResponseKind::CountTokens
        )
        .is_err()
    );
    let mut selected = selection("claude");
    selected.auth["attributes"]["api_key"] = "sk-ant-oat-fixture".into();
    assert!(
        upstream::prepare(
            &config(""),
            &selected,
            "/v1/messages/count_tokens",
            &HeaderMap::new(),
            json!({"model":"alias","messages":[]}),
            false
        )
        .is_err()
    );
    selected.auth["attributes"]["api_key"] = "key".into();
    assert!(
        upstream::prepare(
            &config(""),
            &selected,
            "/v1/messages/count_tokens",
            &HeaderMap::new(),
            json!({"model":"alias","messages":[]}),
            true
        )
        .is_err()
    );
}

#[tokio::test]
async fn native_count_tokens_real_http_passthrough_fixture() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/v1/messages/count_tokens",
                post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                    assert_eq!(headers["authorization"], "Bearer upstream-secret");
                    assert_eq!(headers["anthropic-version"], "2023-06-01");
                    assert_eq!(body["model"], "configured");
                    assert!(body.get("stream").is_none());
                    assert!(body.get("max_tokens").is_none());
                    Json(json!({"input_tokens":17}))
                }),
            ),
        )
        .await
        .unwrap();
    });
    let mut selected = selection("claude");
    selected.auth["attributes"]["base_url"] = format!("http://{address}/v1").into();
    let prepared = upstream::prepare(&config("payload:\n  override:\n    - models: [{name: resolved-model, protocol: claude}]\n      params: {model: configured}\n"), &selected, "/v1/messages/count_tokens", &HeaderMap::new(), json!({"model":"alias","messages":[],"max_tokens":30}), false).unwrap();
    let value = prepared
        .request
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(
        response::decode_json(value, prepared.response_kind).unwrap(),
        json!({"input_tokens":17})
    );
    selected.provider = "openai".into();
    assert!(
        upstream::prepare(
            &config(""),
            &selected,
            "/v1/messages/count_tokens",
            &HeaderMap::new(),
            json!({"model":"alias","messages":[]}),
            false
        )
        .is_err()
    );
    server.abort();
}
