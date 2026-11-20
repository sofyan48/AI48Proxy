use super::*;
use crate::{config::Config, home::client::Selection};
use axum::http::HeaderMap;

fn selection() -> Selection {
    serde_json::from_value(json!({"provider":"openai","model":"selected","auth_index":"test",
        "auth":{"attributes":{"api_key":"synthetic-key","base_url":"http://127.0.0.1:9/tenant/v1"}}})).unwrap()
}

#[test]
fn video_media_counts_match_go_completed_output_and_duration_precedence() {
    let request = json!({"duration":5.5});
    assert!(provider::video::duration_seconds(&request, &json!({"request_id":"job"})).is_none());
    for (response, seconds) in [
        (json!({"video":{"duration":6.25}}), Some(6.25)),
        (
            json!({"usage":{},"duration":2,"video":{"duration":7}}),
            Some(2.0),
        ),
        (json!({"status":" COMPLETED "}), Some(5.5)),
        (json!({"url":"private-url","duration":"9"}), Some(5.5)),
        (
            json!({"data":[],"duration":0,"video":{"duration":9}}),
            Some(5.5),
        ),
    ] {
        assert_eq!(
            provider::video::duration_seconds(&request, &response),
            seconds
        );
    }
    for duration in [json!(0), json!(-1), json!("5"), Value::Null] {
        assert!(
            provider::video::duration_seconds(&json!({"duration":duration}), &json!({"video":{}}))
                .is_none()
        );
    }
}

#[test]
fn payload_configuration_is_final_for_json_and_multipart() {
    let config = Config::parse(b"credential-concurrency:\n  lifecycle-config-revision: 1\npayload:\n  override:\n    - models:\n        - name: selected\n          protocol: openai\n      params:\n        model: final-model\n        quality: high\n  filter:\n    - models:\n        - name: selected\n          protocol: openai\n      params: [prompt]\n").unwrap();
    let original = json!({"model":"alias","prompt":"private","quality":"low"});
    for multipart in [false, true] {
        let form = multipart::Form { parts: Vec::new() };
        let mut body = original.clone();
        let request = provider::prepare(
            &config,
            &selection(),
            "/v1/images/generations",
            &HeaderMap::new(),
            &mut body,
            &original,
            if multipart { Some(&form) } else { None },
        )
        .unwrap()
        .build()
        .unwrap();
        let bytes = request.body().unwrap().as_bytes().unwrap();
        let payload: Value = if multipart {
            multipart::Form::parse(request.headers()["content-type"].to_str().unwrap(), bytes)
                .unwrap()
                .fields()
                .unwrap()
        } else {
            serde_json::from_slice(bytes).unwrap()
        };
        assert_eq!(payload["model"], "final-model");
        assert_eq!(payload["quality"], "high");
        assert!(payload.get("prompt").is_none());
        assert_eq!(request.url().path(), "/tenant/v1/images/generations");
    }
}

#[test]
fn config_image_gate_matches_go_boolean_and_nested_v8_forms() {
    for raw in [
        "disable-image-generation: true",
        "multimedia:\n  disable-image-generation: yes",
    ] {
        let yaml = format!("credential-concurrency:\n  lifecycle-config-revision: 1\n{raw}");
        assert_eq!(
            Config::parse(yaml.as_bytes())
                .unwrap()
                .disable_image_generation,
            crate::config::multimedia::ImageGenerationMode::All
        );
    }
    for raw in [
        "disable-image-generation: false",
        "disable-image-generation: chat",
        "disable-image-generation: passthrough",
    ] {
        let yaml = format!("credential-concurrency:\n  lifecycle-config-revision: 1\n{raw}");
        assert_ne!(
            Config::parse(yaml.as_bytes())
                .unwrap()
                .disable_image_generation,
            crate::config::multimedia::ImageGenerationMode::All
        );
    }
}

#[test]
fn codex_direct_and_bridge_payload_barriers() {
    let config = Config::parse(b"credential-concurrency:\n  lifecycle-config-revision: 1\ngpt-image-2-base-model: gpt-5.5\npayload:\n  override:\n    - models:\n        - name: '*'\n      params:\n        model: final-model\n  filter:\n    - models:\n        - name: '*'\n      params: [prompt]\n").unwrap();
    let mut selected = selection();
    selected.provider = "codex".into();
    selected.model = "gpt-image-2".into();
    let raw = b"--test\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nedit\r\n--test\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n\0\xff\r\n--test\r\nContent-Disposition: form-data; name=\"mask\"; filename=\"mask.png\"\r\nContent-Type: image/png\r\n\r\nmask\r\n--test--\r\n";
    let form = multipart::Form::parse("multipart/form-data; boundary=test", raw).unwrap();
    for direct in [true, false] {
        selected.model = if direct {
            "gpt-image-2"
        } else {
            "custom-image"
        }
        .into();
        let original = json!({"model":"alias","prompt":"edit","response_format":"url"});
        let mut payload =
            provider::codex::payload(&config, &selected, &original, Some(&form), true).unwrap();
        if direct {
            assert_eq!(
                payload["images"][0]["image_url"],
                "data:image/png;base64,AP8="
            );
            assert_eq!(
                payload["mask"]["image_url"],
                "data:image/png;base64,bWFzaw=="
            );
        } else {
            assert_eq!(payload["model"], "gpt-5.5");
            assert_eq!(
                payload["tools"][0]["input_image_mask"]["image_url"],
                "data:image/png;base64,bWFzaw=="
            );
            assert_eq!(
                payload["input"][0]["content"][1]["image_url"],
                "data:image/png;base64,AP8="
            );
        }
        let request = provider::prepare(
            &config,
            &selected,
            "/v1/images/edits",
            &HeaderMap::new(),
            &mut payload,
            &original,
            None,
        )
        .unwrap()
        .build()
        .unwrap();
        assert_eq!(
            request.url().path(),
            if direct {
                "/tenant/v1/images/edits"
            } else {
                "/tenant/v1/responses"
            }
        );
        let final_body: Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(final_body["model"], "final-model");
        assert!(final_body.get("prompt").is_none());
    }
}

#[test]
fn multipart_filtered_files_are_not_restored() {
    let form = multipart::Form {
        parts: vec![multipart::Part {
            name: "image".into(),
            filename: Some("image.png".into()),
            content_type: "image/png".into(),
            bytes: vec![0, 255],
        }],
    };
    let mut view = form.payload(&json!({"model":"selected"}));
    assert!(view.get("image").is_some());
    view.as_object_mut().unwrap().remove("image");
    let (bytes, mime) = form.encode(&view).unwrap();
    assert!(
        multipart::Form::parse(&mime, &bytes)
            .unwrap()
            .images()
            .is_empty()
    );
}

#[test]
fn image_tool_policy_applies_to_http_and_native_ws_before_overrides() {
    use crate::gateway::upstream;
    let mut selected = selection();
    selected.provider = "codex".into();
    selected.model = "gpt-5.4".into();
    for mode in ["false", "true", "chat", "passthrough"] {
        let yaml = format!(
            "credential-concurrency:\n  lifecycle-config-revision: 1\ndisable-image-generation: {mode}\n"
        );
        let config = Config::parse(yaml.as_bytes()).unwrap();
        let body = json!({"model":"alias","input":"hello","tools":[{"type":"image_generation"}],"tool_choice":{"type":"image_generation"}});
        let http = upstream::prepare(
            &config,
            &selected,
            "/v1/responses",
            &HeaderMap::new(),
            body.clone(),
            false,
        )
        .unwrap()
        .request
        .build()
        .unwrap();
        let ws = crate::providers::codex::websocket::prepare(
            &config,
            &selected,
            &HeaderMap::new(),
            body,
        )
        .unwrap();
        for request in [http, ws] {
            let payload: Value =
                serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
            let stripped = matches!(mode, "true" | "chat");
            assert_eq!(payload["tools"].as_array().unwrap().is_empty(), stripped);
            assert_eq!(payload.get("tool_choice").is_none(), stripped);
        }
        let body = json!({"model":"alias","input":"hello"});
        let request = upstream::prepare(
            &config,
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
        let payload: Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(payload.get("tools").is_some(), mode == "false");
    }
    let config = Config::parse(b"credential-concurrency:\n  lifecycle-config-revision: 1\ndisable-image-generation: chat\npayload:\n  override:\n    - models:\n        - name: '*'\n      params:\n        tools: [{type: image_generation}]\n").unwrap();
    let request = upstream::prepare(
        &config,
        &selected,
        "/v1/chat/completions",
        &HeaderMap::new(),
        json!({"model":"alias","messages":[{"role":"user","content":"draw"}]}),
        false,
    )
    .unwrap()
    .request
    .build()
    .unwrap();
    let payload: Value =
        serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(payload["tools"][0]["type"], "image_generation");
}

#[test]
fn native_speech_models_errors_and_limits() {
    for model in [
        "tts-1-hd",
        "gpt-4o-mini-tts",
        "xai/grok-tts(high)",
        "grok/grok-voice-tts-1.0",
    ] {
        assert!(speech::prepare(&json!({"model":model,"text":"hello"})).is_ok());
    }
    assert!(speech::prepare(&json!({"input":"a".repeat(60001)})).is_err());
    assert!(speech::model_unavailable(
        &json!({"error":{"message":"model is unavailable"}})
    ));
    assert!(!speech::model_unavailable(
        &json!({"error":{"message":"unknown voice"}})
    ));
}
