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
