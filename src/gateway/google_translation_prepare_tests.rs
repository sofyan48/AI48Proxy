//! Public-to-crate preparation fixtures for Google translation contract tests.
use super::*;
use crate::gateway::tests::{config, selection};
use serde_json::json;

pub(crate) fn prepare_fixture(
    provider: &str,
    stream: bool,
    include_usage: bool,
    payload: &str,
) -> Prepared {
    let mut selected = selection(provider);
    selected.auth["metadata"] =
        json!({"access_token":"fixture-token","project_id":"fixture-project"});
    prepare(
        &config(payload),
        &selected,
        "/v1/chat/completions",
        &HeaderMap::new(),
        json!({"model":"public-alias","messages":[{"role":"user","content":"hello"}],
            "max_tokens":32,"stream":stream,"stream_options":{"include_usage":include_usage}}),
        stream,
    )
    .unwrap()
}

#[test]
fn google_translation_prepare_routes_and_keeps_source_payload_barrier() {
    for provider in ["gemini", "vertex", "vertexai", "antigravity"] {
        for stream in [false, true] {
            let prepared = prepare_fixture(
                provider,
                stream,
                true,
                "payload:\n  override:\n    - models: [{name: resolved-model, from-protocol: openai}]\n      params: {generationConfig.maxOutputTokens: 123}\n",
            );
            assert_eq!(prepared.format, Format::Gemini);
            assert_eq!(prepared.response_model, "public-alias");
            assert_eq!(
                prepared.response_kind,
                ResponseKind::GeminiChat {
                    antigravity: provider == "antigravity",
                    include_usage: true,
                }
            );
            assert!(prepared.translation.is_none());
            assert_eq!(prepared.stream, stream);
            let request = prepared.request.build().unwrap();
            let body: Value =
                serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
            let native = if provider == "antigravity" {
                assert_eq!(body["model"], "resolved-model");
                assert_eq!(body["project"], "fixture-project");
                &body["request"]
            } else {
                &body
            };
            assert_eq!(native["contents"][0]["parts"][0]["text"], "hello");
            assert_eq!(native["generationConfig"]["maxOutputTokens"], 123);
            assert!(native.get("messages").is_none());
            assert!(native.get("stream_options").is_none());
            assert!(request.url().path().ends_with(if stream {
                ":streamGenerateContent"
            } else {
                ":generateContent"
            }));
            if stream {
                assert!(
                    request
                        .url()
                        .query_pairs()
                        .any(|(k, v)| k == "alt" && v == "sse")
                );
            }
        }
    }
}

#[test]
fn google_translation_prepare_unsupported_routes_still_fail_closed() {
    for provider in ["gemini", "vertex", "antigravity"] {
        for path in ["/v1/messages", "/v1/responses", "/v1/completions"] {
            let mut selected = selection(provider);
            selected.auth["metadata"] =
                json!({"access_token":"fixture-token","project_id":"fixture-project"});
            assert!(
                prepare(
                    &config(""),
                    &selected,
                    path,
                    &HeaderMap::new(),
                    json!({"model":"alias","messages":[{"role":"user","content":"hello"}]}),
                    false
                )
                .is_err()
            );
        }
    }
}
