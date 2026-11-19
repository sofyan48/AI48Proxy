use super::*;
use crate::{config::Config, home::client::Selection};
use upstream::Format;

pub(super) fn config(extra: &str) -> Config {
    Config::parse(
        format!("credential-concurrency:\n  lifecycle-config-revision: 1\n{extra}").as_bytes(),
    )
    .unwrap()
}

pub(super) fn selection(provider: &str) -> Selection {
    Selection {
        model: "resolved-model".into(),
        provider: provider.into(),
        auth_index: "index".into(),
        user_api_key: "caller-secret".into(),
        model_info: None,
        concurrency: None,
        request_retry: 0,
        auth: json!({"id":"credential", "attributes":{
            "api_key":"upstream-secret", "base_url":"https://trusted.invalid/v1"}}),
    }
}

#[test]
fn known_rejection_resolves_admission_without_fencing() {
    let cancellation = tokio_util::sync::CancellationToken::new();
    let registry = crate::execution::Registry::new(cancellation.clone());
    let mut admission = registry.admission();
    let err = anyhow::Error::new(crate::home::client::Rejection(json!({
        "error":{"type":"invalid_credential", "message":"secret"}
    })))
    .context("dispatch context");
    assert!(lifecycle::resolve_rejection(&mut admission, &err));
    assert_eq!(registry.pending_admissions(), 0);
    drop(admission);
    assert!(!cancellation.is_cancelled());
    assert!(registry.is_drained());
    assert_eq!(
        routes::dispatch_error(&err).status(),
        StatusCode::UNAUTHORIZED
    );
}

#[test]
fn ambiguous_failure_keeps_admission_fenced_on_drop() {
    let cancellation = tokio_util::sync::CancellationToken::new();
    let registry = crate::execution::Registry::new(cancellation.clone());
    let mut admission = registry.admission();
    let err = anyhow::anyhow!("transport secret");
    assert!(!lifecycle::resolve_rejection(&mut admission, &err));
    assert_eq!(registry.pending_admissions(), 1);
    drop(admission);
    assert!(cancellation.is_cancelled());
    assert_eq!(
        routes::dispatch_error(&err).status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[test]
fn home_buckets_become_a_sorted_deduplicated_openai_list() {
    let models = openai_models(&json!({
        "claude":[{"id":"shared", "owned_by":"anthropic", "created":42},
            {"id":"claude-model", "display_name":"not part of list contract"}],
        "openai":[{"id":"shared"}, {"id":"gpt-model", "owned_by":"openai"}]
    }))
    .unwrap();
    assert_eq!(models["object"], "list");
    let data = models["data"].as_array().unwrap();
    assert_eq!(data.len(), 3);
    assert_eq!(
        data[0],
        json!({"id":"claude-model", "object":"model", "created":0,"owned_by":"claude"})
    );
    assert_eq!(data[1]["id"], "gpt-model");
    assert_eq!(
        data[2],
        json!({"id":"shared", "object":"model", "created":42,"owned_by":"anthropic"})
    );
    assert_eq!(
        openai_models(&json!({})).unwrap(),
        json!({"object":"list", "data":[]})
    );
    for malformed in [json!([]), json!({"openai":{}}), json!({"openai":[{}]})] {
        assert!(openai_models(&malformed).is_none());
    }
}

#[test]
fn selected_retry_limit_overrides_global_in_both_directions() {
    let global = config("request-retry: 4\n");
    let mut selected = selection("custom");
    assert!(!routes::can_retry(
        0,
        selected.request_retry,
        "credential",
        0,
        0
    ));
    assert_eq!(global.request_retry, 4);
    selected.request_retry = 6;
    assert!(routes::can_retry(
        4,
        selected.request_retry,
        "credential",
        0,
        0
    ));
    assert!(!routes::can_retry(
        6,
        selected.request_retry,
        "credential",
        0,
        0
    ));
    assert!(!routes::can_retry(0, 6, "credential", 0, 1));
    assert!(routes::can_retry(0, 6, "credential", 0, 2));
    assert!(!routes::can_retry(0, 6, "", 0, 0));
    assert!(!routes::can_retry(u32::MAX, u32::MAX, "credential", 0, 0));
}

#[test]
fn home_rejections_have_safe_http_statuses() {
    for (kind, status) in [
        ("invalid_credential", 401),
        ("no_credentials", 401),
        ("permission_denied", 403),
        ("credential_model_concurrency_exceeded", 429),
        ("model_cooldown", 429),
        ("request_retry_exceeded", 503),
        ("internal_error", 500),
    ] {
        let err = anyhow::Error::new(crate::home::client::Rejection(json!({
            "error":{"type":kind,"message":"secret"}
        })));
        assert_eq!(routes::dispatch_error(&err).status().as_u16(), status);
    }
}

#[test]
fn caller_credentials_are_home_scoped_and_never_upstream() {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer caller-secret".parse().unwrap());
    headers.insert("x-upstream-url", "http://attacker.invalid".parse().unwrap());
    let scoped = auth_headers(&headers);
    assert_eq!(scoped["authorization"], json!(["Bearer caller-secret"]));
    assert!(scoped.get("x-upstream-url").is_none());
    let prepared = upstream::prepare(
        &config(""),
        &selection("custom-compat"),
        "/v1/chat/completions",
        &headers,
        json!({"model":"alias"}),
        false,
    )
    .unwrap();
    let request = prepared.request.build().unwrap();
    assert_eq!(
        request.url().as_str(),
        "https://trusted.invalid/v1/chat/completions"
    );
    assert_eq!(request.headers()["authorization"], "Bearer upstream-secret");
    assert!(!request.headers().contains_key("x-upstream-url"));
    let payload: Value =
        serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(payload["model"], "resolved-model");
}

#[test]
fn payload_barrier_can_override_model_stream_and_filter_fields() {
    let config = config(
        "payload:\n  override:\n    - models: [{name: resolved-model, protocol: openai}]\n      params: {model: configured-model, stream: true}\n  filter:\n    - models: [{name: resolved-model}]\n      params: [secret_field]\n",
    );
    let prepared = upstream::prepare(
        &config,
        &selection("custom"),
        "/v1/chat/completions",
        &HeaderMap::new(),
        json!({"model":"alias", "stream":false,"secret_field":1}),
        false,
    )
    .unwrap();
    assert!(prepared.stream);
    let request = prepared.request.build().unwrap();
    let value: Value = serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(value["model"], "configured-model");
    assert!(value.get("secret_field").is_none());
}

#[test]
fn unsupported_provider_protocols_fail_without_guessing_credentials() {
    for provider in ["claude", "gemini", "codex", "antigravity", "gemini-cli"] {
        assert!(
            upstream::prepare(
                &config(""),
                &selection(provider),
                "/v1/chat/completions",
                &HeaderMap::new(),
                json!({"model":"alias"}),
                false
            )
            .is_err()
        );
    }
    let mut oauth = selection("custom");
    oauth.auth["attributes"]["api_key"] = json!("");
    oauth.auth["metadata"] = json!({"access_token":"oauth-secret"});
    assert!(
        upstream::prepare(
            &config(""),
            &oauth,
            "/v1/responses",
            &HeaderMap::new(),
            json!({"model":"alias","input":"hello"}),
            false
        )
        .is_ok()
    );
}

#[test]
fn unsupported_proxy_does_not_fall_back_to_direct() {
    let config = config("proxy-url: socks5://localhost:1234\n");
    assert!(
        upstream::prepare(
            &config,
            &selection("custom"),
            "/v1/responses",
            &HeaderMap::new(),
            json!({"model":"alias","input":"hello"}),
            false
        )
        .is_err()
    );
    let mut direct = selection("custom");
    direct.auth["proxy_url"] = json!("direct");
    assert!(
        upstream::prepare(
            &config,
            &direct,
            "/v1/responses",
            &HeaderMap::new(),
            json!({"model":"alias","input":"hello"}),
            false
        )
        .is_ok()
    );
}

#[test]
fn fragmented_crlf_sse_preserves_authoritative_usage_without_guessing() {
    let mut usage = usage::Usage::new(Format::OpenAi);
    for byte in b"data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4,\"total_tokens\":19}}\r\n\r\ndata: [DONE]\n\n" {
        usage.feed(&[*byte]);
    }
    let breakdown = usage.breakdown();
    assert_eq!(breakdown.total_tokens, 19);
    assert_eq!(breakdown.quality, "inconsistent");
    assert_eq!(breakdown.unclassified_tokens, 19);
    assert_eq!(breakdown.input.total_tokens, 0);
    assert!(breakdown.valid());
}

#[test]
fn claude_partial_usage_merges_and_gemini_never_sums_overlapping_tokens() {
    let mut usage = usage::Usage::new(Format::Claude);
    usage.json(&json!({"message":{"usage":{"input_tokens":8,"cache_read_input_tokens":3}}}));
    usage.json(&json!({"usage":{"output_tokens":5}}));
    assert_eq!(usage.breakdown().total_tokens, 16);
    let mut usage = usage::Usage::new(Format::Gemini);
    usage.json(
        &json!({"usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":7,
        "thoughtsTokenCount":10,"totalTokenCount":37}}),
    );
    assert_eq!(usage.breakdown().unclassified_tokens, 0);
    assert_eq!(usage.breakdown().output.reasoning_tokens, 10);
    assert_eq!(usage.breakdown().input.total_tokens, 20);
    usage.json(&json!({"usageMetadata":{"totalTokenCount":-1}}));
    assert!(usage.breakdown().valid());
}

#[test]
fn oversized_sse_event_is_bounded_and_parser_recovers() {
    let mut usage = usage::Usage::new(Format::OpenAi);
    usage.feed(b"data: ");
    usage.feed(&vec![b'x'; 300 * 1024]);
    usage.feed(b"\n\ndata: {\"usage\":{\"total_tokens\":12}}\n\n");
    assert_eq!(usage.breakdown().total_tokens, 12);
}

#[tokio::test]
async fn public_errors_never_echo_home_secret_messages() {
    use http_body_util::BodyExt;
    let response = home_error(&json!({"error":{"type":"invalid_credential",
        "message":"sensitive upstream-secret"}}));
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(!String::from_utf8_lossy(&body).contains("upstream-secret"));
}
