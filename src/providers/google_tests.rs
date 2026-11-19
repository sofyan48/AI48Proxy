use super::*;
use serde_json::json;

fn selection(provider: &str, model: &str, auth: Value) -> Selection {
    serde_json::from_value(
        json!({"model":model,"provider":provider,"auth_index":"fixture","auth":auth}),
    )
    .unwrap()
}
fn config(extra: &str) -> Config {
    Config::parse(
        format!("credential-concurrency: {{lifecycle-config-revision: 7}}\n{extra}").as_bytes(),
    )
    .unwrap()
}
fn request(selected: &Selection, stream: bool, body: Value) -> reqwest::Request {
    prepare(
        &config(""),
        selected,
        "/v1beta/models/alias:generateContent",
        &HeaderMap::new(),
        body,
        stream,
    )
    .unwrap()
    .request
    .build()
    .unwrap()
}
fn body(request: &reqwest::Request) -> Value {
    serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap()
}

#[test]
fn vertex_home_token_native_request_fixture() {
    for (location, host) in [
        ("global", "aiplatform.googleapis.com"),
        ("us", "aiplatform.us.rep.googleapis.com"),
        ("eu", "aiplatform.eu.rep.googleapis.com"),
        ("us-central1", "us-central1-aiplatform.googleapis.com"),
    ] {
        let selected = selection(
            "vertex",
            "gemini-2.5-pro",
            json!({"metadata":{"access_token":"home-token","project_id":"home-project","location":location}}),
        );
        let request = request(&selected, true, json!({"contents":[],"session_id":"drop"}));
        assert_eq!(
            request.url().as_str(),
            format!(
                "https://{host}/v1/projects/home-project/locations/{location}/publishers/google/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
            )
        );
        assert_eq!(request.headers()["authorization"], "Bearer home-token");
        assert!(request.headers()["authorization"].is_sensitive());
        assert!(!request.headers().contains_key("x-goog-api-key"));
        assert!(body(&request).get("session_id").is_none());
        assert_eq!(body(&request)["model"], "gemini-2.5-pro");
        assert_eq!(request.timeout(), None);
    }
}

#[test]
fn vertex_key_resource_prefix_fixtures() {
    for (base, prefix) in [
        ("https://vertex.example", "/v1/publishers/google/models"),
        (
            "https://vertex.example/v1beta1",
            "/v1beta1/publishers/google/models",
        ),
        (
            "https://vertex.example/v1/projects/p/locations/l/publishers/google/models",
            "/v1/projects/p/locations/l/publishers/google/models",
        ),
    ] {
        let selected = selection(
            "vertex",
            "gemini-2.5-flash",
            json!({"attributes":{"api_key":"key","base_url":base}}),
        );
        let request = request(&selected, false, json!({"contents":[]}));
        assert_eq!(
            request.url().path(),
            format!("{prefix}/gemini-2.5-flash:generateContent")
        );
        assert_eq!(request.headers()["x-goog-api-key"], "key");
        assert!(!request.headers().contains_key("authorization"));
    }
}

#[test]
fn vertex_openapi_request_and_routing_fixtures() {
    let mut selected = selection(
        "vertex",
        "publishers/meta/models/llama-4",
        json!({"metadata":{"access_token":"token","project_id":"p","location":"us-east5"}}),
    );
    assert!(vertex::uses_openapi(&selected, "alias").unwrap());
    let prepared = prepare(
        &config(""),
        &selected,
        "/v1/chat/completions",
        &HeaderMap::new(),
        json!({"messages":[]}),
        true,
    )
    .unwrap();
    assert_eq!(prepared.format, ProviderFormat::OpenAi);
    let request = prepared.request.build().unwrap();
    assert_eq!(
        request.url().as_str(),
        "https://us-east5-aiplatform.googleapis.com/v1/projects/p/locations/us-east5/endpoints/openapi/chat/completions"
    );
    assert_eq!(body(&request)["model"], "meta/llama-4");
    selected.auth["attributes"] = json!({"vertex_model_openapi": "[{\"name\":\"publishers/meta/models/llama-4\",\"alias\":\"native-alias\",\"openapi\":false}]"});
    assert!(!vertex::uses_openapi(&selected, "native-alias").unwrap());
    assert!(!vertex::uses_openapi(&selected, "NATIVE-ALIAS(high)").unwrap());
    assert!(vertex::uses_openapi(&selected, "different-alias").unwrap());
    selected.auth["attributes"] = json!({"base_url":"https://vertex.example/v1/projects/p/locations/old/publishers/google/models", "openapi_location":"us-east5"});
    let overridden = prepare(
        &config(""),
        &selected,
        "/v1/chat/completions",
        &HeaderMap::new(),
        json!({"messages":[]}),
        false,
    )
    .unwrap()
    .request
    .build()
    .unwrap();
    assert_eq!(
        overridden.url().as_str(),
        "https://vertex.example/v1/projects/p/locations/us-east5/endpoints/openapi/chat/completions"
    );
    selected.auth["attributes"] =
        json!({"api_key":"key","base_url":"https://vertex.example/v1/endpoints/openapi"});
    let request = prepare(
        &config(""),
        &selected,
        "/v1/chat/completions",
        &HeaderMap::new(),
        json!({"messages":[]}),
        false,
    )
    .unwrap()
    .request
    .build()
    .unwrap();
    assert_eq!(request.headers()["authorization"], "Bearer key");
    assert_eq!(
        request.url().as_str(),
        "https://vertex.example/v1/endpoints/openapi/chat/completions"
    );
    selected.auth["attributes"]["base_url"] = "https://aiplatform.googleapis.com".into();
    assert!(resolve(&selected, "openai").is_err());
}

#[test]
fn google_credentials_fail_closed_without_local_refresh() {
    for (provider, auth) in [
        (
            "vertex",
            json!({"metadata":{"service_account":{"private_key":"not-used"}}}),
        ),
        (
            "antigravity",
            json!({"metadata":{"refresh_token":"not-used","project_id":"p"}}),
        ),
        ("antigravity", json!({"metadata":{"access_token":"token"}})),
    ] {
        assert!(resolve(&selection(provider, "gemini-2.5-pro", auth), "gemini").is_err());
    }
    let selected = selection(
        "vertex",
        "gemini-2.5-pro",
        json!({"metadata":{"access_token":"token","project_id":"../bad"}}),
    );
    assert!(
        prepare(
            &config(""),
            &selected,
            "/v1beta/models/a:generateContent",
            &HeaderMap::new(),
            json!({"contents":[]}),
            false
        )
        .is_err()
    );
}

#[test]
fn antigravity_native_envelope_fixture() {
    let selected = selection(
        "antigravity",
        "gemini-2.5-pro",
        json!({"metadata":{"access_token":"home-token","project_id":"project"}}),
    );
    let input = json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}],"safetySettings":[],"generationConfig":{"maxOutputTokens":42,"temperature":0.5}});
    let request = request(&selected, true, input.clone());
    assert_eq!(
        request.url().as_str(),
        "https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse"
    );
    assert_eq!(request.headers()["authorization"], "Bearer home-token");
    assert!(
        request.headers()["user-agent"]
            .to_str()
            .unwrap()
            .starts_with("antigravity/hub/2.29.0")
    );
    let envelope = body(&request);
    assert_eq!(envelope["model"], "gemini-2.5-pro");
    assert_eq!(envelope["project"], "project");
    assert_eq!(envelope["userAgent"], "antigravity");
    assert_eq!(envelope["requestType"], "agent");
    assert!(
        envelope["requestId"]
            .as_str()
            .unwrap()
            .starts_with("agent-")
    );
    assert_eq!(envelope["request"]["contents"], input["contents"]);
    assert!(envelope["request"].get("safetySettings").is_none());
    assert!(
        envelope["request"]["generationConfig"]
            .get("maxOutputTokens")
            .is_none()
    );
    assert_eq!(envelope["request"]["generationConfig"]["temperature"], 0.5);
    let second = request_fixture(&selected, input);
    assert_eq!(
        envelope["request"]["sessionId"],
        body(&second)["request"]["sessionId"]
    );
    assert_ne!(envelope["requestId"], body(&second)["requestId"]);
    assert_eq!(request.timeout(), None);
}
fn request_fixture(selected: &Selection, input: Value) -> reqwest::Request {
    request(selected, false, input)
}

#[test]
fn antigravity_payload_barrier_preserves_context_and_overrides() {
    let selected = selection(
        "antigravity",
        "gemini-2.5-pro",
        json!({"metadata":{"access_token":"token","project_id":"p","base_url":"https://cloud.example"}}),
    );
    let config = config(
        "payload:\n  override:\n    - models: [{name: gemini-2.5-pro, protocol: antigravity}]\n      params: {generationConfig.maxOutputTokens: 17}\n  filter:\n    - models: [{name: gemini-2.5-pro, protocol: antigravity}]\n      params: [sessionId]\n",
    );
    let original = json!({"contents":[],"model":"caller-alias"});
    let prepared = prepare_context(
        &config,
        &selected,
        "/v1beta/models/alias:generateContent",
        &HeaderMap::new(),
        original.clone(),
        false,
        Some(crate::payload::ApplyContext {
            requested_model: "caller-alias",
            from_protocol: "gemini",
            original: Some(&original),
            ..Default::default()
        }),
    )
    .unwrap();
    let request = prepared.request.build().unwrap();
    assert_eq!(
        request.url().as_str(),
        "https://cloud.example/v1internal:generateContent"
    );
    let envelope = body(&request);
    assert_eq!(
        envelope["request"]["generationConfig"]["maxOutputTokens"],
        17
    );
    assert!(envelope["request"].get("sessionId").is_none());
    assert!(envelope.get("generationConfig").is_none());
}

#[test]
fn antigravity_response_json_and_sse_data_fixtures() {
    let usage = json!({"promptTokenCount":10,"candidatesTokenCount":3,"thoughtsTokenCount":2,"cachedContentTokenCount":4,"totalTokenCount":15});
    let candidate =
        json!([{"content":{"role":"model","parts":[{"text":"OK"}]},"finishReason":"STOP"}]);
    for envelope in [
        json!({"response":{"candidates":candidate,"usageMetadata":usage},"traceId":"trace"}),
        json!({"response":{"candidates":candidate},"usageMetadata":usage}),
    ] {
        let output = antigravity::unwrap_response(&envelope).unwrap();
        assert_eq!(output["candidates"], candidate);
        assert_eq!(output["usageMetadata"], usage);
        assert_eq!(antigravity::usage_metadata(&envelope), Some(&usage));
        assert!(output.get("response").is_none());
        assert!(output.get("traceId").is_none());
    }
    // Parsed SSE data, including progress without usage and trailing usage-only events.
    assert!(
        antigravity::unwrap_response(&json!({"response":{"candidates":candidate}}))
            .unwrap()
            .get("usageMetadata")
            .is_none()
    );
    assert_eq!(
        antigravity::unwrap_response(&json!({"usageMetadata":usage})).unwrap(),
        json!({"usageMetadata":usage})
    );
    for invalid in [
        json!({}),
        json!({"response":null}),
        json!({"error":{"message":"secret"}}),
        json!({"response":{"error":{}}}),
    ] {
        assert!(antigravity::unwrap_response(&invalid).is_err());
    }
}

#[test]
fn antigravity_unimplemented_inputs_are_not_silently_sent() {
    let selected = selection(
        "antigravity",
        "claude-sonnet-4-5",
        json!({"metadata":{"access_token":"token","project_id":"p"}}),
    );
    for extra in [
        json!({"tools":[]}),
        json!({"generationConfig":{"responseSchema":{}}}),
        json!({"contents":[{"parts":[{"thoughtSignature":"signature"}]}]}),
    ] {
        let mut input = json!({"contents":[]});
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(
            prepare(
                &config(""),
                &selected,
                "/v1beta/models/a:generateContent",
                &HeaderMap::new(),
                input,
                false
            )
            .is_err()
        );
    }
    let request = request(
        &selected,
        false,
        json!({"contents":[],"generationConfig":{"maxOutputTokens":20},"sessionId":"caller-session"}),
    );
    assert_eq!(
        body(&request)["request"]["generationConfig"]["maxOutputTokens"],
        20
    );
    assert_eq!(body(&request)["request"]["sessionId"], "caller-session");
    assert_eq!(
        body(&request)["request"]["toolConfig"]["functionCallingConfig"]["mode"],
        "VALIDATED"
    );
}
