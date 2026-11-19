use super::tests::{config, selection};
use super::*;

#[test]
fn dispatched_provider_selects_translation_before_final_payload_barrier() {
    for (provider, path, body, protocol, suffix) in [
        (
            "claude",
            "/v1/chat/completions",
            json!({"model":"alias","messages":[{"role":"user","content":"hello"}]}),
            "claude",
            "/v1/messages",
        ),
        (
            "openai",
            "/v1/messages",
            json!({"model":"alias","max_tokens":100,"messages":[{"role":"user","content":"hello"}]}),
            "openai",
            "/v1/chat/completions",
        ),
    ] {
        let config = config(&format!(
            "payload:\n  override:\n    - models: [{{name: resolved-model, protocol: {protocol}}}]\n      params: {{model: final-model, temperature: 0.1}}\n  filter:\n    - models: [{{name: resolved-model}}]\n      params: [max_tokens]\n"
        ));
        let prepared = upstream::prepare(
            &config,
            &selection(provider),
            path,
            &HeaderMap::new(),
            body,
            false,
        )
        .unwrap();
        assert!(prepared.translation.is_some());
        assert_eq!(prepared.response_model, "alias");
        let request = prepared.request.build().unwrap();
        assert!(request.url().path().ends_with(suffix));
        let body: Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["model"], "final-model");
        assert_eq!(body["temperature"], 0.1);
        assert!(body.get("max_tokens").is_none());
    }
}
#[test]
fn actual_usage_source_semantics_and_zero_total_are_authoritative() {
    for (format, value, input, read, write, output, reasoning) in [
        (
            upstream::Format::OpenAi,
            json!({"usage":{"prompt_tokens":20,"completion_tokens":7,"total_tokens":27,"prompt_tokens_details":{"cached_tokens":3,"cache_creation_tokens":2},"completion_tokens_details":{"reasoning_tokens":4}}}),
            20,
            3,
            2,
            7,
            4,
        ),
        (
            upstream::Format::Claude,
            json!({"usage":{"input_tokens":20,"cache_read_input_tokens":3,"cache_creation_input_tokens":2,"output_tokens":7,"output_tokens_details":{"thinking_tokens":4}}}),
            25,
            3,
            2,
            7,
            4,
        ),
        (
            upstream::Format::Gemini,
            json!({"usageMetadata":{"promptTokenCount":20,"toolUsePromptTokenCount":2,"cachedContentTokenCount":3,"candidatesTokenCount":7,"thoughtsTokenCount":4,"totalTokenCount":33}}),
            22,
            3,
            0,
            11,
            4,
        ),
    ] {
        let mut usage = usage::Usage::new(format);
        usage.json(&value);
        let b = usage.breakdown();
        assert!(b.valid());
        assert_eq!(b.quality, "complete");
        assert_eq!(
            (
                b.input.total_tokens,
                b.input.cache_read_tokens,
                b.input.cache_write_tokens,
                b.output.total_tokens,
                b.output.reasoning_tokens
            ),
            (input, read, write, output, reasoning)
        );
    }
    let mut partial = usage::Usage::new(upstream::Format::OpenAi);
    partial.json(&json!({"usage":{"input_tokens":10,"total_tokens":15}}));
    assert_eq!(partial.breakdown().input.total_tokens, 10);
    assert_eq!(partial.breakdown().unclassified_tokens, 5);
    partial.json(&json!({"usage":{"output_tokens":0,"total_tokens":0}}));
    assert_eq!(partial.breakdown().quality, "inconsistent");
    assert_eq!(partial.breakdown().total_tokens, 0);
    let mut zero = usage::Usage::new(upstream::Format::OpenAi);
    zero.json(&json!({"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}));
    assert_eq!(zero.breakdown().quality, "complete");
}
#[test]
fn cooldown_wait_obeys_selected_limit_and_max_interval() {
    let reject = |ms, limit| {
        anyhow::Error::new(crate::home::client::Rejection(
            json!({"error":{"type":"model_cooldown","retryable":true,"retry_after_ms":ms,"request_retry":limit}}),
        ))
    };
    let max = std::time::Duration::from_secs(3);
    assert_eq!(
        routes::cooldown_delay(&reject(2000, 2), 0, 0, max),
        Some(std::time::Duration::from_secs(2))
    );
    assert!(routes::cooldown_delay(&reject(4000, 2), 0, 0, max).is_none());
    assert!(routes::cooldown_delay(&reject(2000, 0), 0, 5, max).is_none());
    assert!(routes::cooldown_delay(&reject(2000, 2), 2, 5, max).is_none());
}
#[test]
fn malformed_model_buckets_and_unknown_errors_fail_closed() {
    for value in [
        json!({"openai":[{"id":" "}]}),
        json!({"openai":[{"id":12}]}),
        json!({"openai":null}),
    ] {
        assert!(openai_models(&value).is_none());
    }
    assert_eq!(
        home_error(&json!({"error":{"type":"unexpected","message":"secret"}})).status(),
        StatusCode::BAD_GATEWAY
    );
}
