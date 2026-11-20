//! Real-router provider contracts with synthetic credentials and loopback upstreams only.
#[path = "provider_matrix_fixture.rs"]
pub(super) mod fixture;
use axum::http::StatusCode;
use fixture::{Observed, SECRET, run};
use serde_json::{Value, json};

fn claude_json() -> Value {
    json!({"id":"msg_matrix","type":"message","role":"assistant","model":"resolved-model",
        "content":[{"type":"text","text":"hello matrix"}],"stop_reason":"end_turn","stop_sequence":null,
        "usage":{"input_tokens":7,"cache_read_input_tokens":2,"cache_creation_input_tokens":1,"output_tokens":4}})
}

fn claude_sse() -> String {
    [
        json!({"type":"message_start","message":{"id":"msg_matrix","type":"message","role":"assistant","model":"resolved-model","content":[],"usage":{"input_tokens":7,"cache_read_input_tokens":2,"cache_creation_input_tokens":1,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello matrix"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":4}}),
        json!({"type":"message_stop"}),
    ].iter().map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap())).collect()
}

fn assert_success(result: &Observed, stream: bool, input: i64, output: i64) {
    assert_eq!(result.status, StatusCode::OK);
    assert_eq!(result.usage["failed"], false);
    assert_eq!(result.usage["stream"], stream);
    assert_eq!(result.usage["fail"]["status_code"], 200);
    assert_eq!(result.usage["tokens"]["input_tokens"], input);
    assert_eq!(result.usage["tokens"]["output_tokens"], output);
    assert_eq!(result.usage["tokens"]["total_tokens"], input + output);
    assert_eq!(result.usage["token_breakdown"]["schema_version"], 2);
    assert_eq!(
        result.headers["content-type"],
        if stream {
            "text/event-stream"
        } else {
            "application/json"
        }
    );
}

async fn claude_case(chat: bool, stream: bool) {
    let path = if chat {
        "/v1/chat/completions"
    } else {
        "/v1/messages"
    };
    let body = json!({"model":"public-alias","messages":[{"role":"user","content":"hi"}],"max_tokens":32,"stream":stream});
    let wire = if stream {
        claude_sse()
    } else {
        claude_json().to_string()
    };
    let result = run(
        "claude",
        path,
        body,
        if stream {
            "text/event-stream"
        } else {
            "application/json"
        },
        &wire,
    )
    .await;
    assert_success(&result, stream, 10, 4);
    assert_eq!(result.upstream.target, "/tenant/v1/messages");
    // Current CPA contract uses bearer for API-key credentials at a custom base.
    assert_eq!(
        result.upstream.headers["authorization"],
        format!("Bearer {SECRET}")
    );
    assert!(!result.upstream.headers.contains_key("x-api-key"));
    assert_eq!(result.upstream.headers["anthropic-version"], "2023-06-01");
    assert_eq!(result.upstream.body["model"], "resolved-model");
    assert_eq!(result.upstream.body["stream"], stream);
    assert_eq!(result.upstream.body["max_tokens"], 32);
    assert_eq!(result.upstream.body["messages"][0]["role"], "user");
    assert_eq!(result.usage["tokens"]["cache_read_tokens"], 2);
    assert_eq!(result.usage["tokens"]["cache_creation_tokens"], 1);
    assert!(result.body.contains("hello matrix"));
    if stream {
        if chat {
            let events = data(&result.body);
            assert_eq!(*events.last().unwrap(), "[DONE]");
            let chunks: Vec<Value> = events[..events.len() - 1]
                .iter()
                .map(|s| serde_json::from_str(s).unwrap())
                .collect();
            assert!(chunks.iter().all(|v| v["model"] == "public-alias"));
            assert!(
                chunks
                    .iter()
                    .any(|v| v.pointer("/choices/0/delta/content") == Some(&json!("hello matrix")))
            );
            assert!(
                chunks
                    .iter()
                    .any(|v| v.pointer("/choices/0/finish_reason") == Some(&json!("stop")))
            );
            assert!(!result.body.contains("event: message_start"));
        } else {
            assert_eq!(result.body, wire);
        }
    } else {
        let value: Value = serde_json::from_str(&result.body).unwrap();
        if chat {
            assert_eq!(value["object"], "chat.completion");
            assert_eq!(value["model"], "public-alias");
            assert_eq!(value["choices"][0]["message"]["content"], "hello matrix");
            assert_eq!(value["choices"][0]["finish_reason"], "stop");
            assert_eq!(value["usage"]["prompt_tokens"], 10);
        } else {
            assert_eq!(value, claude_json());
        }
    }
}

fn data(wire: &str) -> Vec<&str> {
    wire.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .collect()
}

#[tokio::test]
async fn claude_api_key_native_json() {
    claude_case(false, false).await;
}
#[tokio::test]
async fn claude_api_key_native_sse() {
    claude_case(false, true).await;
}
#[tokio::test]
async fn chat_to_claude_api_key_json() {
    claude_case(true, false).await;
}
#[tokio::test]
async fn chat_to_claude_api_key_sse() {
    claude_case(true, true).await;
}

async fn gemini_case(stream: bool) {
    let action = if stream {
        "streamGenerateContent"
    } else {
        "generateContent"
    };
    let path =
        format!("/v1beta/models/public-alias:{action}?key=matrix-query-key&untrusted=ignored");
    let value = json!({"candidates":[{"content":{"role":"model","parts":[{"text":"hello matrix"}]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":10,"cachedContentTokenCount":2,"candidatesTokenCount":3,"thoughtsTokenCount":1,"totalTokenCount":14}});
    let wire = if stream {
        format!("data: {value}\n\n")
    } else {
        value.to_string()
    };
    let result = run(
        "gemini",
        &path,
        json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]}),
        if stream {
            "text/event-stream"
        } else {
            "application/json"
        },
        &wire,
    )
    .await;
    assert_success(&result, stream, 10, 4);
    let target = url::Url::parse(&format!("http://127.0.0.1{}", result.upstream.target)).unwrap();
    assert_eq!(
        target.path(),
        format!("/tenant/v1/v1beta/models/resolved-model:{action}")
    );
    assert_eq!(target.query(), if stream { Some("alt=sse") } else { None });
    assert_eq!(result.upstream.headers["x-goog-api-key"], SECRET);
    assert!(!result.upstream.headers.contains_key("authorization"));
    assert!(!result.upstream.headers.contains_key("x-api-key"));
    assert_eq!(
        result.upstream.body["contents"][0]["parts"][0]["text"],
        "hi"
    );
    assert_eq!(result.usage["tokens"]["cache_read_tokens"], 2);
    assert_eq!(result.usage["tokens"]["reasoning_tokens"], 1);
    if stream {
        assert_eq!(result.body, wire);
    } else {
        assert_eq!(serde_json::from_str::<Value>(&result.body).unwrap(), value);
    }
}

#[tokio::test]
async fn gemini_api_key_native_json() {
    gemini_case(false).await;
}
#[tokio::test]
async fn gemini_api_key_native_sse() {
    gemini_case(true).await;
}

#[tokio::test]
async fn named_openai_provider_uses_home_auth_and_trusted_base() {
    let value = json!({"id":"chat_matrix","object":"chat.completion","model":"resolved-model",
        "choices":[{"index":0,"message":{"role":"assistant","content":"hello matrix"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14}});
    let result = run(
        "private-compatible-cloud",
        "/v1/chat/completions",
        json!({"model":"public-alias","messages":[{"role":"user","content":"hi"}]}),
        "application/json",
        &value.to_string(),
    )
    .await;
    assert_success(&result, false, 10, 4);
    assert_eq!(result.upstream.target, "/tenant/v1/chat/completions");
    assert_eq!(
        result.upstream.headers["authorization"],
        format!("Bearer {SECRET}")
    );
    assert!(!result.upstream.headers.contains_key("x-api-key"));
    assert!(!result.upstream.headers.contains_key("x-goog-api-key"));
    assert_eq!(result.upstream.body["model"], "resolved-model");
    assert_eq!(result.upstream.body["messages"][0]["content"], "hi");
    assert_eq!(serde_json::from_str::<Value>(&result.body).unwrap(), value);
}

fn responses_wire(tools: bool) -> String {
    let output = if tools {
        json!([{"type":"function_call","id":"fc_matrix","call_id":"call_matrix","name":"lookup","arguments":"{\"city\":\"Oslo\"}","status":"completed"}])
    } else {
        json!([{"type":"message","id":"msg_matrix","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hello matrix","annotations":[]}]}])
    };
    let response = json!({"id":"resp_matrix","object":"response","status":"completed","model":"resolved-model","created_at":1,
        "output":output,"usage":{"input_tokens":10,"output_tokens":4,"total_tokens":14}});
    let mut events = vec![
        json!({"type":"response.created","response":{"id":"resp_matrix","object":"response","status":"in_progress","model":"resolved-model","created_at":1,"output":[]}}),
    ];
    events.push(json!({"type":"response.output_item.added","output_index":0,"item":if tools {json!({"type":"function_call","id":"fc_matrix","call_id":"call_matrix","name":"lookup","arguments":""})} else {json!({"type":"message","id":"msg_matrix","role":"assistant","content":[]})}}));
    if tools {
        events.push(json!({"type":"response.function_call_arguments.delta","item_id":"fc_matrix","output_index":0,"delta":"{\"city\":\"Oslo\"}"}));
        events.push(json!({"type":"response.function_call_arguments.done","item_id":"fc_matrix","output_index":0,"arguments":"{\"city\":\"Oslo\"}"}));
    } else {
        events.push(json!({"type":"response.output_text.delta","item_id":"msg_matrix","output_index":0,"content_index":0,"delta":"hello matrix"}));
    }
    events.push(
        json!({"type":"response.output_item.done","output_index":0,"item":response["output"][0]}),
    );
    events.push(json!({"type":"response.completed","response":response}));
    events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect()
}

async fn text_provider_case(provider: &str, responses: bool, stream: bool, tools: bool) {
    let path = if responses {
        "/v1/responses"
    } else {
        "/v1/chat/completions"
    };
    let mut body = if responses {
        json!({"model":"public-alias","input":"hi","stream":stream})
    } else {
        json!({"model":"public-alias","messages":[{"role":"user","content":"hi"}],"stream":stream,"stream_options":{"include_usage":true}})
    };
    if tools {
        if responses {
            body["tools"] = json!([{"type":"function","name":"lookup","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}]);
            body["input"] = json!([{"type":"function_call","call_id":"prior_call","name":"lookup","arguments":"{}"},{"type":"function_call_output","call_id":"prior_call","output":"prior result"},{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]);
        } else {
            body["tools"] = json!([{"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}]);
            body["messages"] = json!([{"role":"assistant","content":null,"tool_calls":[{"id":"prior_call","type":"function","function":{"name":"lookup","arguments":"{}"}}]},{"role":"tool","tool_call_id":"prior_call","content":"prior result"},{"role":"user","content":"hi"}]);
        }
    }
    let native_responses = matches!(provider, "xai" | "meta") || responses;
    let wire = if native_responses && (provider != "kimi" || stream) {
        responses_wire(tools)
    } else if responses {
        json!({"id":"resp_matrix","object":"response","status":"completed","model":"resolved-model","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello matrix"}]}],"usage":{"input_tokens":10,"output_tokens":4,"total_tokens":14}}).to_string()
    } else if stream {
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"id":"chat_matrix","object":"chat.completion.chunk","model":"resolved-model","choices":[{"index":0,"delta":{"role":"assistant","content":"hello matrix"},"finish_reason":null}]}),
            json!({"id":"chat_matrix","object":"chat.completion.chunk","model":"resolved-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14}})
        )
    } else {
        json!({"id":"chat_matrix","object":"chat.completion","model":"resolved-model","choices":[{"index":0,"message":{"role":"assistant","content":"hello matrix"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14}}).to_string()
    };
    let upstream_sse = stream || matches!(provider, "xai" | "meta");
    let result = run(
        provider,
        path,
        body,
        if upstream_sse {
            "text/event-stream"
        } else {
            "application/json"
        },
        &wire,
    )
    .await;
    assert_success(&result, stream, 10, 4);
    assert_eq!(
        result.upstream.target,
        if native_responses {
            "/tenant/v1/responses"
        } else {
            "/tenant/v1/chat/completions"
        }
    );
    assert_eq!(
        result.upstream.headers["authorization"],
        format!("Bearer {SECRET}")
    );
    assert_eq!(result.upstream.body["model"], "resolved-model");
    assert_eq!(result.upstream.body["stream"], upstream_sse);
    if tools {
        assert_eq!(result.upstream.body["tools"][0]["name"], "lookup");
        assert_eq!(result.upstream.body["input"][0]["call_id"], "prior_call");
        assert_eq!(result.upstream.body["input"][1]["output"], "prior result");
        if provider == "meta" {
            assert_eq!(result.upstream.body["tools"][1]["name"], "muse");
        }
        if !responses && !stream {
            let value: Value = serde_json::from_str(&result.body).unwrap();
            assert_eq!(value["choices"][0]["finish_reason"], "tool_calls");
            assert_eq!(
                value["choices"][0]["message"]["tool_calls"][0]["id"],
                "call_matrix"
            );
        } else {
            assert!(result.body.contains("lookup"));
        }
    } else {
        assert!(result.body.contains("hello matrix"));
    }
    if stream && !responses {
        assert!(result.body.contains("[DONE]"));
    }
}

#[tokio::test]
async fn native_text_provider_matrix() {
    for provider in ["xai", "kimi", "meta"] {
        for responses in [false, true] {
            for stream in [false, true] {
                text_provider_case(provider, responses, stream, false).await;
            }
        }
    }
}

#[tokio::test]
async fn xai_meta_function_tool_bridge_matrix() {
    for provider in ["xai", "meta"] {
        for responses in [false, true] {
            for stream in [false, true] {
                text_provider_case(provider, responses, stream, true).await;
            }
        }
    }
}

#[test]
fn unported_text_sources_fail_closed_before_transport() {
    let config =
        crate::config::Config::parse(b"credential-concurrency:\n  lifecycle-config-revision: 7\n")
            .unwrap();
    for provider in ["xai", "meta", "kimi", "kimi-ai"] {
        let selection = serde_json::from_value(json!({"provider":provider,"model":"resolved-model","auth_index":"matrix-index","auth":{"attributes":{"api_key":SECRET}}})).unwrap();
        for (path, body) in [
            (
                "/v1/messages",
                json!({"model":"public-alias","messages":[],"max_tokens":32}),
            ),
            (
                "/v1beta/models/public-alias:generateContent",
                json!({"contents":[]}),
            ),
        ] {
            let error = match super::upstream::prepare(
                &config,
                &selection,
                path,
                &http::HeaderMap::new(),
                body,
                false,
            ) {
                Ok(_) => panic!("unported source accepted"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("source format unsupported"));
        }
    }
}

#[tokio::test]
async fn local_json_parse_failure_releases_without_credential_cooldown() {
    for (provider, path, body) in [
        (
            "claude",
            "/v1/messages",
            json!({"model":"public-alias","max_tokens":32,"messages":[]}),
        ),
        (
            "claude",
            "/v1/chat/completions",
            json!({"model":"public-alias","messages":[{"role":"user","content":"hi"}]}),
        ),
        (
            "gemini",
            "/v1beta/models/public-alias:generateContent",
            json!({"contents":[]}),
        ),
        (
            "private-compatible-cloud",
            "/v1/chat/completions",
            json!({"model":"public-alias","messages":[]}),
        ),
    ] {
        // Upstream 200 is not a credential failure: the local parser must not cause cooldown.
        // The fixture expects exactly one RPOP then one release, even with retries enabled.
        let result = run(provider, path, body, "application/json", "{not-json").await;
        assert_eq!(result.status, StatusCode::BAD_GATEWAY);
        let error: Value = serde_json::from_str(&result.body).unwrap();
        assert_eq!(error["error"]["type"], "upstream_error");
        assert!(!result.body.contains("not-json"));
        assert_eq!(result.usage["failed"], true);
        assert_eq!(
            result.usage["fail"],
            json!({"status_code":502,"code":"request_scoped","body":""})
        );
        assert_eq!(result.usage["tokens"]["total_tokens"], 0);
    }
}
