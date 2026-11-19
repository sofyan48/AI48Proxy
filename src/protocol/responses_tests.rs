//! Fixtures follow CPA codex_openai_{request,response}.go event shapes and mappings.
use super::*;

fn terminal() -> Value {
    json!({"id":"resp_fixture","object":"response","created_at":123,"status":"completed","output":[
        {"type":"reasoning","summary":[{"type":"summary_text","text":"Thinking"}]},
        {"type":"message","content":[{"type":"output_text","text":"Hello"}]},
        {"type":"function_call","call_id":"call_a","name":"weather","arguments":"{\"city\":\"Paris\"}"}
    ],"usage":{"input_tokens":20,"output_tokens":7,"total_tokens":27,
        "input_tokens_details":{"cached_tokens":4,"cache_creation_tokens":2},"output_tokens_details":{"reasoning_tokens":3}}})
}
fn wire(events: &[Value]) -> String {
    events
        .iter()
        .map(|v| {
            format!(
                "event: {}\r\ndata: {v}\r\n\r\n",
                v["type"].as_str().unwrap()
            )
        })
        .collect()
}
fn chunks(bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(bytes)
        .unwrap()
        .split("\n\n")
        .filter_map(|line| {
            let data = line.strip_prefix("data: ")?;
            if data == "[DONE]" {
                None
            } else {
                Some(serde_json::from_str(data).unwrap())
            }
        })
        .collect()
}
#[test]
fn chat_history_tools_images_and_schema_match_responses_shapes() {
    let body = json!({"model":"alias","messages":[
        {"role":"system","content":"Be helpful"},
        {"role":"user","content":[{"type":"text","text":"Weather?"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA==","detail":"low"}}]},
        {"role":"assistant","content":null,"tool_calls":[{"id":"call_a","type":"function","function":{"name":"weather","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"call_a","content":"sunny"}],
        "tools":[{"type":"function","function":{"name":"weather","description":"Weather","parameters":{"type":"object"},"strict":true}}],
        "tool_choice":{"type":"function","function":{"name":"weather"}},"max_completion_tokens":100,
        "reasoning_effort":"high","response_format":{"type":"json_schema","json_schema":{"name":"answer","schema":{"type":"object"},"strict":true}}});
    let out = chat_request(&body, "resolved").unwrap();
    assert_eq!(out["input"][0]["role"], "developer");
    assert_eq!(out["input"][1]["content"][1]["type"], "input_image");
    assert_eq!(
        out["input"][2],
        json!({"type":"function_call","call_id":"call_a","name":"weather","arguments":"{}"})
    );
    assert_eq!(
        out["input"][3],
        json!({"type":"function_call_output","call_id":"call_a","output":"sunny"})
    );
    assert_eq!(out["tools"][0]["name"], "weather");
    assert_eq!(
        out["tool_choice"],
        json!({"type":"function","name":"weather"})
    );
    assert_eq!(out["text"]["format"]["name"], "answer");
    assert_eq!(out["max_output_tokens"], 100);
    assert_eq!(out["reasoning"]["effort"], "high");
    assert!(out.get("messages").is_none());
}
#[test]
fn terminal_maps_text_reasoning_tools_usage_and_finish() {
    let chat = chat_response(&terminal(), "alias").unwrap();
    assert_eq!(chat["model"], "alias");
    assert_eq!(chat["created"], 123);
    assert_eq!(chat["choices"][0]["message"]["content"], "Hello");
    assert_eq!(
        chat["choices"][0]["message"]["reasoning_content"],
        "Thinking"
    );
    assert_eq!(
        chat["choices"][0]["message"]["tool_calls"][0]["id"],
        "call_a"
    );
    assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(chat["usage"]["prompt_tokens_details"]["cached_tokens"], 4);
    assert_eq!(
        chat["usage"]["completion_tokens_details"]["reasoning_tokens"],
        3
    );
    let mut incomplete = terminal();
    incomplete["status"] = json!("incomplete");
    incomplete["incomplete_details"] = json!({"reason":"max_output_tokens"});
    assert_eq!(
        chat_response(&incomplete, "alias").unwrap()["choices"][0]["finish_reason"],
        "length"
    );
}
#[test]
fn fragmented_stream_interleaves_tool_indices_without_duplicating_done_arguments() {
    let events = vec![
        json!({"type":"response.created","response":{"id":"r","created_at":123}}),
        json!({"type":"response.reasoning_summary_text.delta","delta":"Think"}),
        json!({"type":"response.output_text.delta","delta":"Héllo"}),
        json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_a","call_id":"call_a","name":"weather","arguments":""}}),
        json!({"type":"response.output_item.added","output_index":3,"item":{"type":"function_call","id":"fc_b","call_id":"call_b","name":"time","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"{}"}),
        json!({"type":"response.function_call_arguments.done","output_index":2,"arguments":"{}"}),
        json!({"type":"response.output_item.done","output_index":3,"item":{"type":"function_call","call_id":"call_b","name":"time","arguments":"{\"zone\":1}"}}),
        json!({"type":"response.completed","response":terminal()}),
    ];
    let wire = wire(&events);
    for size in [1, 2, 7, 4096] {
        let mut translator = StreamTranslator::new("alias", true);
        let mut out = Vec::new();
        for bytes in wire.as_bytes().chunks(size) {
            out.extend(translator.feed(bytes).unwrap());
        }
        translator.finish().unwrap();
        assert!(out.ends_with(b"data: [DONE]\n\n"));
        let chunks = chunks(&out);
        let args: Vec<_> = chunks
            .iter()
            .filter_map(|c| {
                c.pointer("/choices/0/delta/tool_calls/0/function/arguments")
                    .and_then(Value::as_str)
            })
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(args, vec!["{}", "{\"zone\":1}"]);
        assert_eq!(chunks.last().unwrap()["usage"]["prompt_tokens"], 20);
        assert_eq!(
            chunks[chunks.len() - 2]["choices"][0]["finish_reason"],
            "tool_calls"
        );
    }
}
#[test]
fn stream_errors_truncation_and_done_before_terminal_fail() {
    for wire in [
        "data: [DONE]\n\n",
        "data: {broken}\n\n",
        "data: {\"type\":\"response.failed\"}\n\n",
    ] {
        assert!(
            StreamTranslator::new("alias", false)
                .feed(wire.as_bytes())
                .is_err()
        );
    }
    assert!(StreamTranslator::new("alias", false).finish().is_err());
    let mut translator = StreamTranslator::new("alias", false);
    let out = translator
        .feed(wire(&[json!({"type":"response.completed","response":terminal()})]).as_bytes())
        .unwrap();
    assert!(chunks(&out).iter().all(|c| c.get("usage").is_none()));
}
#[test]
fn invalid_or_unrepresentable_requests_fail_instead_of_dropping_parts() {
    for body in [
        json!({"messages":false}),
        json!({"messages":[],"n":2}),
        json!({"messages":[{"role":"user","content":[{"type":"unknown"}]}]}),
    ] {
        assert!(chat_request(&body, "model").is_err());
    }
}
