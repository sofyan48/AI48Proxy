use super::*;
use crate::{config::Config, home::client::Selection};
use axum::http::HeaderMap;

fn selection() -> Selection {
    serde_json::from_value(json!({"provider":"openai","model":"selected","auth_index":"test",
        "auth":{"attributes":{"api_key":"synthetic-key","base_url":"http://127.0.0.1:9/tenant/v1"}}})).unwrap()
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
        assert!(images_disabled(&serde_yaml::from_str(raw).unwrap()));
    }
    for raw in [
        "disable-image-generation: false",
        "disable-image-generation: chat",
        "disable-image-generation: passthrough",
    ] {
        assert!(!images_disabled(&serde_yaml::from_str(raw).unwrap()));
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
