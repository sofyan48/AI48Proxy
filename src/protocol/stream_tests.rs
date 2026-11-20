use super::*;
fn fixture(from: Format, events: &[Value], done: bool) -> (StreamTranslator, String) {
    let mut wire = events
        .iter()
        .map(|e| format!("data: {e}\r\n\r\n"))
        .collect::<String>();
    if done {
        wire.push_str("data: [DONE]\n\n");
    }
    let mut translator = StreamTranslator::new(from, "alias", 42);
    let mut result = Vec::new();
    for byte in wire.bytes() {
        result.extend(translator.feed(&[byte]).unwrap());
    }
    (translator, String::from_utf8(result).unwrap())
}
#[test]
fn fragmented_chat_text_tools_usage_to_claude() {
    let (t, out) = fixture(
        Format::OpenAiChat,
        &[
            json!({"id":"r","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"},"finish_reason":null}]}),
            json!({"id":"r","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"f","arguments":"{"}}]},"finish_reason":null}]}),
            json!({"id":"r","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"id":"r","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14,"prompt_tokens_details":{"cached_tokens":3},"completion_tokens_details":{"reasoning_tokens":1}}}),
        ],
        true,
    );
    t.finish().unwrap();
    assert!(out.contains("text_delta"));
    assert!(out.contains("input_json_delta"));
    assert!(out.contains("\"input_tokens\":7"));
    assert!(out.contains("\"cache_read_input_tokens\":3"));
    assert!(out.ends_with("data: {\"type\":\"message_stop\"}\n\n"));
}
#[test]
fn fragmented_claude_text_tools_and_partial_usage_to_chat() {
    let (t, out) = fixture(
        Format::ClaudeMessages,
        &[
            json!({"type":"message_start","message":{"id":"r","usage":{"input_tokens":7,"cache_read_input_tokens":3,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"c","name":"f","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}),
            json!({"type":"message_stop"}),
        ],
        false,
    );
    t.finish().unwrap();
    assert!(out.contains("\"content\":\"hello\""));
    assert!(out.contains("\"arguments\":\"{}\""));
    assert!(out.contains("\"prompt_tokens\":10"));
    assert!(out.contains("\"total_tokens\":14"));
    assert!(out.ends_with("data: [DONE]\n\n"));
}
#[test]
fn claude_thinking_blocks_stream_as_reasoning_content() {
    let (t, out) = fixture(
        Format::ClaudeMessages,
        &[
            json!({"type":"message_start","message":{"id":"r","usage":{"input_tokens":7,"output_tokens":2}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"ping"}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":null}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"ponder"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque-sig"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"hey"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
            json!({"type":"message_stop"}),
        ],
        false,
    );
    t.finish().unwrap();
    assert!(out.contains("\"reasoning_content\":\"ponder\""));
    assert!(out.contains("\"content\":\"hey\""));
    assert!(!out.contains("opaque"));
    assert!(out.ends_with("data: [DONE]\n\n"));
}
#[test]
fn truncated_unsupported_and_oversized_streams_fail() {
    let (t, _) = fixture(
        Format::OpenAiChat,
        &[json!({"id":"r","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]})],
        false,
    );
    assert!(t.finish().is_err());
    let mut t = StreamTranslator::new(Format::ClaudeMessages, "m", 0);
    assert!(t.feed(b"data: {\"type\":\"error\"}\n\n").is_err());
    assert!(t.feed(&vec![b'x'; 300 * 1024]).is_err());
}
#[test]
fn usage_conversion_preserves_absence_and_explicit_zero() {
    assert_eq!(
        usage::mapped_usage(Format::ClaudeMessages, &json!({})).unwrap(),
        json!({})
    );
    assert_eq!(
        usage::mapped_usage(
            Format::ClaudeMessages,
            &json!({"input_tokens":0,"output_tokens":0})
        )
        .unwrap(),
        json!({"prompt_tokens":0,"completion_tokens":0,"total_tokens":0})
    );
}
