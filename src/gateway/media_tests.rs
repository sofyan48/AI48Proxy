use super::*;
#[path = "media_fixture.rs"]
mod fixture;

#[tokio::test]
async fn compatible_generation_uses_selected_model_and_reports_tokens() {
    let result = fixture::run("openai", "/v1/images/generations",
        json!({"model":"public-alias","prompt":"draw","response_format":"url"}),
        "application/json", r#"{"created":12,"data":[{"b64_json":"AA=="}],"usage":{"input_tokens":3,"output_tokens":7,"total_tokens":10}}"#).await;
    assert_eq!(result.status, StatusCode::OK);
    assert_eq!(result.upstream.target, "/tenant/v1/images/generations");
    assert_eq!(result.upstream.body["model"], "resolved-model");
    assert!(result.upstream.body.get("stream").is_none());
    assert!(result.body.contains("data:image/png;base64,AA=="));
    assert_eq!(result.usage["tokens"]["total_tokens"], 10);
    assert_eq!(result.usage["image_count"], 1);
}

#[tokio::test]
async fn speech_alias_forwards_native_payload_and_binary_audio() {
    // Fixture dispatch alias is deliberately fixed: exercise speech normalization separately,
    // then send its accepted alias through the media-specific loopback fixture below.
    let (model, payload, mime) = speech::prepare(
        &json!({"model":"tts-1","input":" hello ","voice":"nova","response_format":"wav"}),
    )
    .unwrap();
    assert_eq!(model, "grok-tts");
    assert_eq!(payload["text"], "hello");
    assert_eq!(payload["voice_id"], "eve");
    assert_eq!(mime, "audio/wav");
    assert!(payload.get("model").is_none());
    for path in ["/v1/audio/speech", "/v1/tts"] {
        let result = fixture::run(
            "xai",
            path,
            json!({"model":"tts-1","input":" hello ","voice":"nova","response_format":"wav"}),
            "application/octet-stream",
            "RIFF\0audio",
        )
        .await;
        assert_eq!(result.status, StatusCode::OK);
        assert_eq!(result.headers["content-type"], "audio/wav");
        assert_eq!(result.body.as_bytes(), b"RIFF\0audio");
        assert_eq!(result.upstream.target, "/tenant/v1/tts");
        assert_eq!(result.upstream.body, payload);
        assert_eq!(result.usage["failed"], false);
        assert_eq!(
            result.upstream.headers["authorization"],
            format!("Bearer {}", fixture::SECRET)
        );
    }
}

#[tokio::test]
async fn multipart_edits_and_xai_generation_use_home_dispatch() {
    let raw = b"--test\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\npublic-alias\r\n--test\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nedit\r\n--test\r\nContent-Disposition: form-data; name=\"image[]\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n\0\xff\r\n--test--\r\n";
    for provider in ["openai", "xai"] {
        let result = fixture::run_raw(
            provider,
            "/v1/images/edits",
            "multipart/form-data; boundary=test",
            raw.to_vec(),
            "application/json",
            r#"{"created":12,"data":[{"b64_json":"AA=="}]}"#,
        )
        .await;
        assert_eq!(result.status, StatusCode::OK);
        assert_eq!(result.upstream.target, "/tenant/v1/images/edits");
        if provider == "xai" {
            assert_eq!(result.upstream.body["model"], "grok-imagine-image");
            assert_eq!(
                result.upstream.body["image"]["url"],
                "data:image/png;base64,AP8="
            );
        } else {
            assert_eq!(result.upstream.body["model"], "resolved-model");
        }
        assert_eq!(result.usage["failed"], false);
    }
    let result = fixture::run(
        "xai",
        "/v1/images/generations",
        json!({"model":"public-alias","prompt":"draw"}),
        "application/json",
        r#"{"data":[{"url":"https://example.invalid/image"}]}"#,
    )
    .await;
    assert_eq!(result.status, StatusCode::OK);
    assert_eq!(result.upstream.body["model"], "grok-imagine-image");
    assert_eq!(result.upstream.body["aspect_ratio"], "1:1");
}

#[test]
fn multipart_roundtrip_preserves_files_masks_repetition_and_rewrites_model() {
    let raw = b"--test\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\npublic\r\n--test\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nedit\r\n--test\r\nContent-Disposition: form-data; name=\"image[]\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n\0\xff\r\n--test\r\nContent-Disposition: form-data; name=\"mask\"; filename=\"mask.png\"\r\nContent-Type: image/png\r\n\r\nmask\r\n--test--\r\n";
    let form = multipart::Form::parse("multipart/form-data; boundary=test", raw).unwrap();
    let mut fields = form.fields().unwrap();
    fields["model"] = "selected".into();
    let (encoded, content_type) = form.encode(&form.payload(&fields)).unwrap();
    let parsed = multipart::Form::parse(&content_type, &encoded).unwrap();
    assert_eq!(parsed.fields().unwrap()["model"], "selected");
    assert_eq!(parsed.images()[0].bytes, b"\0\xff");
    assert!(
        parsed
            .parts
            .iter()
            .any(|p| p.name == "mask" && p.bytes == b"mask")
    );
    assert!(multipart::Form::parse("multipart/form-data", raw).is_err());
    assert!(
        multipart::Form::parse("multipart/form-data; boundary=test", &raw[..raw.len() - 10])
            .is_err()
    );
}

#[test]
fn xai_image_normalization_and_response_formats() {
    let out = images::xai_request(
        &json!({"prompt":" cat ","size":"2048x2048","n":2}),
        "grok-imagine-image",
        None,
        false,
    )
    .unwrap();
    assert_eq!(out["prompt"], "cat");
    assert_eq!(out["resolution"], "2k");
    assert_eq!(out["aspect_ratio"], "1:1");
    assert_eq!(out["response_format"], "b64_json");
    let out = images::response(
        json!({"data":[{"url":"https://example.invalid/image"}]}),
        "b64_json",
    )
    .unwrap();
    assert!(out["data"][0].get("url").is_some());
    assert!(speech::prepare(&json!({"model":"arbitrary","input":"hello"})).is_err());
    assert!(speech::prepare(&json!({"input":"hello","response_format":"opus"})).is_err());
}

#[tokio::test]
async fn unsupported_provider_combinations_release_without_upstream_io() {
    for provider in ["claude", "gemini"] {
        fixture::rejected_provider(
            provider,
            "/v1/images/generations",
            json!({"model":"public-alias","prompt":"draw"}),
        )
        .await;
    }
    fixture::rejected_provider(
        "openai",
        "/v1/audio/speech",
        json!({"model":"tts-1","input":"hello"}),
    )
    .await;
}

#[tokio::test]
async fn codex_responses_bridge_requires_completion_and_reports_images() {
    let wire = "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"image_generation_call\",\"result\":\"AA==\",\"size\":\"1024x1536\",\"quality\":\"high\",\"output_format\":\"webp\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"created_at\":12,\"output\":[],\"tool_usage\":{\"image_gen\":{\"input_tokens\":3,\"output_tokens\":7,\"total_tokens\":10}}}}\n\n";
    for path in ["/v1/images/generations", "/v1/images/edits"] {
        let result = fixture::run("codex", path,
            json!({"model":"public-alias","prompt":"draw","response_format":"url","images":[{"image_url":"data:image/png;base64,AA=="}]}),
            "text/event-stream", wire).await;
        assert_eq!(result.status, StatusCode::OK);
        assert_eq!(result.upstream.target, "/tenant/v1/responses");
        assert_eq!(result.upstream.body["model"], "gpt-5.4-mini");
        assert_eq!(result.upstream.body["tools"][0]["model"], "public-alias");
        assert_eq!(
            result.upstream.body["tools"][0]["action"],
            if path.ends_with("edits") {
                "edit"
            } else {
                "generate"
            }
        );
        assert!(result.body.contains("data:image/webp;base64,AA=="));
        assert_eq!(result.usage["image_count"], 1);
        assert_eq!(result.usage["image_details"]["quality"], "high");
        assert_eq!(result.usage["tokens"]["total_tokens"], 10);
    }
    for wire in ["data: {}\n\n", "data: {\"type\":\"response.failed\"}\n\n"] {
        let result = fixture::run(
            "codex",
            "/v1/images/generations",
            json!({"model":"public-alias","prompt":"draw"}),
            "text/event-stream",
            wire,
        )
        .await;
        assert_eq!(result.status, StatusCode::BAD_GATEWAY);
        assert_eq!(result.usage["failed"], true);
        assert!(result.usage.get("image_count").is_none());
    }
}

#[tokio::test]
async fn video_routes_fail_closed() {
    assert_eq!(unsupported().await.status(), StatusCode::NOT_IMPLEMENTED);
}
