use super::*;
const O: Format = Format::OpenAiChat;
const C: Format = Format::ClaudeMessages;
fn req(from: Format, v: &Value) -> Result<Value> {
    translate_request(
        from,
        if from == O { C } else { O },
        v,
        RequestOptions {
            target_model: "target",
            default_max_tokens: Some(1024),
        },
    )
}
fn resp(from: Format, v: &Value) -> Result<Value> {
    translate_response(
        from,
        if from == O { C } else { O },
        v,
        ResponseOptions {
            target_model: "target",
            created: 123,
        },
    )
}
fn request() -> Value {
    json!({"model":"source","messages":[{"role":"system","content":"help"},{"role":"user","content":[{"type":"text","text":"hello"}]}],"temperature":0.5,"stop":"END"})
}
#[test]
fn text_system_and_sampling() {
    let original = request();
    let c = req(O, &original).unwrap();
    assert_eq!(c["system"], "help");
    assert_eq!(c["messages"][0]["content"][0]["text"], "hello");
    assert_eq!(c["max_tokens"], 1024);
    let o = req(C, &c).unwrap();
    assert_eq!(o["messages"][0], json!({"role":"system","content":"help"}));
    assert_eq!(o["messages"][1]["content"], "hello");
    assert_eq!(o["stop"], json!(["END"]));
    assert_eq!(original, request());
    let mut late = request();
    late["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role":"system","content":"late"}));
    assert!(req(O, &late).is_err());
    assert!(
        translate_request(
            O,
            C,
            &original,
            RequestOptions {
                target_model: "t",
                default_max_tokens: None
            }
        )
        .is_err()
    );
}
#[test]
fn basic_tool_conversation_both_directions() {
    let o = json!({"model":"m","max_tokens":4096,"messages":[{"role":"user","content":"weather"},{"role":"assistant","content":null,"tool_calls":[{"id":"call1","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Paris\"}"}}]},{"role":"tool","tool_call_id":"call1","content":"sunny"},{"role":"user","content":"thanks"}],"tools":[{"type":"function","function":{"name":"weather","description":"weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}],"tool_choice":{"type":"function","function":{"name":"weather"}}});
    let c = req(O, &o).unwrap();
    assert_eq!(
        c["messages"][1]["content"][0]["input"],
        json!({"city":"Paris"})
    );
    assert_eq!(c["messages"][2]["content"][0]["type"], "tool_result");
    let back = req(C, &c).unwrap();
    assert_eq!(back["messages"], o["messages"]);
    assert_eq!(back["tools"], o["tools"]);
    assert_eq!(back["tool_choice"], o["tool_choice"]);
    let mut bad = o.clone();
    bad["messages"][1]["tool_calls"][0]["function"]["arguments"] = json!("not json");
    assert!(req(O, &bad).is_err());
    bad = o.clone();
    bad["messages"][2]["tool_call_id"] = json!("wrong");
    assert!(req(O, &bad).is_err());
    bad = o;
    bad["tools"][0]["function"]["strict"] = json!(true);
    assert!(matches!(
        req(O, &bad),
        Err(TranslationError::Unsupported(_))
    ));
}
#[test]
fn parameterless_claude_calls_default_only_missing_input() {
    let mut request = json!({"model":"m","max_tokens":100,"messages":[
        {"role":"user","content":"time"},
        {"role":"assistant","content":[{"type":"tool_use","id":"t","name":"time"}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"noon"}]}
    ]});
    let translated = req(C, &request).unwrap();
    let arguments = &translated["messages"][1]["tool_calls"][0]["function"]["arguments"];
    assert!(arguments.is_string());
    assert_eq!(arguments, "{}");
    assert_eq!(translated["messages"][2]["tool_call_id"], "t");
    for invalid in [Value::Null, json!(""), json!([]), json!(false)] {
        request["messages"][1]["content"][0]["input"] = invalid;
        assert!(req(C, &request).is_err());
    }
    request["messages"][1]["content"][0]["input"] = json!({"city":"Paris"});
    assert_eq!(
        req(C, &request).unwrap()["messages"][1]["tool_calls"][0]["function"]["arguments"],
        "{\"city\":\"Paris\"}"
    );
    request["messages"][2]["content"][0]["tool_use_id"] = json!("wrong");
    assert!(req(C, &request).is_err());
}
#[test]
fn nonstream_responses_and_tools() {
    let o = json!({"id":"r","object":"chat.completion","created":123,"model":"target","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}});
    let c = resp(O, &o).unwrap();
    assert_eq!(c["stop_reason"], "end_turn");
    assert_eq!(resp(C, &c).unwrap(), o);
    let mut tool = o.clone();
    tool["choices"][0]["message"] = json!({"role":"assistant","content":null,"tool_calls":[{"type":"function","id":"t","function":{"name":"f","arguments":"{}"}}]});
    tool["choices"][0]["finish_reason"] = json!("tool_calls");
    let c = resp(O, &tool).unwrap();
    assert_eq!(c["content"][0]["type"], "tool_use");
    assert_eq!(resp(C, &c).unwrap(), tool);
    tool["choices"][0]["finish_reason"] = json!("content_filter");
    assert!(matches!(
        resp(O, &tool),
        Err(TranslationError::Unsupported(_))
    ));
    let mut length = o;
    length["choices"][0]["finish_reason"] = json!("length");
    assert_eq!(resp(O, &length).unwrap()["stop_reason"], "max_tokens");
}
#[test]
fn explicit_unsupported_media_streaming_and_semantics() {
    for kind in ["image_url", "input_audio", "file", "video_url"] {
        let mut r = request();
        r["messages"][1]["content"] = json!([{"type":kind}]);
        assert!(
            matches!(req(O, &r), Err(TranslationError::Unsupported(_))),
            "{kind}"
        );
    }
    for kind in ["image", "document", "thinking", "server_tool_use"] {
        let r = json!({"model":"m","max_tokens":100,"messages":[{"role":"user","content":[{"type":kind}]}]});
        assert!(
            matches!(req(C, &r), Err(TranslationError::Unsupported(_))),
            "{kind}"
        );
    }
    for from in [O, C] {
        assert!(req(from, &json!({"stream":true})).is_err());
        assert!(matches!(
            resp(from, &json!({"stream":true})),
            Err(TranslationError::Unsupported(_))
        ));
    }
    assert!(matches!(
        resp(O, &json!({"id":"r","object":"chat.completion.chunk"})),
        Err(TranslationError::Unsupported(_))
    ));
    assert!(matches!(
        resp(C, &json!({"id":"r","type":"content_block_delta"})),
        Err(TranslationError::Unsupported(_))
    ));
    for (key, value) in [
        ("reasoning_effort", json!("high")),
        ("response_format", json!({"type":"json_object"})),
        ("n", json!(2)),
        ("stream_options", json!({"unknown":true})),
    ] {
        let mut r = request();
        r[key] = value;
        assert!(
            matches!(req(O, &r), Err(TranslationError::Unsupported(_))),
            "{key}"
        );
    }
    let c = json!({"id":"r","type":"message","role":"assistant","model":"m","content":[{"type":"thinking","thinking":"secret"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2}});
    assert!(matches!(resp(C, &c), Err(TranslationError::Unsupported(_))));
}
