//! End-to-end loopback contracts for Gemini Chat JSON/SSE and source accounting.
use super::super::provider_matrix_tests::fixture;
use super::*;
use serde_json::json;

fn native() -> Value {
    json!({"candidates":[{"content":{"parts":[{"text":"hello google"}]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":10,"cachedContentTokenCount":2,
            "candidatesTokenCount":3,"thoughtsTokenCount":1,"totalTokenCount":14}})
}

#[tokio::test]
async fn google_translation_json_and_sse_real_router() {
    for provider in ["gemini", "vertex", "vertexai", "antigravity"] {
        // The shared router fixture provides API-key credentials, not Cloud Code
        // metadata. Antigravity preparation/unwrap is covered separately below.
        if provider == "antigravity" {
            continue;
        }
        for stream in [false, true] {
            for include_usage in [false, true] {
                let wire = if stream {
                    format!("data: {}", native())
                } else {
                    native().to_string()
                };
                let result = fixture::run(
                    provider,
                    "/v1/chat/completions",
                    json!({"model":"public-alias","messages":[{"role":"user","content":"hi"}],
                        "stream":stream,"stream_options":{"include_usage":include_usage}}),
                    if stream {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                    &wire,
                )
                .await;
                assert_eq!(result.status, StatusCode::OK);
                assert_eq!(result.usage["failed"], false);
                assert_eq!(result.usage["tokens"]["input_tokens"], 10);
                assert_eq!(result.usage["tokens"]["output_tokens"], 4);
                assert_eq!(result.usage["tokens"]["total_tokens"], 14);
                assert_eq!(result.usage["tokens"]["cache_read_tokens"], 2);
                assert!(result.body.contains("hello google"));
                assert!(result.upstream.body.get("messages").is_none());
                assert_eq!(
                    result.upstream.body["contents"][0]["parts"][0]["text"],
                    "hi"
                );
                if stream {
                    assert_eq!(result.body.matches("data: [DONE]").count(), 1);
                    let chunks: Vec<Value> = result
                        .body
                        .split("\n\n")
                        .filter_map(|s| s.strip_prefix("data: "))
                        .filter(|s| *s != "[DONE]")
                        .map(|s| serde_json::from_str(s).unwrap())
                        .collect();
                    assert!(chunks.iter().all(|c| c["model"] == "public-alias"));
                    assert_eq!(
                        chunks.iter().any(|c| c.get("usage").is_some()),
                        include_usage
                    );
                    if include_usage {
                        assert_eq!(chunks.last().unwrap()["usage"]["completion_tokens"], 4);
                    }
                } else {
                    let chat: Value = serde_json::from_str(&result.body).unwrap();
                    assert_eq!(chat["choices"][0]["message"]["content"], "hello google");
                    assert_eq!(chat["choices"][0]["finish_reason"], "stop");
                    assert_eq!(chat["usage"]["completion_tokens"], 4);
                }
            }
        }
    }
}

#[test]
fn google_translation_antigravity_unwrap_precedes_usage_and_translation() {
    let mut envelope = json!({"response":native()});
    let usage = envelope["response"]
        .as_object_mut()
        .unwrap()
        .remove("usageMetadata")
        .unwrap();
    envelope["usageMetadata"] = usage;
    let kind = upstream::ResponseKind::GeminiChat {
        antigravity: true,
        include_usage: true,
    };
    let value = decode_json(envelope.clone(), kind).unwrap();
    assert!(value.get("response").is_none());
    let chat = crate::protocol::gemini::chat_response(&value, "alias").unwrap();
    assert_eq!(chat["usage"]["total_tokens"], 14);
    for size in [1, 7, 4096] {
        let wire = format!("data: {envelope}\n\n");
        let mut events = crate::gateway::antigravity::Events::default();
        let mut usage = Usage::new(upstream::Format::Gemini);
        let mut translator = crate::protocol::gemini::StreamTranslator::new("alias", true);
        let mut output = Vec::new();
        for chunk in wire.as_bytes().chunks(size) {
            output.extend(
                translator
                    .feed(&events.feed(chunk, &mut usage).unwrap())
                    .unwrap(),
            );
        }
        output.extend(translator.finish().unwrap());
        assert!(events.complete());
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("\"response\""));
        assert_eq!(output.matches("data: [DONE]").count(), 1);
        assert_eq!(usage.breakdown().total_tokens, 14);
        assert_eq!(usage.breakdown().input.cache_read_tokens, 2);
        assert_eq!(usage.breakdown().output.reasoning_tokens, 1);
    }
    assert!(decode_json(json!({"error":{"message":"redacted"}}), kind).is_err());
}
