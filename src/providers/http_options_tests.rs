//! Fixtures transcribed from current CLIProxyAPI Go tests/helpers, not live-provider claims.
use super::*;
use serde_json::json;

fn selection(provider: &str, auth: Value) -> Selection {
    serde_json::from_value(
        json!({"provider":provider,"model":"upstream","auth_index":"fixture","auth":auth}),
    )
    .unwrap()
}
fn config() -> Config {
    Config::parse(b"credential-concurrency: {lifecycle-config-revision: 7}\n").unwrap()
}

#[test]
fn go_codex_explicit_prompt_key_session_header_precedence() {
    // Source: CLIProxyAPI/internal/runtime/executor/codex_executor_request.go:
    // cacheHelperDoc reads req.Payload.prompt_cache_key, finalizes payload, then
    // sets Session-Id; applyCodexHeadersWithSIWC uses misc.EnsureHeader (incoming
    // nonblank wins), then ApplyCustomHeadersFromAttrs (credential wins).
    for (incoming, custom, expected) in [
        (None, None, Some("synthetic-cache")),
        (Some("caller-session"), None, Some("caller-session")),
        (Some("  caller-session  "), None, Some("caller-session")),
        (Some("  "), None, Some("synthetic-cache")),
        (
            Some("caller-session"),
            Some("operator-session"),
            Some("operator-session"),
        ),
    ] {
        let mut auth = json!({"metadata":{"access_token":"fixture"}});
        if let Some(custom) = custom {
            auth["attributes"] = json!({"header:Session-Id":custom});
        }
        let selected = selection("codex", auth);
        let mut headers = HeaderMap::new();
        if let Some(incoming) = incoming {
            insert(&mut headers, "session-id", incoming).unwrap();
        }
        let prepared = prepare(
            &config(),
            &selected,
            "/v1/responses",
            &headers,
            json!({"input":"hi","prompt_cache_key":"synthetic-cache"}),
            false,
        )
        .unwrap();
        let request = prepared.request.build().unwrap();
        assert_eq!(
            request
                .headers()
                .get("session-id")
                .and_then(|v| v.to_str().ok()),
            expected
        );
        assert_eq!(request.headers().get_all("session-id").iter().count(), 1);
    }
    let selected = selection("codex", json!({"metadata":{"access_token":"fixture"}}));
    for body in [
        json!({"input":"hi"}),
        json!({"input":"hi","prompt_cache_key":""}),
    ] {
        let request = prepare(
            &config(),
            &selected,
            "/v1/responses",
            &HeaderMap::new(),
            body,
            false,
        )
        .unwrap()
        .request
        .build()
        .unwrap();
        assert!(!request.headers().contains_key("session-id"));
    }
}

#[test]
fn go_codex_session_seed_precedes_payload_finalization() {
    let selected = selection("codex", json!({"metadata":{"access_token":"fixture"}}));
    for rule in [
        "  override:\n    - models: [{name: '*'}]\n      params: {prompt_cache_key: configured-cache}\n",
        "  filter:\n    - models: [{name: '*'}]\n      params: [prompt_cache_key]\n",
    ] {
        let config = Config::parse(
            format!("credential-concurrency: {{lifecycle-config-revision: 7}}\npayload:\n{rule}")
                .as_bytes(),
        )
        .unwrap();
        let request = prepare(
            &config,
            &selected,
            "/v1/responses",
            &HeaderMap::new(),
            json!({"input":"hi","prompt_cache_key":"synthetic-cache"}),
            false,
        )
        .unwrap()
        .request
        .build()
        .unwrap();
        assert_eq!(request.headers()["session-id"], "synthetic-cache");
        let body: Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        if rule.contains("override") {
            assert_eq!(body["prompt_cache_key"], "configured-cache");
        } else {
            assert!(body.get("prompt_cache_key").is_none());
        }
    }
}

#[test]
fn go_codex_hosted_tools_include_and_cache_breakpoints() {
    // codex_openai-responses_request.go: builtin aliases + required include.
    let mut body = json!({"input":[{"role":"system","content":[{"type":"input_text","text":"hi","prompt_cache_breakpoint":true}],"prompt_cache_breakpoint":true}],
        "tools":[{"type":"web_search_preview_2025_03_11"},{"type":"image_generation","model":"custom"}],
        "tool_choice":{"type":"web_search_preview"},"include":["web_search_call.action.sources","other"]});
    codex::normalize_request(&mut body, "upstream").unwrap();
    assert_eq!(body["tools"][0]["type"], "web_search");
    assert_eq!(
        body["tools"][1],
        json!({"type":"image_generation","model":"custom"})
    );
    assert_eq!(body["tool_choice"]["type"], "web_search");
    assert_eq!(
        body["include"],
        json!([
            "reasoning.encrypted_content",
            "web_search_call.action.sources"
        ])
    );
    assert_eq!(body["input"][0]["role"], "developer");
    assert!(body["input"][0].get("prompt_cache_breakpoint").is_none());
    assert!(
        body["input"][0]["content"][0]
            .get("prompt_cache_breakpoint")
            .is_none()
    );
}

#[test]
fn go_codex_input_ids() {
    // helps/codex_input_ids_test.go + codex_executor_input_ids_test.go.
    let mut body = json!({"input":[
        {"type":"reasoning","id":format!("rs_{}", "a".repeat(64)),"encrypted_content":"fixture"},
        {"type":"message","id":"item_74ec40c883248ebb4885ec84"},
        {"type":"function_call","id":"item_call","call_id":"call-1"},
        {"type":"tool_search_call","id":"ts_call+1/2","call_id":"call-2"},
        {"type":"message","id":"msg_short+id/with=chars"},
        {"type":"message","id":"msg_short_id_with_chars"},
        {"id":"b".repeat(65)}, {"id":"界".repeat(65)}]});
    codex::sanitize_input_ids(&mut body);
    assert_eq!(body["input"].as_array().unwrap().len(), 7);
    assert_eq!(body["input"][0]["id"], "msg_item_74ec40c883248ebb4885ec84");
    assert_eq!(body["input"][1]["id"], "fc_item_call");
    assert_eq!(body["input"][1]["call_id"], "call-1");
    assert_eq!(body["input"][2]["id"], "tsc_call_1_2");
    assert_ne!(body["input"][3]["id"], body["input"][4]["id"]);
    assert_eq!(body["input"][4]["id"], "msg_short_id_with_chars");
    for item in body["input"].as_array().unwrap() {
        let id = item["id"].as_str().unwrap();
        assert!(id.len() <= 64);
        assert!(
            id.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        );
    }
    let again = body.clone();
    codex::sanitize_input_ids(&mut body);
    assert_eq!(again, body);
}

#[test]
fn go_home_selected_compat_options_and_alias() {
    // openai_compat_home_options_test.go: shared upstream, distinct alias options.
    let selected = selection(
        "home-compat",
        json!({"prefix":"tenant","attributes":{"compat_name":"home-compat","api_key":"fixture","base_url":"https://example.test/v1"},
        "metadata":{"credential_options":{"models":[
            {"name":"upstream","alias":"alias-a","use-max-completion-tokens":false},
            {"name":"upstream","alias":"alias-b","use-max-completion-tokens":true,"input-modalities":["text"]}]}}}),
    );
    let mut body = json!({"max_tokens":64,"messages":[{"role":"tool","content":[{"type":"text","text":"result"},{"type":"image_url","image_url":{"url":"fixture"}}]}]});
    openai::normalize_request(&mut body, &selected, "tenant/alias-b(high)", false, true).unwrap();
    assert_eq!(body["max_completion_tokens"], 64);
    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(
        body["messages"][0]["content"],
        "result\n\n[image omitted: unsupported by upstream]"
    );
    let creds = resolve(&selected, "openai").unwrap();
    assert_eq!(creds.headers["authorization"], "Bearer fixture");
}

#[test]
fn go_max_tokens_preferred_field_wins_including_null() {
    let selected = selection(
        "openai",
        json!({"attributes":{"base_url":"https://example.test/v1"}}),
    );
    let mut body = json!({"max_tokens":null,"max_completion_tokens":512});
    openai::normalize_request(&mut body, &selected, "upstream", false, false).unwrap();
    assert_eq!(body["max_tokens"], Value::Null);
    assert!(body.get("max_completion_tokens").is_none());
    let creds = resolve(&selected, "openai").unwrap();
    assert!(!creds.headers.contains_key("authorization"));
    assert!(
        resolve(
            &selection(
                "openai",
                json!({"metadata":{"access_token":"not-a-compat-key"}})
            ),
            "openai"
        )
        .is_err()
    );
}

#[test]
fn go_compact_uses_json_and_preserves_native_fields() {
    // codex_executor_execute.go:executeCompact bypasses the streaming translator.
    for provider in ["codex", "openai-compatibility"] {
        let selected = selection(
            provider,
            json!({"attributes":{"api_key":"fixture","base_url":"https://example.test/v1"}}),
        );
        let prepared = prepare(
            &config(),
            &selected,
            "/v1/responses/compact",
            &HeaderMap::new(),
            json!({"input":[],"stream":true,"temperature":0.4,"max_output_tokens":2}),
            false,
        )
        .unwrap();
        assert_eq!(prepared.response_mode, ResponseMode::Json);
        let request = prepared.request.build().unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://example.test/v1/responses/compact"
        );
        let body: Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert!(body.get("stream").is_none());
        assert_eq!(body["temperature"], 0.4);
        assert_eq!(body["max_output_tokens"], 2);
    }
}

#[test]
fn continuation_follows_go_delete_and_send_contract() {
    // Go codex_executor_execute.go deletes previous_response_id for HTTP Codex.
    let original =
        json!({"input":[{"role":"user","content":"next"}],"previous_response_id":"resp_prior"});
    let mut body = original.clone();
    codex::normalize_request(&mut body, "upstream").unwrap();
    assert!(body.get("previous_response_id").is_none());
    assert!(body.pointer("/input/0").is_some());
    let mut compact = original;
    assert!(codex::normalize_compact(&mut compact, "upstream").is_err());
}

#[test]
fn go_header_defaults_and_credential_precedence() {
    let selected = selection(
        "codex",
        json!({"metadata":{"access_token":"fixture"},"attributes":{"header:user-agent":"credential-agent"}}),
    );
    let mut incoming = HeaderMap::new();
    insert(&mut incoming, "user-agent", "client-agent").unwrap();
    insert(&mut incoming, "originator", "client-origin").unwrap();
    let mut creds = resolve(&selected, "openai-response").unwrap();
    codex::request_headers(
        &mut creds.headers,
        &incoming,
        &config(),
        &selected.auth,
        true,
    )
    .unwrap();
    assert_eq!(creds.headers["user-agent"], "credential-agent");
    assert_eq!(creds.headers["originator"], "client-origin");
    let mut headers = HeaderMap::new();
    openai::headers(&mut headers, &HeaderMap::new()).unwrap();
    assert_eq!(headers["user-agent"], "cli-proxy-openai-compat");
}

#[test]
fn translated_responses_uses_chat_endpoint_and_context() {
    let selected = selection(
        "openai",
        json!({"attributes":{"base_url":"https://example.test/v1"}}),
    );
    let prepared = prepare(
        &config(),
        &selected,
        "/v1/responses",
        &HeaderMap::new(),
        json!({"messages":[{"role":"user","content":"hi"}]}),
        false,
    )
    .unwrap();
    assert_eq!(
        prepared.request.build().unwrap().url().as_str(),
        "https://example.test/v1/chat/completions"
    );
    assert!(
        prepare(
            &config(),
            &selected,
            "/v1/responses",
            &HeaderMap::new(),
            json!({"input":"untranslated"}),
            false
        )
        .is_err()
    );
}

#[test]
fn payload_is_final_for_compact_and_chat() {
    let cfg = Config::parse(b"credential-concurrency: {lifecycle-config-revision: 7}\npayload:\n  override:\n    - models: [{name: '*'}]\n      params: {max_tokens: 9, temperature: 0.8}\n").unwrap();
    let selected = selection(
        "openai",
        json!({"attributes":{"base_url":"https://example.test/v1"},"metadata":{"credential_options":{"models":[{"name":"upstream","use-max-completion-tokens":true}]}}}),
    );
    for path in ["/v1/chat/completions", "/v1/responses/compact"] {
        let prepared = prepare(
            &cfg,
            &selected,
            path,
            &HeaderMap::new(),
            json!({"messages":[],"max_tokens":64}),
            false,
        )
        .unwrap();
        let request = prepared.request.build().unwrap();
        let body: Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["max_tokens"], 9);
        assert_eq!(body["temperature"], 0.8);
    }
}
