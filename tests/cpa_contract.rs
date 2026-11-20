//! Source-derived Go contracts through the public provider API; never sends HTTP.
//! Protocol conversion belongs to the gateway; provider preparation requires native input.
use ai48proxy::{config::Config, home::client::Selection, providers};
use http::HeaderMap;
use serde_json::{Value, json};

fn fixtures() -> Value {
    serde_json::from_str(include_str!("fixtures/cpa_contract.json")).unwrap()
}

fn selected(key: &str) -> Selection {
    serde_json::from_value(fixtures()[key].clone()).unwrap()
}

fn config(extra: &str) -> Config {
    Config::parse(
        format!("credential-concurrency: {{lifecycle-config-revision: 7}}\n{extra}").as_bytes(),
    )
    .unwrap()
}

fn request(
    selection: &Selection,
    path: &str,
    headers: &HeaderMap,
    body: Value,
    stream: bool,
) -> reqwest::Request {
    providers::prepare(&config(""), selection, path, headers, body, stream)
        .unwrap()
        .request
        .build()
        .unwrap()
}

fn body(request: &reqwest::Request) -> Value {
    serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap()
}

#[test]
fn codex_source_derived_normalization_contracts() {
    let fixtures = fixtures();
    for case in fixtures["codex_cases"].as_array().unwrap() {
        let prepared = providers::prepare(
            &config(""),
            &selected("codex_selection"),
            "/v1/responses",
            &HeaderMap::new(),
            case["input"].clone(),
            false,
        )
        .unwrap();
        assert!(
            prepared.stream,
            "Codex nonstream callers still require upstream SSE"
        );
        assert_eq!(prepared.response_mode, providers::ResponseMode::CodexSse);
        let request = prepared.request.build().unwrap();
        assert_eq!(request.method(), "POST");
        assert_eq!(
            request.url().as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            request.headers()["authorization"],
            "Bearer synthetic-oauth-token"
        );
        assert!(request.headers()["authorization"].is_sensitive());
        assert_eq!(request.headers()["chatgpt-account-id"], "synthetic-account");
        assert_eq!(request.headers()["originator"], "codex-tui");
        assert_eq!(request.headers()["content-type"], "application/json");
        assert_eq!(request.headers()["accept"], "text/event-stream");
        assert_eq!(body(&request), case["expected"], "{}", case["name"]);
    }
}

#[test]
fn codex_api_key_wins_without_oauth_account_headers() {
    let mut selection = selected("codex_selection");
    selection.auth["attributes"] =
        json!({"api_key":"synthetic-key", "base_url":"https://codex.example.test/custom/"});
    let request = request(
        &selection,
        "/v1/responses",
        &HeaderMap::new(),
        json!({"input":"hi"}),
        false,
    );
    assert_eq!(
        request.url().as_str(),
        "https://codex.example.test/custom/responses"
    );
    assert_eq!(request.headers()["authorization"], "Bearer synthetic-key");
    assert!(!request.headers().contains_key("chatgpt-account-id"));
    assert!(!request.headers().contains_key("originator"));
}

#[test]
fn codex_forwards_turn_headers_but_not_client_authorization() {
    let mut headers = HeaderMap::new();
    for name in [
        "version",
        "x-codex-beta-features",
        "x-codex-turn-metadata",
        "x-codex-turn-state",
        "x-client-request-id",
        "x-codex-window-id",
        "thread-id",
        "session-id",
        "x-openai-internal-codex-responses-lite",
    ] {
        headers.insert(name, "synthetic-value".parse().unwrap());
    }
    headers.insert("authorization", "Bearer downstream-only".parse().unwrap());
    let request = request(
        &selected("codex_selection"),
        "/v1/responses",
        &headers,
        json!({"input":"hi"}),
        true,
    );
    for (name, value) in &headers {
        if name != "authorization" {
            assert_eq!(request.headers().get(name), Some(value), "{name}");
        }
    }
    assert_eq!(
        request.headers()["authorization"],
        "Bearer synthetic-oauth-token"
    );
}

#[test]
fn codex_specialized_paths_are_explicitly_blocked_not_claimed_supported() {
    let fixtures = fixtures();
    for input in fixtures["blocked_codex_inputs"].as_array().unwrap() {
        // Hosted tools pass through and HTTP drops previous_response_id like Go.
        if input.get("tools").is_some() || input.get("previous_response_id").is_some() {
            continue;
        }
        assert!(
            providers::prepare(
                &config(""),
                &selected("codex_selection"),
                "/v1/responses",
                &HeaderMap::new(),
                input.clone(),
                false
            )
            .is_err(),
            "{input}"
        );
    }
    for auth in [
        json!({"attributes":{"use_siwc":"true"},"metadata":{"access_token":"synthetic"}}),
        json!({"metadata":{"use_siwc":true,"access_token":"synthetic"}}),
        json!({"metadata":{"siwc":{"access_token":"synthetic"},"access_token":"synthetic"}}),
    ] {
        let mut selection = selected("codex_selection");
        selection.auth = auth;
        assert!(providers::resolve(&selection, "openai-response").is_err());
    }
}

#[test]
fn compat_native_chat_uses_selected_model_base_key_and_stream_mode() {
    for stream in [false, true] {
        let request = request(
            &selected("compat_selection"),
            "/v1/chat/completions",
            &HeaderMap::new(),
            json!({"model":"chosen","messages":[{"role":"user","content":"hi"}]}),
            stream,
        );
        assert_eq!(
            request.url().as_str(),
            "https://compat.example.test/v1/chat/completions"
        );
        assert_eq!(
            request.headers()["authorization"],
            "Bearer synthetic-api-key"
        );
        assert_eq!(body(&request)["model"], "upstream");
        assert_eq!(body(&request)["stream"], stream);
        assert_eq!(
            request.headers()["accept"],
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            }
        );
    }
}

#[test]
fn user_payload_rules_are_final_after_codex_normalization() {
    let config = config(
        "payload:\n  override:\n    - models: [{name: 'gpt-5.4', protocol: codex}]\n      params: {max_output_tokens: 3, store: true, service_tier: custom}\n  filter:\n    - models: [{name: 'gpt-5.4', protocol: codex}]\n      params: [include]\n",
    );
    let prepared = providers::prepare(
        &config,
        &selected("codex_selection"),
        "/v1/responses",
        &HeaderMap::new(),
        json!({"input":"hi","max_output_tokens":1}),
        false,
    )
    .unwrap();
    let body = body(&prepared.request.build().unwrap());
    assert_eq!(body["max_output_tokens"], 3);
    assert_eq!(body["store"], true);
    assert_eq!(body["service_tier"], "custom");
    assert!(body.get("include").is_none());
}

#[test]

fn gap_compat_selected_home_model_controls_token_field() {
    for stream in [false, true] {
        let request = request(
            &selected("compat_selection"),
            "/v1/chat/completions",
            &HeaderMap::new(),
            json!({"model":"chosen","messages":[],"max_tokens":64}),
            stream,
        );
        let body = body(&request);
        assert_eq!(body["max_completion_tokens"], 64);
        assert!(body.get("max_tokens").is_none());
    }
}

#[test]

fn gap_compat_default_prefers_max_tokens_without_duplicate_fields() {
    let mut selection = selected("compat_selection");
    selection.auth["metadata"]["credential_options"]["models"][0]["use-max-completion-tokens"] =
        false.into();
    let request = request(
        &selection,
        "/v1/chat/completions",
        &HeaderMap::new(),
        json!({"messages":[],"max_completion_tokens":64}),
        false,
    );
    assert_eq!(body(&request)["max_tokens"], 64);
    assert!(body(&request).get("max_completion_tokens").is_none());
}

#[test]
fn compat_provider_requires_gateway_translated_responses_input() {
    // End-to-end Responses conversion is covered by gateway/compat_integration_tests.
    // A direct provider call must not forward an untranslated Responses body as Chat.
    assert!(
        providers::prepare(
            &config(""),
            &selected("compat_selection"),
            "/v1/responses",
            &HeaderMap::new(),
            json!({"model":"chosen","input":"hi"}),
            false,
        )
        .is_err()
    );
}

#[test]

fn codex_prompt_cache_key_seeds_session_header() {
    let request = request(
        &selected("codex_selection"),
        "/v1/responses",
        &HeaderMap::new(),
        json!({"input":"hi","prompt_cache_key":"synthetic-cache"}),
        false,
    );
    assert_eq!(
        request
            .headers()
            .get("session-id")
            .and_then(|v| v.to_str().ok()),
        Some("synthetic-cache")
    );
    assert_eq!(body(&request)["prompt_cache_key"], "synthetic-cache");
}

#[test]
fn codex_http_continuation_matches_go_delete_without_local_store() {
    let request = request(
        &selected("codex_selection"),
        "/v1/responses",
        &HeaderMap::new(),
        json!({"input":"next","previous_response_id":"resp_prior"}),
        false,
    );
    assert!(body(&request).get("previous_response_id").is_none());
    assert_eq!(body(&request)["input"][0]["content"][0]["text"], "next");
}

#[test]
fn gap_codex_strips_nested_cache_hints() {
    let request = request(
        &selected("codex_selection"),
        "/v1/responses",
        &HeaderMap::new(),
        fixtures()["nested_cache_hints"].clone(),
        false,
    );
    let body = body(&request);
    assert!(body["input"][0].get("prompt_cache_breakpoint").is_none());
    assert!(
        body["input"][0]["content"][0]
            .get("prompt_cache_breakpoint")
            .is_none()
    );
    assert_eq!(body["input"][0]["content"][0]["text"], "hello");
}

#[test]

fn gap_codex_custom_turn_header_wins_over_incoming_header() {
    let mut selection = selected("codex_selection");
    selection.auth["attributes"] = json!({"header:x-codex-turn-state":"operator-state"});
    let mut headers = HeaderMap::new();
    headers.insert("x-codex-turn-state", "caller-state".parse().unwrap());
    let request = request(
        &selection,
        "/v1/responses",
        &headers,
        json!({"input":"hi"}),
        false,
    );
    assert_eq!(request.headers()["x-codex-turn-state"], "operator-state");
}
