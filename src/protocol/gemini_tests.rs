use super::*;

fn events(bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(bytes)
        .unwrap()
        .split("\n\n")
        .filter_map(|s| s.strip_prefix("data: "))
        .filter(|s| *s != "[DONE]")
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}
fn sse(value: Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}

// Source fixtures: gemini_openai_request.go and its response_test.go:
// CompletionTokensIncludeThoughts, GeminiFinishReasonOnlyOnFinalChunk,
// EmptyTextProducesEmptyString. Native metadata is deliberately optional.
#[test]
fn go_request_text_config_and_system() {
    let request = chat_request(&json!({"messages":[
        {"role":"system","content":"rules"}, {"role":"user","content":[{"type":"text","text":"hello"}]},
        {"role":"developer","content":"more rules"}],
        "temperature":0.5,"top_p":0.8,"max_completion_tokens":100,
        "response_format":{"type":"json_schema","json_schema":{"schema":{"type":"object"}}},
        "stream":true,"stream_options":{"include_usage":true}}), "gemini-test").unwrap();
    assert_eq!(request["systemInstruction"]["parts"][0]["text"], "rules");
    assert_eq!(
        request["contents"][1]["parts"][0]["text"],
        "<system-reminder>\nmore rules\n</system-reminder>"
    );
    assert_eq!(request["generationConfig"]["maxOutputTokens"], 100);
    assert_eq!(
        request["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert!(request.get("stream").is_none());
}
#[test]
fn go_function_declaration_call_result() {
    let request = chat_request(&json!({"messages":[
        {"role":"user","content":"weather"},
        {"role":"assistant","content":null,"tool_calls":[{"id":"abc","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Paris\"}"}}]},
        {"role":"tool","tool_call_id":"abc","content":"sunny"}],
        "tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object"}}}],
        "tool_choice":{"type":"function","function":{"name":"weather"}}}), "gemini").unwrap();
    assert_eq!(
        request["contents"][1]["parts"][0]["functionCall"]["args"]["city"],
        "Paris"
    );
    assert_eq!(
        request["contents"][1]["parts"][0]["thoughtSignature"],
        "skip_thought_signature_validator"
    );
    assert_eq!(
        request["contents"][2]["parts"][0]["functionResponse"],
        json!({"id":"abc","name":"weather","response":{"result":"sunny"}})
    );
    assert_eq!(
        request["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
        json!({"type":"object","properties":{}})
    );
    assert_eq!(
        request["toolConfig"]["functionCallingConfig"]["mode"],
        "ANY"
    );
}
#[test]
fn go_response_usage_thoughts_and_cache() {
    let out = chat_response(&json!({"candidates":[{"content":{"parts":[{"text":"hi"},{"text":"thinking","thought":true}]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":16,"candidatesTokenCount":5,"thoughtsTokenCount":42,"totalTokenCount":63,"cachedContentTokenCount":10}}), "fallback").unwrap();
    assert_eq!(out["model"], "fallback");
    assert_eq!(
        out["choices"][0]["message"]["reasoning_content"],
        "thinking"
    );
    assert_eq!(out["usage"]["completion_tokens"], 47);
    assert_eq!(out["usage"]["prompt_tokens"], 16);
    assert_eq!(out["usage"]["prompt_tokens_details"]["cached_tokens"], 10);
    let empty = chat_response(&json!({"candidates":[{"content":{"parts":[{"text":""},{"text":"","thought":true}]},"finishReason":"STOP"}]}), "g").unwrap();
    assert_eq!(empty["choices"][0]["message"]["content"], "");
    assert_eq!(empty["choices"][0]["message"]["reasoning_content"], "");
}
#[test]
fn fragmented_utf8_crlf_and_eof_without_done() {
    let bytes = format!(": keepalive\r\ndata: {}\r\n\r\ndata: {}", json!({"candidates":[{"content":{"parts":[{"text":"hé世界"}]}}]}),
        json!({"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":16,"candidatesTokenCount":5,"thoughtsTokenCount":42,"totalTokenCount":63,"cachedContentTokenCount":10}})).into_bytes();
    let mut bytewise = StreamTranslator::new("fallback", false);
    let mut bytewise_out = Vec::new();
    for byte in &bytes {
        bytewise_out.extend(bytewise.feed(std::slice::from_ref(byte)).unwrap());
    }
    bytewise_out.extend(bytewise.finish().unwrap());
    assert_eq!(
        events(&bytewise_out)[0]["choices"][0]["delta"]["content"],
        "hé世界"
    );
    for split in 0..=bytes.len() {
        let mut stream = StreamTranslator::new("fallback", true);
        let mut out = stream.feed(&bytes[..split]).unwrap();
        out.extend(stream.feed(&bytes[split..]).unwrap());
        out.extend(stream.finish().unwrap());
        assert!(stream.finish().unwrap().is_empty());
        let chunks = events(&out);
        assert_eq!(chunks[0]["choices"][0]["delta"]["content"], "hé世界");
        assert!(chunks[0]["choices"][0]["finish_reason"].is_null());
        assert_eq!(chunks[1]["choices"][0]["finish_reason"], "stop");
        assert_eq!(chunks[2]["choices"], json!([]));
        assert_eq!(chunks[2]["usage"]["completion_tokens"], 47);
        assert_eq!(
            chunks[2]["usage"]["prompt_tokens_details"]["cached_tokens"],
            10
        );
        assert_eq!(String::from_utf8(out).unwrap().matches("[DONE]").count(), 1);
    }
}
#[test]
fn tool_indices_finish_only_at_terminal_and_trailing_usage() {
    let mut stream = StreamTranslator::new("g", true);
    let mut out = Vec::new();
    for city in ["Paris", "Rome"] {
        out.extend(stream.feed(&sse(json!({"candidates":[{"index":2,"content":{"parts":[{"functionCall":{"name":"weather","args":{"city":city}}}]}}]}))).unwrap());
    }
    out.extend(
        stream
            .feed(&sse(
                json!({"candidates":[{"index":2,"finishReason":"STOP"}]}),
            ))
            .unwrap(),
    );
    out.extend(stream.feed(&sse(json!({"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}))).unwrap());
    out.extend(stream.finish().unwrap());
    let chunks = events(&out);
    let first = &chunks[0]["choices"][0]["delta"]["tool_calls"][0];
    let second = &chunks[1]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(first["index"], 0);
    assert_eq!(second["index"], 1);
    assert_ne!(first["id"], second["id"]);
    assert_eq!(
        serde_json::from_str::<Value>(first["function"]["arguments"].as_str().unwrap()).unwrap(),
        json!({"city":"Paris"})
    );
    assert!(chunks[0]["choices"][0]["finish_reason"].is_null());
    assert_eq!(chunks[2]["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(chunks[3]["usage"]["total_tokens"], 15);
}
#[test]
fn errors_never_become_success_and_poison_stream() {
    for value in [
        json!({"error":{"code":429,"message":"secret"}}),
        json!({"promptFeedback":{"blockReason":"SAFETY"}}),
    ] {
        assert!(chat_response(&value, "g").is_err());
        let mut stream = StreamTranslator::new("g", false);
        let error = stream.feed(&sse(value)).unwrap_err().to_string();
        assert!(!error.contains("secret"));
        assert!(stream.finish().is_err());
    }
    for reason in ["", "FINISH_REASON_UNSPECIFIED", "OTHER"] {
        assert!(chat_response(&json!({"candidates":[{"finishReason":reason}]}), "g").is_err());
    }
    let mut stream = StreamTranslator::new("g", false);
    stream
        .feed(&sse(
            json!({"candidates":[{"content":{"parts":[{"text":"partial"}]}}]}),
        ))
        .unwrap();
    assert!(stream.finish().is_err());
    assert!(stream.feed(b"data: [DONE]\n\n").is_err());
    let mut malformed = StreamTranslator::new("g", false);
    assert!(malformed.feed(b"data: {bad}\n\n").is_err());
    assert!(malformed.finish().is_err());
}
#[test]
fn explicit_unsupported_media_and_invalid_tool_arguments() {
    for part in [
        json!({"type":"image_url","image_url":{"url":"https://example.com/a.png"}}),
        json!({"type":"input_audio","input_audio":{"data":"AA==","format":"wav"}}),
    ] {
        assert!(
            chat_request(&json!({"messages":[{"role":"user","content":[part]}]}), "g").is_err()
        );
    }
    assert!(chat_response(&json!({"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"AA=="}}]},"finishReason":"STOP"}]}), "g").is_err());
    assert!(chat_request(&json!({"messages":[{"role":"assistant","tool_calls":[{"type":"function","id":"a","function":{"name":"x","arguments":"not json"}}]}]}), "g").is_err());
}
#[test]
fn response_tools_signatures_and_unsuccessful_terminal() {
    let body = json!({"candidates":[{"content":{"parts":[
        {"functionCall":{"name":"weather","args":{"city":"Paris"}},"thoughtSignature":"signature"},
        {"functionCall":{"name":"clock","args":{}}}]},"finishReason":"STOP"}]});
    let response = chat_response(&body, "g").unwrap();
    let choice = &response["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    let calls = &choice["message"]["tool_calls"];
    assert_eq!(
        calls[0]["extra_content"]["google"]["thought_signature"],
        "signature"
    );
    assert!(calls[0].get("index").is_none());
    let request = chat_request(
        &json!({"messages":[
        {"role":"user","content":"weather and time"}, choice["message"].clone(),
        {"role":"tool","tool_call_id":calls[0]["id"],"content":"sunny"},
        {"role":"tool","tool_call_id":calls[1]["id"],"content":"noon"}]}),
        "g",
    )
    .unwrap();
    assert_eq!(request["contents"][2]["parts"].as_array().unwrap().len(), 2);
    for (reason, expected) in [("MAX_TOKENS", "length"), ("SAFETY", "content_filter")] {
        let mut changed = body.clone();
        changed["candidates"][0]["finishReason"] = json!(reason);
        assert_eq!(
            chat_response(&changed, "g").unwrap()["choices"][0]["finish_reason"],
            expected
        );
        let mut stream = StreamTranslator::new("g", false);
        let out = stream.feed(&sse(changed)).unwrap();
        assert_eq!(events(&out)[0]["choices"][0]["finish_reason"], expected);
        stream.finish().unwrap();
    }
}
#[test]
fn multiline_sse_and_incomplete_candidates() {
    let mut stream = StreamTranslator::new("g", false);
    let out = stream
        .feed(b"event: message\ndata: {\ndata: \"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n")
        .unwrap();
    assert_eq!(events(&out)[0]["choices"][0]["finish_reason"], "stop");
    stream.finish().unwrap();
    let mut incomplete = StreamTranslator::new("g", false);
    incomplete.feed(&sse(json!({"candidates":[{"index":0,"finishReason":"STOP"}, {"index":1,"content":{"parts":[{"text":"partial"}]}}]}))).unwrap();
    assert!(incomplete.finish().is_err());
    let mut continued = StreamTranslator::new("g", false);
    continued
        .feed(&sse(json!({"candidates":[{"finishReason":"STOP"}]})))
        .unwrap();
    assert!(
        continued
            .feed(&sse(
                json!({"candidates":[{"content":{"parts":[{"text":"too late"}]}}]})
            ))
            .is_err()
    );
}
#[test]
fn unsupported_signature_only_and_incremental_calls_fail_closed() {
    for part in [
        json!({"thoughtSignature":"opaque"}),
        json!({"functionCall":{"name":"run","partialArgs":[],"willContinue":true}}),
    ] {
        let body = json!({"candidates":[{"content":{"parts":[part]},"finishReason":"STOP"}]});
        assert!(chat_response(&body, "g").is_err());
        let mut stream = StreamTranslator::new("g", false);
        assert!(stream.feed(&sse(body)).is_err());
        assert!(stream.finish().is_err());
    }
}

#[test]
fn tool_choices_and_schema_carriers_match_provider_semantics() {
    let base = json!({"messages":[{"role":"user","content":"run"}],
        "tools":[{"type":"function","function":{"name":"run","parameters":{
            "type":"object","properties":{"count":{"type":"integer","enum":[1,2]},
            "pattern":{"type":"string","pattern":"^[a-z]+$"}},"required":["count","missing"],"additionalProperties":false}}}]});
    for (choice, mode) in [
        (json!("auto"), "AUTO"),
        (json!("none"), "NONE"),
        (json!("required"), "ANY"),
        (json!({"type":"function","function":{"name":"run"}}), "ANY"),
    ] {
        let mut body = base.clone();
        body["tool_choice"] = choice;
        let out = chat_request(&body, "g").unwrap();
        assert_eq!(out["toolConfig"]["functionCallingConfig"]["mode"], mode);
        let schema = &out["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"];
        assert_eq!(schema["properties"]["count"]["type"], "string");
        assert_eq!(schema["properties"]["count"]["enum"], json!(["1", "2"]));
        assert_eq!(schema["properties"]["pattern"]["pattern"], "^[a-z]+$");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["count"]));
        let out = antigravity_chat_request(&body, "g").unwrap();
        assert_eq!(
            out["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["properties"]["count"]
                ["type"],
            "integer"
        );
    }
    for choice in [
        json!("unknown"),
        json!({"type":"function","function":{"name":"missing"}}),
    ] {
        let mut body = base.clone();
        body["tool_choice"] = choice;
        assert!(chat_request(&body, "g").is_err());
    }
}

#[test]
fn terminal_reasons_metadata_multicandidate_and_no_usage() {
    for (reason, expected) in [
        ("STOP", "stop"),
        ("MAX_TOKENS", "length"),
        ("SAFETY", "content_filter"),
        ("MALFORMED_FUNCTION_CALL", "content_filter"),
    ] {
        let response = json!({"responseId":"r","createTime":"2026-01-01T00:00:00Z","modelVersion":"native",
            "candidates":[{"index":0,"finishReason":reason},{"index":1,"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":0}});
        let full = chat_response(&response, "fallback").unwrap();
        assert_eq!(full["choices"][0]["finish_reason"], expected);
        assert_eq!(full["model"], "native");
        assert_eq!(full["created"], 1767225600i64);
        let mut stream = StreamTranslator::new("fallback", false);
        let mut out = stream.feed(&sse(response)).unwrap();
        out.extend(stream.feed(b"data: [DONE]\n\n").unwrap());
        assert!(stream.finish().unwrap().is_empty());
        let chunks = events(&out);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0]["choices"][0]["finish_reason"], expected);
        assert!(chunks.iter().all(|c| c.get("usage").is_none()));
    }
}
