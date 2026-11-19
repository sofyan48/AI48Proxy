//! Real local HTTP fixtures; no Home enrollment or live-provider parity claim.
use super::{
    tests::{config, selection},
    *,
};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn responses_dispatch_options_source_context_and_final_payload_barrier() {
    for provider in ["openai", "openai-compatibility", "named-provider"] {
        let mut selected = selection(provider);
        selected.auth["attributes"]["home_upstream_model"] = json!("resolved-model");
        selected.auth["metadata"] = json!({"credential_options":{"models":[
            {"name":"unrelated","alias":"alias","use-max-completion-tokens":false},
            {"name":"resolved-model","alias":"alias","use-max-completion-tokens":true}]}});
        let mut headers = HeaderMap::new();
        headers.insert("x-client", "fixture".parse().unwrap());
        let cfg = config(
            "payload:\n  override:\n    - models: [{name: alias, protocol: openai, from-protocol: openai-response, headers: {x-client: fixture}}]\n      params: {model: final-model, temperature: 0.25}\n  filter:\n    - models: [{name: alias, protocol: openai, from-protocol: openai-response}]\n      params: [max_completion_tokens]\n",
        );
        let prepared = upstream::prepare(
            &cfg,
            &selected,
            "/v1/responses",
            &headers,
            json!({"model":"alias","input":"Hi","max_output_tokens":33}),
            false,
        )
        .unwrap();
        assert_eq!(
            prepared.response_kind,
            upstream::ResponseKind::ChatResponses
        );
        let request = prepared.request.build().unwrap();
        assert_eq!(request.url().path(), "/v1/chat/completions");
        let body: Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["messages"][0]["content"], "Hi");
        assert_eq!(body["model"], "final-model");
        assert_eq!(body["temperature"], 0.25);
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("max_completion_tokens").is_none());
        assert!(body.get("input").is_none());
        let prepared = upstream::prepare(
            &config(""),
            &selected,
            "/v1/responses",
            &headers,
            json!({"model":"alias","input":"Hi","max_output_tokens":33}),
            false,
        )
        .unwrap();
        let request = prepared.request.build().unwrap();
        let body: Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["max_completion_tokens"], 33);
    }
}
#[test]
fn compact_routes_use_native_provider_prepare_and_never_chat_translation() {
    for provider in ["codex", "openai", "openai-compatibility", "named"] {
        let prepared = upstream::prepare(
            &config(""),
            &selection(provider),
            "/v1/responses/compact",
            &HeaderMap::new(),
            json!({"model":"alias","input":"Hi"}),
            false,
        )
        .unwrap();
        assert_eq!(prepared.response_kind, upstream::ResponseKind::Native);
        assert_eq!(prepared.response_mode, crate::providers::ResponseMode::Json);
        let request = prepared.request.build().unwrap();
        assert_eq!(request.url().path(), "/v1/responses/compact");
        let body: Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["input"], "Hi");
        assert!(body.get("messages").is_none());
        assert!(body.get("stream").is_none());
    }
    assert!(
        upstream::prepare(
            &config(""),
            &selection("codex"),
            "/v1/responses/compact",
            &HeaderMap::new(),
            json!({"input":[],"stream":true}),
            true
        )
        .is_err()
    );
}

async fn fixture(stream: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let chat = json!({"id":"fixture","created":123,"choices":[{"index":0,"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}});
    let expected = chat.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        let mut buf = [0; 4096];
        loop {
            let n = socket.read(&mut buf).await.unwrap();
            assert_ne!(n, 0);
            received.extend_from_slice(&buf[..n]);
            if let Some(end) = received.windows(4).position(|v| v == b"\r\n\r\n") {
                let headers = std::str::from_utf8(&received[..end]).unwrap();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap();
                if received.len() < end + 4 + length {
                    continue;
                }
                assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1"));
                assert!(headers.contains("authorization: Bearer upstream-secret"));
                let body: Value =
                    serde_json::from_slice(&received[end + 4..end + 4 + length]).unwrap();
                assert_eq!(body["messages"][0]["role"], "system");
                assert_eq!(body["messages"][1]["content"], "Hi");
                assert_eq!(body["stream"], stream);
                assert!(body.get("input").is_none());
                if stream {
                    assert_eq!(body["stream_options"]["include_usage"], true);
                }
                break;
            }
        }
        let (content_type, wire) = if stream {
            let chunks = [
                json!({"id":"fixture","created":123,"choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}),
                json!({"id":"fixture","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                json!({"id":"fixture","choices":[],"usage":chat["usage"]}),
            ];
            let mut wire: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
            wire.push_str("data: [DONE]\n\n");
            ("text/event-stream", wire)
        } else {
            ("application/json", chat.to_string())
        };
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",wire.len()).as_bytes()).await.unwrap();
        for chunk in wire.as_bytes().chunks(3) {
            socket.write_all(chunk).await.unwrap();
        }
    });
    let mut selected = selection("named");
    selected.auth["attributes"]["base_url"] = json!(format!("http://{address}/v1"));
    let prepared = upstream::prepare(
        &config(""),
        &selected,
        "/v1/responses",
        &HeaderMap::new(),
        json!({"model":"alias","instructions":"Be helpful","input":"Hi","stream":stream}),
        stream,
    )
    .unwrap();
    let response = prepared.request.send().await.unwrap();
    assert!(response.status().is_success());
    let mut usage = usage::Usage::new(upstream::Format::OpenAi);
    if stream {
        let mut translator = crate::protocol::responses::ChatStreamTranslator::new("alias");
        let mut source = response.bytes_stream();
        let mut out = Vec::new();
        while let Some(bytes) = source.next().await {
            let bytes = bytes.unwrap();
            usage.feed(&bytes);
            out.extend(translator.feed(&bytes).unwrap());
        }
        translator.finish().unwrap();
        let data = std::str::from_utf8(&out)
            .unwrap()
            .trim()
            .rsplit_once("data: ")
            .unwrap()
            .1;
        let terminal: Value = serde_json::from_str(data).unwrap();
        assert_eq!(terminal["type"], "response.completed");
        assert_eq!(terminal["response"]["usage"]["total_tokens"], 7);
        assert_eq!(
            terminal["response"]["output"][0]["content"][0]["text"],
            "Hello"
        );
    } else {
        let value: Value = response.json().await.unwrap();
        usage.json(&value);
        let response = crate::protocol::responses::responses_response(&value, "alias").unwrap();
        assert_eq!(
            response["output"][0]["content"][0]["text"],
            expected["choices"][0]["message"]["content"]
        );
        assert_eq!(response["usage"]["total_tokens"], 7);
    }
    assert_eq!(usage.breakdown().total_tokens, 7);
    server.await.unwrap();
}
#[tokio::test]
async fn responses_compat_real_http_json() {
    fixture(false).await;
}
#[tokio::test]
async fn responses_compat_real_http_sse() {
    fixture(true).await;
}
