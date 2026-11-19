use super::*;
use crate::{config::Config, home::client::Selection, providers};

fn auth() -> Value {
    // Home's sanitizer removes refresh material, not account/device identity.
    json!({"id":"home-claude","provider":"claude","metadata":{
        "access_token":"sk-ant-oat01-fixture", "account_uuid":"cccccccc-cccc-4ccc-8ccc-cccccccccccc",
        "claude_device_ids":["bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]}})
}

fn prepare(auth: Value, body: Value, incoming: &HeaderMap, stream: bool) -> reqwest::Request {
    let model = body["model"].as_str().unwrap_or("claude-sonnet-4-5");
    let selection: Selection = serde_json::from_value(json!({
        "provider":"claude","model":model,"auth_index":"home-selected","auth":auth
    }))
    .unwrap();
    providers::prepare(
        &Config::parse(b"credential-concurrency: {lifecycle-config-revision: 7}\n").unwrap(),
        &selection,
        "/v1/messages",
        incoming,
        body,
        stream,
    )
    .unwrap()
    .request
    .build()
    .unwrap()
}

fn payload(request: &reqwest::Request) -> Value {
    serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap()
}

#[test]
fn home_oauth_custom_tools_and_identity_round_trip() {
    let session = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let body = json!({"messages":[{"role":"user","content":"Hello"}], "max_tokens":128,
        "metadata":{"other":"keep","user_id":json!({"device_id":"attacker","account_uuid":"attacker",
            "session_id":session,"extension":{"keep":true}}).to_string()},
        "tools":[{"type":"custom","name":"lookup","description":"Find data","input_schema":{"type":"object"}}]});
    for stream in [false, true] {
        let request = prepare(auth(), body.clone(), &HeaderMap::new(), stream);
        assert_eq!(
            request.url().as_str(),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            request.headers()["authorization"],
            "Bearer sk-ant-oat01-fixture"
        );
        assert!(request.headers()["authorization"].is_sensitive());
        assert!(!request.headers().contains_key("x-api-key"));
        assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
        assert_eq!(request.headers()["x-claude-code-session-id"], session);
        assert!(
            uuid::Uuid::parse_str(request.headers()["x-client-request-id"].to_str().unwrap())
                .is_ok()
        );
        assert_eq!(request.timeout(), None);
        let body = payload(&request);
        assert_eq!(body["stream"], stream);
        assert_eq!(body["tools"][0]["name"], "lookup");
        assert!(body["tools"][0].get("type").is_none());
        let identity: Value =
            serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(identity["account_uuid"], auth()["metadata"]["account_uuid"]);
        assert_eq!(
            identity["device_id"],
            auth()["metadata"]["claude_device_ids"][0]
        );
        assert_eq!(identity["extension"], json!({"keep":true}));
        assert_eq!(body["metadata"]["other"], "keep");
    }
}

#[test]
fn api_key_and_oauth_lift_all_betas_and_preserve_version() {
    let mut incoming = HeaderMap::new();
    incoming.append("anthropic-beta", " first , shared ".parse().unwrap());
    incoming.append("anthropic-beta", "second,first".parse().unwrap());
    incoming.insert("anthropic-version", "2023-06-01".parse().unwrap());
    for auth in [
        auth(),
        json!({"attributes":{"api_key":"sk-ant-api03-fixture"}}),
    ] {
        for extra in [json!([" shared ", "third", ""]), json!(" shared,third ")] {
            let request = prepare(
                auth.clone(),
                json!({"messages":[],"max_tokens":16,"betas":extra}),
                &incoming,
                false,
            );
            let body = payload(&request);
            assert!(body.get("betas").is_none());
            let betas: Vec<_> = request.headers()["anthropic-beta"]
                .to_str()
                .unwrap()
                .split(',')
                .collect();
            for name in ["first", "second", "shared", "third"] {
                assert_eq!(betas.iter().filter(|v| **v == name).count(), 1);
            }
            if request.headers().contains_key("x-api-key") {
                assert_eq!(betas, ["first", "shared", "second", "third"]);
                assert!(!request.headers().contains_key("authorization"));
                assert!(!request.headers().contains_key("x-app"));
                assert_eq!(body.get("metadata"), None);
            } else {
                assert!(betas.contains(&"oauth-2025-04-20"));
            }
        }
    }
}

#[test]
fn forced_tools_and_native_sampling_match_go_constraints() {
    for oauth in [false, true] {
        for choice in ["any", "tool"] {
            let mut body = json!({"messages":[], "tool_choice":{"type":choice,"name":"lookup"},
                "thinking":{"type":"adaptive"}, "output_config":{"effort":"high","format":{"type":"json_schema"}},
                "temperature":0.5,"top_p":0.9});
            prepare_body(
                &mut body,
                &mut HeaderMap::new(),
                &auth(),
                &HeaderMap::new(),
                oauth,
            )
            .unwrap();
            assert!(body.get("thinking").is_none());
            assert!(body["output_config"].get("effort").is_none());
            assert_eq!(body["output_config"]["format"]["type"], "json_schema");
            assert_eq!(body["temperature"], 0.5);
            assert!(body.get("top_p").is_none());
        }
        let mut body = json!({"messages":[],"thinking":{"type":"enabled","budget_tokens":1024},
            "temperature":0.5,"top_p":0.9,"top_k":10});
        prepare_body(
            &mut body,
            &mut HeaderMap::new(),
            &auth(),
            &HeaderMap::new(),
            oauth,
        )
        .unwrap();
        for field in ["temperature", "top_p", "top_k"] {
            assert!(body.get(field).is_none());
        }
        body["temperature"] = json!(1);
        body["top_p"] = json!(0.95);
        prepare_body(
            &mut body,
            &mut HeaderMap::new(),
            &auth(),
            &HeaderMap::new(),
            oauth,
        )
        .unwrap();
        assert_eq!(body["temperature"], 1);
        assert_eq!(body["top_p"], 0.95);
    }
}

#[test]
fn provenance_is_removed_without_inventing_thinking_signatures() {
    let thinking = json!({"type":"thinking","thinking":"native reasoning","signature":"opaque-native-signature"});
    let mut body = json!({"messages":[{"role":"assistant","content":[thinking.clone(),
        {"type":"tool_use","id":"tool_1","name":"lookup","input":{},"signature":"foreign",
            "thoughtSignature":"foreign","thought_signature":"foreign","model":"foreign",
            "extra_content":{"google":{"thought_signature":"foreign"}}}]}]});
    prepare_body(
        &mut body,
        &mut HeaderMap::new(),
        &auth(),
        &HeaderMap::new(),
        true,
    )
    .unwrap();
    assert_eq!(body["messages"][0]["content"][0], thinking);
    assert_eq!(
        body["messages"][0]["content"][1],
        json!({"type":"tool_use","id":"tool_1","name":"lookup","input":{}})
    );
}

#[test]
fn missing_home_identity_and_advanced_oauth_tools_still_fail_closed() {
    for field in ["account_uuid", "claude_device_ids"] {
        let mut selected = auth();
        selected["metadata"].as_object_mut().unwrap().remove(field);
        assert!(
            prepare_body(
                &mut json!({"messages":[]}),
                &mut HeaderMap::new(),
                &selected,
                &HeaderMap::new(),
                true
            )
            .is_err()
        );
    }
    for tool_type in ["web_search_20250305", "bash_20250124", "computer_20250124"] {
        assert!(
            prepare_body(
                &mut json!({"messages":[],"tools":[{"type":tool_type}]}),
                &mut HeaderMap::new(),
                &auth(),
                &HeaderMap::new(),
                true
            )
            .is_err()
        );
    }
}

#[test]
fn api_key_native_tools_are_not_subject_to_oauth_restrictions() {
    let tools = json!([
        {"name":"lookup","input_schema":{"type":"object"}},
        {"type":"custom","name":"custom_lookup","input_schema":{"type":"object"}},
        {"type":"web_search_20250305","name":"web_search"}
    ]);
    let request = prepare(
        json!({"attributes":{"api_key":"sk-ant-api03-fixture"}}),
        json!({"messages":[],"max_tokens":16,"tools":tools}),
        &HeaderMap::new(),
        false,
    );
    assert_eq!(payload(&request)["tools"], tools);
}

#[test]
fn go_account_identity_requires_nonempty_not_uuid_syntax() {
    let mut selected = auth();
    selected["metadata"]["account_uuid"] = json!("home-account-id");
    let request = prepare(
        selected,
        json!({"messages":[],"max_tokens":16}),
        &HeaderMap::new(),
        false,
    );
    let identity: Value =
        serde_json::from_str(payload(&request)["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(identity["account_uuid"], "home-account-id");
}

#[test]
fn operator_beta_and_required_field_betas() {
    let mut headers = HeaderMap::new();
    headers.insert("anthropic-beta", "operator-beta".parse().unwrap());
    let mut body = json!({"messages":[], "speed":"fast", "thinking":{"type":"adaptive","block_binding":"bound"}});
    prepare_body(
        &mut body,
        &mut headers,
        &json!({}),
        &HeaderMap::new(),
        false,
    )
    .unwrap();
    assert_eq!(
        headers["anthropic-beta"],
        "operator-beta,fast-mode-2026-02-01,thinking-binding-controls-2026-08-01"
    );
}

#[test]
fn basic_oauth_model_baselines_match_go_wire_order() {
    // Derived from claudeCodeCLIBetas with no requested betas, probe/helper,
    // subagent or advanced fields. Compare the complete header, not membership.
    let prefix = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,context-management-2025-06-27,prompt-caching-scope-2026-01-05";
    let haiku = "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,context-management-2025-06-27,prompt-caching-scope-2026-01-05,claude-code-20250219";
    for (model, start, tail) in [
        (
            "claude-haiku-5-5",
            haiku,
            "per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,thinking-token-count-2026-05-13,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24",
        ),
        (
            "claude-haiku-4-5",
            prefix,
            "thinking-token-count-2026-05-13",
        ),
        (
            "claude-sonnet-4-5",
            prefix,
            "thinking-token-count-2026-05-13,effort-2025-11-24",
        ),
        (
            "claude-sonnet-5",
            prefix,
            "thinking-token-count-2026-05-13,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24",
        ),
        (
            "claude-sonnet-5-5",
            prefix,
            "per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,thinking-token-count-2026-05-13,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24",
        ),
        (
            "claude-opus-5-5",
            prefix,
            "per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,thinking-token-count-2026-05-13,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24",
        ),
    ] {
        for stream in [false, true] {
            let request = prepare(
                auth(),
                json!({"model":model,"messages":[{"role":"user","content":"hi"}],"max_tokens":16}),
                &HeaderMap::new(),
                stream,
            );
            assert_eq!(
                request.headers()["anthropic-beta"],
                format!("{start},{tail},extended-cache-ttl-2025-04-11"),
                "{model}, stream={stream}"
            );
            assert_eq!(
                payload(&request)["messages"],
                json!([{"role":"user","content":"hi"}])
            );
        }
    }
}

#[test]
fn haiku55_matching_is_the_go_helper_not_a_substring_guess() {
    for model in [
        "claude-haiku-5-5",
        "claude-haiku-5.5",
        "claude-haiku-5-5-20261101",
        "claude-haiku-5.5-latest",
        "claude-haiku-5-5[1m]",
        "claude-haiku-5.5[1m]",
        " Provider/Nested/CLAUDE-HAIKU-5-5 ",
    ] {
        assert!(betas::is_haiku55(model), "{model}");
        let mut body = json!({"model":model,"messages":[]});
        let mut headers = HeaderMap::new();
        prepare_body(&mut body, &mut headers, &auth(), &HeaderMap::new(), true).unwrap();
        assert!(headers["anthropic-beta"].to_str().unwrap().contains(
            "prompt-caching-scope-2026-01-05,claude-code-20250219,per-turn-control-2026-07-01"
        ));
    }
    for model in [
        "claude-haiku-5-50",
        "claude-haiku-5.50",
        "claude-haiku-5-5x",
        "claude-haiku-4-5",
        "prefix-claude-haiku-5-5",
        "claude-haiku-5-5/other",
    ] {
        assert!(!betas::is_haiku55(model), "{model}");
    }
}

#[test]
fn haiku_baseline_preserves_custom_beta_precedence_and_display_policy() {
    for (display, redact) in [
        (json!("omitted"), false),
        (json!("updates"), false),
        (json!(" "), true),
        (Value::Null, true),
    ] {
        let mut incoming = HeaderMap::new();
        incoming.insert(
            "anthropic-beta",
            "caller-beta,claude-code-20250219".parse().unwrap(),
        );
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-beta", "operator-beta".parse().unwrap());
        let mut body = json!({"model":"claude-haiku-5-5","messages":[],"thinking":{"type":"disabled","display":display},"betas":["caller-beta","body-beta"]});
        prepare_body(&mut body, &mut headers, &auth(), &incoming, true).unwrap();
        let header = headers["anthropic-beta"].to_str().unwrap();
        assert_eq!(header.contains("redact-thinking-2026-02-12"), redact);
        assert!(!header.contains("effort-2025-11-24"));
        assert!(!header.contains("operator-beta"));
        assert!(header.ends_with("extended-cache-ttl-2025-04-11,caller-beta,body-beta"));
        assert_eq!(header.matches("claude-code-20250219").count(), 1);
        assert!(body.get("betas").is_none());
        assert_eq!(body["thinking"]["type"], "disabled");
    }
}

#[test]
fn claude_payload_rules_remain_the_final_business_payload_barrier() {
    let selection: Selection = serde_json::from_value(json!({
        "provider":"claude","model":"claude-haiku-5-5","auth_index":"home-selected","auth":auth()
    }))
    .unwrap();
    let config = Config::parse(b"credential-concurrency: {lifecycle-config-revision: 7}\npayload:\n  override:\n    - models: [{name: 'claude-haiku-5-5', protocol: claude}]\n      params: {max_tokens: 3, temperature: 0.25}\n  filter:\n    - models: [{name: 'claude-haiku-5-5', protocol: claude}]\n      params: [metadata, thinking]\n").unwrap();
    let request = providers::prepare(
        &config,
        &selection,
        "/v1/messages",
        &HeaderMap::new(),
        json!({"messages":[{"role":"user","content":"hi"}], "max_tokens":16,
            "thinking":{"type":"enabled","budget_tokens":1024}, "temperature":0.5}),
        false,
    )
    .unwrap()
    .request
    .build()
    .unwrap();
    let body = payload(&request);
    assert_eq!(body["max_tokens"], 3);
    assert_eq!(body["temperature"], 0.25);
    assert!(body.get("metadata").is_none());
    assert!(body.get("thinking").is_none());
}

#[test]
fn checked_out_go_contract_sources_match_the_regression_fixtures() {
    // Source parity is checked against the actual sibling checkout, not a copied
    // Go fixture. Packaged Rust-only checkouts can still run the behavior tests.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let go = root.join("CLIProxyAPI/internal/runtime/executor/claude_executor_request.go");
    if !go.exists() {
        return;
    }
    let source = std::fs::read_to_string(go).unwrap();
    for marker in [
        "disableThinkingIfToolChoiceForcedDoc",
        "normalizeClaudeSamplingForUpstreamDoc",
        "Every client",
        "including typed custom declarations",
        "doc.Delete(\"betas\")",
        "fast-mode-2026-02-01",
        "thinking-binding-controls-2026-08-01",
        "2023-06-01",
        "claudeCodeAfterScope := isClaudeHaiku55Model(model)",
        "if !isQuotaProbe && !claudeCodeAfterScope {",
        "if !isQuotaProbe && claudeCodeAfterScope {",
        "return model == \"claude-haiku-5-5\" || strings.HasPrefix(model, \"claude-haiku-5-5-\") || strings.HasPrefix(model, \"claude-haiku-5-5[\") || model == \"claude-haiku-5.5\" || strings.HasPrefix(model, \"claude-haiku-5.5-\") || strings.HasPrefix(model, \"claude-haiku-5.5[\")",
        "model = model[slash+1:]",
        "if !isClaudeSonnet5Model(model) {",
        "if !claudeUsesLegacySystemReminder(body) && claudeIncludeMidConvClearAt(body, requested) {",
        "if isClaudeHaikuModel(model) && !isClaudeHaiku55Model(model) {",
        "display.Type == ojson.String && strings.TrimSpace(display.String()) != \"\"",
    ] {
        assert!(
            source.contains(marker),
            "Go request contract changed: {marker}"
        );
    }
    let identity = std::fs::read_to_string(
        root.join("CLIProxyAPI/internal/runtime/executor/helps/claude_credential_identity.go"),
    )
    .unwrap();
    assert!(identity.contains("case \"device_id\", \"account_uuid\", \"session_id\":"));
    let device =
        std::fs::read_to_string(root.join("CLIProxyAPI/internal/auth/claude/identity.go")).unwrap();
    assert!(device.contains("ClaudeDevicePoolSize       = 1"));
    let sanitizer =
        std::fs::read_to_string(root.join("AI48Home/internal/home/auth_sanitize.go")).unwrap();
    assert!(sanitizer.contains("delete(out.Metadata, \"refresh_token\")"));
    assert!(!sanitizer.contains("delete(out.Metadata, \"account_uuid\")"));
    assert!(!sanitizer.contains("delete(out.Metadata, \"claude_device_ids\")"));
}
