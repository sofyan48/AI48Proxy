use super::*;
use serde_json::json;

fn selected(auth: Value) -> Selection {
    serde_json::from_value(json!({
        "provider":"antigravity", "model":"gemini-3-flash",
        "auth_index":"fixture", "auth":auth
    }))
    .unwrap()
}

#[test]
fn configured_user_agent_matches_go_runtime_not_onboarding() {
    let long = format!("{USER_AGENT} google-api-nodejs-client/10.3.0");
    for (input, expected) in [
        ("", USER_AGENT),
        ("   ", USER_AGENT),
        (long.as_str(), USER_AGENT),
        (
            " Antigravity/2.29.0 GOOGLE-API-NODEJS-CLIENT/10.3.0 ",
            "Antigravity/2.29.0",
        ),
        (
            "custom google-api-nodejs-client/10.3.0",
            "custom google-api-nodejs-client/10.3.0",
        ),
    ] {
        assert_eq!(request_user_agent(input), expected);
    }
    let auth = json!({"metadata":{"access_token":"fixture", "project_id":"fixture-project", "user_agent":"metadata-ua"},
        "attributes":{"user_agent":long, "base_url":" https://example.invalid/prefix/ "}});
    let mut headers = HeaderMap::new();
    assert_eq!(
        credentials(&auth, "gemini", &mut headers).unwrap(),
        "https://example.invalid/prefix/"
    );
    assert_eq!(headers["user-agent"], USER_AGENT);
    assert!(headers["authorization"].is_sensitive());
    assert!(!headers.contains_key("x-goog-api-key"));
    assert!(!headers.contains_key("x-goog-user-project"));
    assert!(!headers.contains_key("x-goog-api-client"));
}

#[test]
fn daily_and_custom_paths_match_go() {
    assert_eq!(
        url(BASE, false).unwrap().as_str(),
        "https://daily-cloudcode-pa.googleapis.com/v1internal:generateContent"
    );
    assert_eq!(
        url(BASE, true).unwrap().as_str(),
        "https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse"
    );
    assert_eq!(
        url("https://example.invalid/prefix/", true)
            .unwrap()
            .as_str(),
        "https://example.invalid/prefix/v1internal:streamGenerateContent?alt=sse"
    );
}

#[test]
fn flash_envelope_preserves_project_and_matches_go_cleanup() {
    let selection = selected(json!({"metadata":{"project_id":" fixture-project "}}));
    let mut body = json!({"contents":[{"role":"user", "parts":[{"text":"hello"}]}],
        "safetySettings":[], "model":"client-alias", "stream":true,
        "generationConfig":{"maxOutputTokens":999,"temperature":0.5}});
    let session = stable_session(&body);
    normalize(&mut body, &selection).unwrap();
    assert_eq!(body["model"], "gemini-3-flash");
    assert_eq!(body["project"], "fixture-project");
    assert_eq!(body["userAgent"], "antigravity");
    assert_eq!(body["requestType"], "agent");
    let request_id = body["requestId"]
        .as_str()
        .unwrap()
        .strip_prefix("agent-")
        .unwrap();
    assert!(uuid::Uuid::parse_str(request_id).is_ok());
    assert_eq!(body["request"]["sessionId"], session);
    assert_eq!(body["request"]["generationConfig"]["temperature"], 0.5);
    for path in [
        "/request/safetySettings",
        "/request/model",
        "/request/stream",
        "/request/generationConfig/maxOutputTokens",
    ] {
        assert!(body.pointer(path).is_none());
    }
    assert_eq!(body.as_object().unwrap().len(), 6);
}

#[test]
fn missing_or_wrong_type_project_never_invents_a_project() {
    for project in [Value::Null, json!({"id":"fixture"}), json!(1), json!(" ")] {
        let auth = json!({"metadata":{"access_token":"fixture", "project_id":project}});
        assert!(credentials(&auth, "gemini", &mut HeaderMap::new()).is_err());
    }
}

#[test]
fn forbidden_diagnostic_returns_only_allowlisted_values() {
    let bytes = serde_json::to_vec(&json!({"error":{
        "code":403,"status":"PERMISSION_DENIED", "message":"secret email token project",
        "details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo", "reason":"TOS_VIOLATION",
            "metadata":{"project":"private-project", "token":"private-token"}}]
    }}))
    .unwrap();
    assert_eq!(
        forbidden_diagnostic(403, &bytes),
        Some(ForbiddenDiagnostic {
            code: Some(403),
            status: Some("PERMISSION_DENIED"),
            reason: Some("TOS_VIOLATION")
        })
    );
    let diagnostic = format!("{:?}", forbidden_diagnostic(403, &bytes));
    for private in ["secret", "email", "token", "project"] {
        assert!(!diagnostic.contains(private));
    }
    assert!(forbidden_diagnostic(401, &bytes).is_none());
    assert!(forbidden_diagnostic(403, b"not json private-token").is_none());
    assert!(forbidden_diagnostic(403, &vec![b' '; 65537]).is_none());
    let unknown = br#"{"error":{"code":"403","status":"private-token","message":"TOS_VIOLATION","details":[{"@type":"wrong","reason":"TOS_VIOLATION"},{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"private-project"}]}}"#;
    assert_eq!(
        forbidden_diagnostic(403, unknown),
        Some(ForbiddenDiagnostic::default())
    );
}

#[test]
fn chat_tools_go_schema_cleanup_and_synthetic_replay() {
    let selection = selected(json!({"metadata":{"project_id":"p"}}));
    let args = json!({"title":"keep", "format":"keep", "default":3, "const":4});
    let chat = json!({"messages":[{"role":"user","content":"run"},
        {"role":"assistant","content":null,"tool_calls":[
            {"id":"a","type":"function","function":{"name":"run","arguments":args.to_string()}},
            {"id":"b","type":"function","function":{"name":"run","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"b","content":"{\"raw\":true}"},
        {"role":"tool","tool_call_id":"a","content":"done"}],
        "tools":[{"type":"function","function":{"name":"run","strict":true,"parameters":{
            "type":"object","additionalProperties":false,"properties":{
                "title":{"type":"integer","enum":[1,2],"minimum":1},
                "items":{"type":"array"}},"required":["title","missing"]}}}],
        "tool_choice":{"type":"function","function":{"name":"run"}}});
    let mut body =
        crate::protocol::gemini::antigravity_chat_request(&chat, "gemini-3-flash").unwrap();
    normalize(&mut body, &selection).unwrap();
    let request = &body["request"];
    let schema = &request["tools"][0]["functionDeclarations"][0]["parameters"];
    assert_eq!(schema["properties"]["title"]["type"], "integer");
    assert_eq!(
        schema["properties"]["title"]["description"],
        "Allowed: 1, 2 (minimum: 1)"
    );
    assert!(schema["properties"]["title"].get("enum").is_none());
    assert_eq!(schema["required"], json!(["title"]));
    assert_eq!(
        schema["properties"]["items"]["items"],
        json!({"type":"string"})
    );
    assert_eq!(
        request["contents"][1]["parts"][0]["functionCall"]["args"],
        args
    );
    assert_eq!(
        request["contents"][1]["parts"][0]["thoughtSignature"],
        BYPASS_FIXTURE
    );
    assert!(
        request["contents"][1]["parts"][1]
            .get("thoughtSignature")
            .is_none()
    );
    assert_eq!(
        request["contents"][2]["parts"][0]["functionResponse"]["id"],
        "b"
    );
    assert_eq!(
        request["contents"][2]["parts"][0]["functionResponse"]["response"]["result"],
        "{\"raw\":true}"
    );
    assert_eq!(
        request["toolConfig"]["functionCallingConfig"],
        json!({"mode":"ANY","allowedFunctionNames":["run"]})
    );
}
const BYPASS_FIXTURE: &str = "skip_thought_signature_validator";

#[test]
fn replay_restores_native_function_response_id_from_stable_provenance() {
    let selection = selected(json!({"metadata":{"project_id":"p"}}));
    let args = json!({"path":"/tmp/file"});
    let stable_id = super::tools::stable_replay_id("native-call", "Read", &args);
    let mut body = json!({
        "contents":[
            {"role":"model","parts":[{"functionCall":{"id":"native-call","name":"Read","args":args}}]},
            {"role":"user","parts":[{"functionResponse":{"id":stable_id,"name":"Read","response":{"result":"ok"}}}]}
        ]
    });

    normalize(&mut body, &selection).unwrap();
    let response = &body["request"]["contents"][1]["parts"][0]["functionResponse"];
    assert_eq!(response["id"], "native-call");
    assert_eq!(response["name"], "Read");
}

#[test]
fn mismatched_stable_provenance_does_not_restore_native_id() {
    let selection = selected(json!({"metadata":{"project_id":"p"}}));
    let mut body = json!({"contents":[
        {"role":"model","parts":[{"functionCall":{"id":"native-call","name":"Read","args":{"path":"/tmp/file"}}}]},
        {"role":"user","parts":[{"functionResponse":{"id":"cpa_gemini_bad","name":"Read","response":{"result":"ok"}}}]}
    ]});
    assert!(normalize(&mut body, &selection).is_err());
}

#[test]
fn native_tool_schema_preserves_types_and_claude_placeholders() {
    for model in ["gemini-3-flash", "claude-sonnet-4-5"] {
        let mut selection = selected(json!({"metadata":{"project_id":"p"}}));
        selection.model = model.into();
        let mut body = json!({"contents":[],"tools":[{"functionDeclarations":[
            {"name":"run","parametersJsonSchema":{"type":"object","properties":{
                "count":{"type":"integer","enum":[1,2]},"empty":{"type":"object"}},"required":["count"]}}]}]});
        normalize(&mut body, &selection).unwrap();
        let schema = &body["request"]["tools"][0]["functionDeclarations"][0]["parameters"];
        assert_eq!(schema["properties"]["count"]["type"], "integer");
        assert!(schema["properties"]["count"].get("enum").is_none());
        if model.contains("claude") {
            assert_eq!(schema["properties"]["empty"]["required"], json!(["reason"]));
        } else {
            assert_eq!(schema["properties"]["empty"]["properties"], json!({}));
        }
    }
}

#[test]
fn unsupported_replay_and_schema_still_fail_closed() {
    let selection = selected(json!({"metadata":{"project_id":"p"}}));
    for part in [
        json!({"text":"thinking","thought":true}),
        json!({"text":"signed","thought_signature":"opaque"}),
        json!({"functionCall":{"id":"a","name":"run","args":{}},"thoughtSignature":"opaque"}),
        json!({"functionResponse":{"id":"a","name":"run","response":{}}}),
        json!({"functionCall":{"id":"a","name":"run","args":{},"partialArgs":[]}}),
    ] {
        let mut body = json!({"contents":[{"role":"model","parts":[part]}]});
        assert!(normalize(&mut body, &selection).is_err());
    }
    for schema in [
        json!({"$ref":"#/$defs/A"}),
        json!({"allOf":[{"type":"object"}]}),
        json!({"if":{}}),
    ] {
        let mut body = json!({"contents":[],"tools":[{"functionDeclarations":[{"name":"run","parameters":schema}]}]});
        assert!(normalize(&mut body, &selection).is_err());
    }
    for mode in ["AUTO", "NONE", "ANY"] {
        let mut selection = selected(json!({"metadata":{"project_id":"p"}}));
        selection.model = "claude-sonnet-4-5".into();
        let mut body = json!({"contents":[],"tools":[{"functionDeclarations":[{"name":"run"}]}],"toolConfig":{"functionCallingConfig":{"mode":mode}}});
        assert!(normalize(&mut body, &selection).is_err());
    }
}

#[test]
fn go_gemini_function_signature_roundtrip_json_and_fragmented_sse() {
    let sig = "EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA";
    assert!(signature::function_signature(sig));
    assert!(!signature::function_signature(
        "AQAAAAEAAAAAAAAAAAAAAAAAAAAA"
    ));
    let native = unwrap_response(&json!({"response":{"candidates":[{"content":{"parts":[
        {"functionCall":{"id":"native-id","name":"run","args":{}},"thoughtSignature":sig} ]},"finishReason":"STOP"}]}})).unwrap();
    let chat = crate::protocol::gemini::chat_response(&native, "g").unwrap();
    let message = &chat["choices"][0]["message"];
    let mut body = crate::protocol::gemini::chat_request(
        &json!({"messages":[
        {"role":"user","content":"run"}, message,
        {"role":"tool","tool_call_id":"native-id","content":"ok"}]}),
        "g",
    )
    .unwrap();
    normalize(&mut body, &selected(json!({"metadata":{"project_id":"p"}}))).unwrap();
    assert_eq!(
        body["request"]["contents"][1]["parts"][0]["thoughtSignature"],
        sig
    );
    let bytes = format!("data: {native}\n\n").into_bytes();
    for split in 0..=bytes.len() {
        let mut stream = crate::protocol::gemini::StreamTranslator::new("g", false);
        let mut out = stream.feed(&bytes[..split]).unwrap();
        out.extend(stream.feed(&bytes[split..]).unwrap());
        out.extend(stream.finish().unwrap());
        let out = String::from_utf8(out).unwrap();
        let chunk: Value = serde_json::from_str(
            out.split("\n\n")
                .next()
                .unwrap()
                .strip_prefix("data: ")
                .unwrap(),
        )
        .unwrap();
        let call = &chunk["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(call["id"], "native-id");
        assert_eq!(call["extra_content"]["google"]["thought_signature"], sig);
        assert_eq!(chunk["choices"][0]["finish_reason"], "tool_calls");
    }
}

#[test]
fn generic_permission_denied_does_not_claim_account_suspension() {
    assert_eq!(
        forbidden_diagnostic(
            403,
            br#"{"error":{"code":403,"status":"PERMISSION_DENIED"}}"#
        ),
        Some(ForbiddenDiagnostic {
            code: Some(403),
            status: Some("PERMISSION_DENIED"),
            reason: None
        })
    );
}
