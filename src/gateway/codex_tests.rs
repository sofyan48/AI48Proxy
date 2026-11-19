use super::*;
use bytes::Bytes;
use futures_util::stream;
use serde_json::json;

#[test]
fn codex_chat_translation_preserves_source_context_and_final_barrier() {
    let config = super::super::tests::config(
        "payload:\n  override:\n    - models: [{name: alias, protocol: codex, from-protocol: openai, headers: {x-client: fixture}}]\n      params: {model: final-model, 'reasoning.effort': low, temperature: 0.25}\n  filter:\n    - models: [{name: alias, protocol: codex, from-protocol: openai}]\n      params: [store, include]\n",
    );
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("x-client", "fixture".parse().unwrap());
    for oauth in [false, true] {
        let mut selected = super::super::tests::selection("codex");
        if oauth {
            selected.auth = json!({"id":"credential","attributes":{"base_url":"https://trusted.invalid/v1"},"metadata":{"access_token":"oauth-fixture","account_id":"account"}});
        }
        let prepared = super::super::upstream::prepare(&config,&selected,"/v1/chat/completions",&headers,
            json!({"model":"alias","messages":[{"role":"user","content":"Hello"}],"stream":true,"stream_options":{"include_usage":true}}),true).unwrap();
        assert_eq!(
            prepared.response_kind,
            super::super::upstream::ResponseKind::ResponsesChatUsage
        );
        assert_eq!(prepared.response_model, "alias");
        let request = prepared.request.build().unwrap();
        assert_eq!(request.url().path(), "/v1/responses");
        assert_eq!(request.headers().contains_key("chatgpt-account-id"), oauth);
        let body: Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["model"], "final-model");
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["temperature"], 0.25);
        assert!(body.get("store").is_none());
        assert!(body.get("include").is_none());
        assert!(body.get("messages").is_none());
        assert_eq!(body["input"][0]["content"][0]["text"], "Hello");
    }
}

#[test]
fn chat_incomplete_terminal_retains_usage_and_length_reason() {
    let mut response = response();
    response["status"] = json!("incomplete");
    response["incomplete_details"] = json!({"reason":"max_output_tokens"});
    let wire = format!(
        "data: {}\n\n",
        json!({"type":"response.incomplete","response":response})
    );
    let mut events = Events::for_chat();
    let mut usage = Usage::new(Format::OpenAi);
    events.feed(wire.as_bytes(), &mut usage).unwrap();
    assert!(events.complete());
    assert_eq!(usage.breakdown().total_tokens, 14);
    let chat =
        crate::protocol::responses::chat_response(events.response.as_ref().unwrap(), "alias")
            .unwrap();
    assert_eq!(chat["choices"][0]["finish_reason"], "length");
}

#[test]
fn native_responses_reasoning_uses_capability_validation_without_chat_fields() {
    let mut selected = super::super::tests::selection("codex");
    selected.model_info = Some(json!({"thinking":{"levels":["low","high"]}}));
    let prepared = super::super::upstream::prepare(&super::super::tests::config(""), &selected,
        "/v1/responses", &axum::http::HeaderMap::new(),
        json!({"model":"alias(high)","input":"hello","reasoning":{"effort":"low","summary":"auto"}}),false).unwrap();
    let request = prepared.request.build().unwrap();
    let body: Value = serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(body["reasoning"], json!({"effort":"high","summary":"auto"}));
    assert!(body.get("reasoning_effort").is_none());
}

fn response() -> Value {
    json!({"id":"resp_fixture","object":"response","status":"completed","output":[
        {"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]},
        {"type":"function_call","call_id":"call_fixture","name":"weather","arguments":"{}"}
    ],"usage":{"input_tokens":10,"output_tokens":4,"total_tokens":14,
        "input_tokens_details":{"cached_tokens":3},"output_tokens_details":{"reasoning_tokens":2}}})
}
fn terminal() -> String {
    format!(
        "event: response.completed\r\ndata: {}\r\n\r\n",
        json!({"type":"response.completed","response":response()})
    )
}

#[tokio::test]
async fn local_http_chat_executes_prepared_responses_and_collects_real_sse() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        let mut bytes = [0; 4096];
        loop {
            let count = socket.read(&mut bytes).await.unwrap();
            assert_ne!(count, 0);
            received.extend_from_slice(&bytes[..count]);
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
                assert!(headers.starts_with("POST /v1/responses HTTP/1.1"));
                assert!(headers.contains("authorization: Bearer upstream-secret"));
                let body: Value =
                    serde_json::from_slice(&received[end + 4..end + 4 + length]).unwrap();
                assert_eq!(body["input"][0]["content"][0]["text"], "Hello");
                assert_eq!(body["stream"], true);
                break;
            }
        }
        let wire = terminal();
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",wire.len()).as_bytes()).await.unwrap();
        for chunk in wire.as_bytes().chunks(7) {
            socket.write_all(chunk).await.unwrap();
        }
    });
    let mut selected = super::super::tests::selection("codex");
    selected.auth["attributes"]["base_url"] = json!(format!("http://{address}/v1"));
    let prepared = super::super::upstream::prepare(
        &super::super::tests::config(""),
        &selected,
        "/v1/chat/completions",
        &axum::http::HeaderMap::new(),
        json!({"model":"alias","messages":[{"role":"user","content":"Hello"}]}),
        false,
    )
    .unwrap();
    let response = prepared.request.send().await.unwrap();
    assert!(response.status().is_success());
    let mut usage = Usage::new(Format::OpenAi);
    let terminal = collect(
        response.bytes_stream(),
        CancellationToken::new(),
        &mut usage,
        4096,
        || {},
    )
    .await
    .unwrap();
    let chat = crate::protocol::responses::chat_response(&terminal, "alias").unwrap();
    assert_eq!(chat["choices"][0]["message"]["content"], "hello");
    assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(usage.breakdown().input.cache_read_tokens, 3);
    assert_eq!(usage.breakdown().output.reasoning_tokens, 2);
    server.await.unwrap();
}

#[tokio::test]
async fn collected_response_preserves_tools_and_usage_from_fragmented_terminal() {
    let wire = format!(
        "data: {}\n\n{}",
        json!({"type":"response.output_text.delta","delta":"hello"}),
        terminal()
    );
    let chunks: Vec<Result<Bytes, ()>> = wire
        .bytes()
        .map(|byte| Ok(Bytes::from(vec![byte])))
        .collect();
    let mut usage = Usage::new(Format::OpenAi);
    let mut first = 0;
    let collected = collect(
        stream::iter(chunks),
        CancellationToken::new(),
        &mut usage,
        4096,
        || first += 1,
    )
    .await
    .unwrap();
    assert_eq!(collected, response());
    assert_eq!(first, 1);
    let b = usage.breakdown();
    assert_eq!(b.quality, "complete");
    assert_eq!(b.input.cache_read_tokens, 3);
    assert_eq!(b.output.reasoning_tokens, 2);
}

#[tokio::test]
async fn completion_returns_without_waiting_for_transport_eof() {
    let source = stream::iter([Ok::<_, ()>(Bytes::from(terminal()))]).chain(stream::pending());
    let mut usage = Usage::new(Format::OpenAi);
    let collected = collect(source, CancellationToken::new(), &mut usage, 4096, || {})
        .await
        .unwrap();
    assert_eq!(collected, response());
}

#[tokio::test]
async fn collected_response_rejects_errors_truncation_and_limits() {
    for wire in [
        "data: [DONE]\n\n".to_owned(),
        "data: {broken}\n\n".to_owned(),
        "data: {\"type\":\"response.completed\"}\n\n".to_owned(),
        "data: {\"type\":\"response.created\"}\n\n".to_owned(),
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"object\":\"response\",\"status\":\"incomplete\"}}\n\n".to_owned(),
        terminal().trim_end().to_owned(),
    ] {
        let source = stream::iter([Ok::<_, ()>(Bytes::from(wire))]);
        assert!(collect(source,CancellationToken::new(),&mut Usage::new(Format::OpenAi),4096,|| {}).await.is_err());
    }
    for kind in ["error", "response.failed", "response.incomplete"] {
        let wire = format!(
            "data: {}\n\n",
            json!({"type":kind,"response":{"usage":{"input_tokens":3,"output_tokens":0,"total_tokens":3}}})
        );
        let mut usage = Usage::new(Format::OpenAi);
        assert!(
            collect(
                stream::iter([Ok::<_, ()>(Bytes::from(wire))]),
                CancellationToken::new(),
                &mut usage,
                4096,
                || {}
            )
            .await
            .is_err()
        );
        assert_eq!(usage.breakdown().total_tokens, 3);
    }
    let many = vec![Ok::<_, ()>(Bytes::from_static(b": keepalive\n\n")); 100];
    assert!(
        collect(
            stream::iter(many),
            CancellationToken::new(),
            &mut Usage::new(Format::OpenAi),
            128,
            || {}
        )
        .await
        .is_err()
    );
    let mut events = Events::with_limit(32);
    assert!(
        events
            .feed(&[b'x'; 33], &mut Usage::new(Format::OpenAi))
            .is_err()
    );
}

#[tokio::test]
async fn cancelled_and_broken_transport_do_not_collect_success() {
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(
        collect(
            stream::pending::<Result<Bytes, ()>>(),
            cancelled,
            &mut Usage::new(Format::OpenAi),
            4096,
            || {}
        )
        .await
        .is_err()
    );
    assert!(
        collect(
            stream::iter([Err::<Bytes, _>(())]),
            CancellationToken::new(),
            &mut Usage::new(Format::OpenAi),
            4096,
            || {}
        )
        .await
        .is_err()
    );
}

#[test]
fn streaming_terminal_tracker_requires_real_completed_event() {
    let mut events = Events::new();
    let mut usage = Usage::new(Format::OpenAi);
    events
        .feed(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"response.completed\"}\n\n",
            &mut usage,
        )
        .unwrap();
    assert!(!events.complete());
    for byte in terminal().bytes() {
        events.feed(&[byte], &mut usage).unwrap();
    }
    assert!(events.complete());
    assert_eq!(events.response.unwrap(), response());
}

#[test]
fn oauth_responses_prepare_keeps_codex_transport_for_both_downstream_modes() {
    let config = super::super::tests::config("");
    for stream in [false, true] {
        let selection: crate::home::client::Selection = serde_json::from_value(json!({
            "model":"gpt-resolved","provider":"codex","auth_index":"index",
            "auth":{"id":"credential","metadata":{"access_token":"oauth-fixture","account_id":"account-fixture"}}
        })).unwrap();
        let prepared = super::super::upstream::prepare(
            &config,
            &selection,
            "/v1/responses",
            &axum::http::HeaderMap::new(),
            json!({"model":"alias","input":"hello","stream":stream}),
            stream,
        )
        .unwrap();
        assert_eq!(
            prepared.response_mode,
            crate::providers::ResponseMode::CodexSse
        );
        assert!(prepared.stream);
        let request = prepared.request.build().unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(request.headers()["authorization"], "Bearer oauth-fixture");
        let body: Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["model"], "gpt-resolved");
        assert!(body["input"].is_array());
    }
}
