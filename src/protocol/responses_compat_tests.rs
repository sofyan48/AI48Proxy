use super::*;
fn chat() -> Value {
    json!({"id":"chat_fixture","created":123,"choices":[{"index":0,"message":{"role":"assistant","content":"Hello",
        "reasoning_content":"Think","tool_calls":[{"id":"call_a","type":"function","function":{"name":"weather","arguments":"{}"}}]},"finish_reason":"tool_calls"}],
        "usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14,"prompt_tokens_details":{"cached_tokens":3},"completion_tokens_details":{"reasoning_tokens":2}}})
}
#[test]
fn go_request_history_function_schema_and_options() {
    let request = json!({"instructions":"Be helpful","input":[
        {"role":"user","content":[{"type":"input_text","text":"Hi"},{"type":"input_image","image_url":"https://example.test/image","detail":"low"}]},
        {"type":"reasoning","summary":[{"type":"summary_text","text":"Think"}]},
        {"type":"function_call","call_id":"a","name":"weather","arguments":"{}"},
        {"type":"function_call","call_id":"b","name":"time","arguments":"{}"},
        {"type":"function_call_output","call_id":"a","output":"sunny"}],
        "max_output_tokens":100,"reasoning":{"effort":"high"},"prompt_cache_key":"explicit",
        "tools":[{"type":"function","name":"weather","parameters":{"type":"object"},"strict":true}],
        "tool_choice":{"type":"function","name":"weather"},"text":{"format":{"type":"json_schema","name":"result","schema":{"type":"object"},"strict":true}}});
    let body = responses_request(&request, "resolved").unwrap();
    assert_eq!(
        body["messages"][0],
        json!({"role":"system","content":"Be helpful"})
    );
    assert_eq!(
        body["messages"][1]["content"][1]["image_url"]["detail"],
        "low"
    );
    assert_eq!(
        body["messages"][2]["tool_calls"].as_array().unwrap().len(),
        2
    );
    assert_eq!(body["messages"][2]["reasoning_content"], "Think");
    assert_eq!(body["messages"][3]["role"], "tool");
    assert_eq!(body["max_tokens"], 100);
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["tools"][0]["function"]["name"], "weather");
    assert_eq!(body["response_format"]["json_schema"]["name"], "result");
    assert!(body.get("input").is_none());
}
#[test]
fn go_terminal_chat_response_tools_reasoning_usage_and_incomplete() {
    let out = responses_response(&chat(), "alias").unwrap();
    assert_eq!(out["object"], "response");
    assert_eq!(out["output"][0]["type"], "reasoning");
    assert_eq!(out["output"][1]["content"][0]["text"], "Hello");
    assert_eq!(out["output"][2]["call_id"], "call_a");
    assert_eq!(out["usage"]["input_tokens_details"]["cached_tokens"], 3);
    assert_eq!(out["usage"]["output_tokens_details"]["reasoning_tokens"], 2);
    let mut value = chat();
    value["choices"][0]["finish_reason"] = json!("length");
    let out = responses_response(&value, "alias").unwrap();
    assert_eq!(out["status"], "incomplete");
    assert_eq!(out["incomplete_details"]["reason"], "max_output_tokens");
}
fn sse(values: &[Value]) -> String {
    let mut wire: String = values
        .iter()
        .map(|v| format!("data: {v}\r\n\r\n"))
        .collect();
    wire.push_str("data: [DONE]\r\n\r\n");
    wire
}
fn events(bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(bytes)
        .unwrap()
        .split("\n\n")
        .filter_map(|event| event.lines().find_map(|line| line.strip_prefix("data: ")))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}
#[test]
fn go_stream_lifecycle_fragmented_tools_reasoning_and_trailing_usage() {
    let chunk = |delta: Value, finish: Value| json!({"id":"fixture","created":123,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
    let wire = sse(&[
        chunk(json!({"reasoning_content":"Think"}), Value::Null),
        chunk(json!({"content":"Héllo"}), Value::Null),
        chunk(
            json!({"tool_calls":[{"index":4,"id":"a","type":"function","function":{"name":"weather","arguments":"{"}}]}),
            Value::Null,
        ),
        chunk(
            json!({"tool_calls":[{"index":7,"id":"b","type":"function","function":{"name":"time","arguments":"{}"}}]}),
            Value::Null,
        ),
        chunk(
            json!({"tool_calls":[{"index":4,"function":{"arguments":"}"}}]}),
            Value::Null,
        ),
        chunk(json!({}), json!("tool_calls")),
        json!({"id":"fixture","choices":[],"usage":chat()["usage"]}),
    ]);
    for size in [1, 7, 4096] {
        let mut translator = ChatStreamTranslator::new("alias");
        let mut out = Vec::new();
        for bytes in wire.as_bytes().chunks(size) {
            out.extend(translator.feed(bytes).unwrap());
        }
        translator.finish().unwrap();
        let events = events(&out);
        for (sequence, event) in events.iter().enumerate() {
            assert_eq!(event["sequence_number"], sequence);
        }
        assert_eq!(events[0]["type"], "response.created");
        let terminal = events.last().unwrap();
        assert_eq!(terminal["type"], "response.completed");
        assert_eq!(
            terminal["response"]["output"][1]["content"][0]["text"],
            "Héllo"
        );
        assert_eq!(terminal["response"]["output"][2]["arguments"], "{}");
        assert_eq!(terminal["response"]["output"][3]["call_id"], "b");
        assert_eq!(terminal["response"]["usage"]["total_tokens"], 14);
    }
}
#[test]
fn missing_terminal_failed_chunks_and_unsupported_state_fail_closed() {
    for wire in [
        "data: [DONE]\n\n",
        "data: {broken}\n\n",
        "data: {\"error\":{}}\n\n",
    ] {
        assert!(
            ChatStreamTranslator::new("alias")
                .feed(wire.as_bytes())
                .is_err()
        );
    }
    assert!(ChatStreamTranslator::new("alias").finish().is_err());
    let mut truncated = ChatStreamTranslator::new("alias");
    truncated.feed(b"data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n").unwrap();
    assert!(truncated.finish().is_err());
    let wire = sse(&[
        json!({"id":"x","choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":"length"}]}),
    ]);
    let mut incomplete = ChatStreamTranslator::new("alias");
    let out = incomplete.feed(wire.as_bytes()).unwrap();
    incomplete.finish().unwrap();
    assert_eq!(events(&out).last().unwrap()["type"], "response.incomplete");
    for body in [
        json!({"input":"hi","previous_response_id":"old"}),
        json!({"input":[{"type":"compaction","encrypted_content":"capsule"}]}),
        json!({"input":"hi","tools":[{"type":"web_search"}]}),
    ] {
        assert!(responses_request(&body, "model").is_err());
    }
}
