//! Synthetic, offline public-API contracts derived from the current Go implementation.
//! `gap_` tests deliberately assert missing behavior, not the current broken result.
use ai48proxy::{config::Config, home::client::Selection, protocol::responses, providers};
use http::HeaderMap;
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/zed_request.json")).unwrap()
}

fn selection(provider: &str, base: &str) -> Selection {
    serde_json::from_value(json!({
        "provider":provider,"model":"synthetic-upstream","auth_index":"synthetic-credential",
        "auth":{"attributes":{"base_url":base,"api_key":"synthetic-not-a-secret"}}
    }))
    .unwrap()
}

fn prepare(provider: &str, base: &str, body: Value) -> reqwest::Request {
    let config =
        Config::parse(b"credential-concurrency: {lifecycle-config-revision: 7}\n").unwrap();
    let native = if provider == "codex" {
        responses::chat_request(&body, "synthetic-upstream").unwrap()
    } else {
        body
    };
    providers::prepare(
        &config,
        &selection(provider, base),
        if provider == "codex" {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        },
        &HeaderMap::new(),
        native,
        true,
    )
    .unwrap()
    .request
    .build()
    .unwrap()
}

fn request_body(request: &reqwest::Request) -> Value {
    serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap()
}

fn sse(event: Value) -> Vec<u8> {
    format!("data: {event}\n\n").into_bytes()
}

fn terminal(output: Value) -> Value {
    json!({"id":"resp_fixture","object":"response","status":"completed","output":output})
}

#[test]
fn zed_standard_fields_translate_and_normalize_without_network() {
    let source = fixture();
    let translated = responses::chat_request(&source, "synthetic-upstream").unwrap();
    assert_eq!(translated["tools"][0]["name"], "read_file");
    assert_eq!(
        translated["tools"][0]["parameters"],
        source["tools"][0]["function"]["parameters"]
    );
    assert_eq!(
        translated["tool_choice"],
        json!({"type":"function","name":"read_file"})
    );
    assert_eq!(translated["input"][0]["role"], "developer");
    assert_eq!(translated["input"][2]["type"], "function_call");
    assert_eq!(translated["input"][3]["type"], "function_call_output");
    assert_eq!(
        translated["input"][2]["call_id"],
        translated["input"][3]["call_id"]
    );
    assert_eq!(translated["reasoning"]["effort"], "medium");
    assert_eq!(translated["max_output_tokens"], 4096);
    assert!(translated.get("synthetic_unfamiliar_extension").is_none());
    let body = request_body(&prepare("codex", "https://codex.example.test/v1", source));
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["max_output_tokens"], 4096);
    assert!(body.get("max_tokens").is_none());
    assert!(body.get("stream_options").is_none());
}

#[test]
fn zed_openai_compat_retains_native_fields_and_unknown_extensions() {
    let source = fixture();
    let body = request_body(&prepare(
        "openai",
        "https://compat.example.test/v1",
        source.clone(),
    ));
    for key in [
        "tools",
        "tool_choice",
        "reasoning_effort",
        "max_tokens",
        "include",
        "synthetic_unfamiliar_extension",
    ] {
        assert_eq!(body[key], source[key], "{key}");
    }
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["model"], "synthetic-upstream");
}

#[test]
fn provider_base_paths_append_once_for_valid_go_style_bases() {
    for (provider, base, expected) in [
        (
            "codex",
            "https://chatgpt.com/backend-api/codex/",
            "/backend-api/codex/responses",
        ),
        ("codex", "https://codex.example.test/v1", "/v1/responses"),
        (
            "openai",
            "https://compat.example.test/v1/",
            "/v1/chat/completions",
        ),
    ] {
        assert_eq!(prepare(provider, base, fixture()).url().path(), expected);
    }
}

#[test]
fn endpoint_urls_are_not_base_urls_in_either_executor_contract() {
    // Go also appends the endpoint: this is a configuration hazard, not Rust-only parity.
    assert_eq!(
        prepare(
            "codex",
            "https://codex.example.test/v1/responses",
            fixture()
        )
        .url()
        .path(),
        "/v1/responses/responses"
    );
    assert_eq!(
        prepare(
            "openai",
            "https://compat.example.test/v1/chat/completions",
            fixture()
        )
        .url()
        .path(),
        "/v1/chat/completions/chat/completions"
    );
}

#[test]
fn explicit_strict_false_is_preserved() {
    let mut source = fixture();
    source["tools"][0]["function"]["strict"] = false.into();
    assert_eq!(
        request_body(&prepare("codex", "https://codex.example.test/v1", source))["tools"][0]["strict"],
        false
    );
}

#[test]
fn gap_chat_tools_must_default_to_non_strict_like_go() {
    // Go codex_openai_request.go:473-478 explicitly defaults omitted Chat strict to false.
    // The fixture deliberately has an optional start_line and no additionalProperties flag.
    let body = request_body(&prepare(
        "codex",
        "https://codex.example.test/v1",
        fixture(),
    ));
    assert_eq!(
        body["tools"][0].get("strict"),
        Some(&json!(false)),
        "Responses strict defaults must not change an ordinary Chat tool schema"
    );
}

#[test]
fn failure_events_and_truncated_sse_are_not_success() {
    for kind in ["error", "response.failed"] {
        let event = json!({"type":kind,"error":{"code":"server_error","message":"synthetic"}});
        assert!(providers::codex::completed_response(&event).is_err());
        assert!(
            responses::StreamTranslator::new("synthetic", true)
                .feed(&sse(event))
                .is_err()
        );
    }
    assert!(
        responses::StreamTranslator::new("synthetic", false)
            .finish()
            .is_err()
    );
    let mut translator = responses::StreamTranslator::new("synthetic", false);
    assert!(translator.feed(&sse(json!({"type":"response.incomplete","response":{
        "id":"resp_fixture","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[]
    }}))).is_ok());
    assert!(translator.finish().is_ok());
}

#[test]
fn gap_unannounced_argument_done_must_allow_output_item_done_fallback_like_go() {
    // Go codex_openai_response.go:338-342 ignores unknown argument state;
    // its output_item.done fallback then announces and emits the complete call.
    let item = json!({"type":"function_call","id":"fc_fixture","call_id":"call_fixture",
        "name":"read_file","arguments":"{\"path\":\"README.md\"}"});
    let events = [
        json!({"type":"response.created","response":{"id":"resp_fixture"}}),
        json!({"type":"response.function_call_arguments.done","output_index":0,"item_id":"fc_fixture","arguments":item["arguments"]}),
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
        json!({"type":"response.completed","response":terminal(json!([item]))}),
    ];
    let mut translator = responses::StreamTranslator::new("synthetic", true);
    let mut output = Vec::new();
    for event in events {
        output.extend(
            translator
                .feed(&sse(event.clone()))
                .unwrap_or_else(|error| {
                    panic!(
                        "Go tolerates this event ordering; rejected {}: {error}",
                        event["type"]
                    )
                }),
        );
    }
    translator.finish().unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("read_file"));
    assert!(output.contains("tool_calls"));
    assert!(output.contains("[DONE]"));
}

#[test]
fn gap_response_done_terminal_alias_must_complete_like_go() {
    // Go codex_executor_stream.go accepts response.done as a successful terminal.
    let event = json!({"type":"response.done","response":terminal(json!([]))});
    assert!(
        providers::codex::completed_response(&event)
            .unwrap()
            .is_some(),
        "response.done must not be ignored until EOF becomes a synthetic 502"
    );
}

#[test]
fn gap_completed_hosted_tool_metadata_must_not_reject_valid_text() {
    // Go nonstream translation switches over known output types and skips metadata;
    // Rust currently rejects web_search_call even beside a valid assistant message.
    let response = terminal(json!([
        {"type":"web_search_call","id":"ws_fixture","status":"completed",
            "action":{"type":"search","query":"synthetic query"}},
        {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Synthetic answer."}]}
    ]));
    let chat = responses::chat_response(&response, "synthetic")
        .expect("completed hosted-tool metadata is not a translation failure");
    assert_eq!(
        chat["choices"][0]["message"]["content"],
        "Synthetic answer."
    );
}
