use super::*;

fn run(mut body: Value) -> Value {
    cloak(&mut body, false, "2026-08-01").unwrap();
    cache::normalize(&mut body);
    body
}

#[test]
fn go_utf16_fingerprint_fixtures() {
    // claude_cloaked_cache_repro_test.go: TestComputeFingerprintUsesNativeUTF16Indices.
    assert_eq!(
        fingerprint("😀abcdefghijklmnopqrstuvwxyz", "2.1.280"),
        "dac"
    );
    assert_eq!(fingerprint("abc😀abcdefghijklmnop", "2.1.280"), "695");
}

#[test]
fn modern_relocation_preserves_blocks_after_leading_user_run() {
    let body = run(json!({"model":"claude-sonnet-5", "system":[
        {"type":"text","text":"  first\n"}, {"type":"text","text":"second"}],
        "messages":[{"role":"user","content":"hello"},
        {"role":"system","content":[],"output_config":{"effort":"high"}},
        {"role":"user","content":"more"}, {"role":"assistant","content":"answer"}]}));
    assert_eq!(body["messages"][3]["role"], "system");
    assert_eq!(body["messages"][3]["content"][0]["text"], "  first\n");
    assert_eq!(body["messages"][4]["content"][0]["text"], "second");
    assert_eq!(body["messages"][5]["role"], "assistant");
    assert_eq!(body["system"][1]["text"], IDENTITY);
    let billing = body["system"][0]["text"].as_str().unwrap();
    assert_eq!(
        billing,
        format!(
            "x-anthropic-billing-header: cc_version=2.1.321.{}; cc_entrypoint=cli; cch=00000; cc_turn_origin=human;",
            fingerprint("hello", "2.1.321")
        )
    );
    assert!(body["system"][0].get("cache_control").is_none());
    assert_eq!(
        body["messages"][0]["content"][0]["text"],
        format!(
            "{DATE_PREFIX}2026-08-01.\n\n      IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>\n\n"
        )
    );
}

#[test]
fn legacy_reminders_follow_tool_results_and_preserve_whitespace() {
    let body = run(
        json!({"model":"anthropic/CLAUDE-SONNET-4-5", "system":"  policy\n",
        "messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"id","content":"ok"}, {"type":"text","text":"ask"}]}]}),
    );
    assert_eq!(body["messages"][0]["content"][0]["type"], "tool_result");
    assert_eq!(
        body["messages"][0]["content"][2]["text"],
        "<system-reminder>\n  policy\n</system-reminder>"
    );
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
}

#[test]
fn first_user_fingerprint_ignores_context_and_later_turns() {
    let body = run(json!({"model":"claude-opus-5", "messages":[
        {"role":"user","content":[{"type":"text","text":"<system-reminder>context</system-reminder>"}, {"type":"text","text":"original"}]},
        {"role":"assistant","content":"ok"}, {"role":"user","content":"later"}]}));
    assert!(
        body["system"][0]["text"]
            .as_str()
            .unwrap()
            .contains(&format!("2.1.321.{};", fingerprint("original", "2.1.321")))
    );
}

#[test]
fn strict_never_and_attribute_precedence() {
    let original = json!({"model":"claude-sonnet-5","system":"keep", "messages":[{"role":"user","content":"hello"}]});
    let mut body = original.clone();
    apply(
        &mut body,
        &json!({"metadata":{"cloak_mode":"never"}}),
        &HeaderMap::new(),
        false,
    )
    .unwrap();
    assert_eq!(body, original);
    apply(&mut body, &json!({"attributes":{"cloak_mode":"always","cloak_strict_mode":"true"},"metadata":{"cloak_mode":"never"}}), &HeaderMap::new(), false).unwrap();
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    assert!(!body.to_string().contains("keep"));
    for auth in [
        json!({"metadata":{"cloak_mode":"uncloak"}}),
        json!({"metadata":{"cloak_sensitive_words":["secret"]}}),
        json!({"metadata":{"cloak_cache_user_id":true}}),
    ] {
        assert!(apply(&mut original.clone(), &auth, &HeaderMap::new(), false).is_err());
    }
}

#[test]
fn no_silent_loss_of_unsupported_system_content() {
    for system in [
        json!([{"type":"image","source":{}}]),
        json!({"text":"policy"}),
    ] {
        let mut body = json!({"system":system,"messages":[{"role":"user","content":"hi"}]});
        assert!(cloak(&mut body, false, "2026-08-01").is_err());
    }
    assert!(
        cloak(
            &mut json!({"system":"keep","messages":[]}),
            false,
            "2026-08-01"
        )
        .is_err()
    );
}

#[test]
fn context_management_only_for_exact_valid_thinking_after_forced_choice() {
    for thinking in [
        json!(null),
        json!({"type":"disabled"}),
        json!({"type":"auto"}),
        json!({"type":"enabled"}),
        json!({"type":"adaptive"}),
    ] {
        let mut body = json!({"thinking":thinking});
        context_management(&mut body).unwrap();
        assert_eq!(
            body.get("context_management").is_some(),
            matches!(thinking["type"].as_str(), Some("enabled" | "adaptive"))
        );
    }
    let mut body =
        json!({"messages":[],"thinking":{"type":"adaptive"},"tool_choice":{"type":"any"}});
    super::super::normalize_native_body(&mut body);
    context_management(&mut body).unwrap();
    assert!(body.get("context_management").is_none());
    assert!(context_management(&mut json!({"context_management":{"edits":[]}})).is_err());
}

#[test]
fn native_requires_all_go_standard_signals_and_is_not_recloaked() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "user-agent",
        "claude-cli/2.1.321 (external, sdk-ts, agent-sdk/0.2.1)"
            .parse()
            .unwrap(),
    );
    headers.insert("x-app", "cli".parse().unwrap());
    headers.insert(
        "anthropic-beta",
        "oauth-2025-04-20,claude-code-20250219".parse().unwrap(),
    );
    let body = json!({"system":"native owned", "messages":[], "metadata":{"user_id":json!({
        "device_id":"a".repeat(64),"account_uuid":"", "session_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}).to_string()}});
    assert!(confirmed(&body, &headers));
    let mut untouched = body.clone();
    apply(&mut untouched, &json!({}), &headers, true).unwrap();
    assert_eq!(untouched, body);
    for name in ["user-agent", "x-app", "anthropic-beta"] {
        let mut missing = headers.clone();
        missing.remove(name);
        assert!(!confirmed(&body, &missing));
    }
    for ua in [
        "claude-cli/2.1.320 (external, cli)",
        "claude-cli/2.1.321 (external, sdk-py)",
        "claude-cli/2.1.321 (external, cli, bad)",
    ] {
        headers.insert("user-agent", ua.parse().unwrap());
        assert!(!confirmed(&body, &headers));
    }
}

#[test]
fn prepare_boundary_keeps_api_keys_native_and_oauth_placeholder_pre_serialization() {
    let auth = json!({"metadata":{"account_uuid":"account", "claude_device_ids":["b".repeat(64)]}});
    for oauth in [false, true] {
        let mut body = json!({"model":"claude-sonnet-5","system":"policy", "thinking":{"type":"adaptive"}, "messages":[{"role":"user","content":"hello"}]});
        super::super::prepare_body(
            &mut body,
            &mut HeaderMap::new(),
            &auth,
            &HeaderMap::new(),
            oauth,
        )
        .unwrap();
        if oauth {
            assert!(
                body["system"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("cch=00000;")
            );
            assert_eq!(body["thinking"]["display"], "updates");
            assert!(body.get("context_management").is_some());
            assert_eq!(body["messages"][1]["content"][0]["text"], "policy");
        } else {
            assert_eq!(body["system"], "policy");
            assert_eq!(body["messages"][0]["content"], "hello");
            assert!(body.get("context_management").is_none());
        }
    }
}

#[test]
fn unsupported_advanced_paths_fail_explicitly() {
    for body in [
        json!({"max_tokens":1,"messages":[]}),
        json!({"model":"claude-fable-5-1","messages":[]}),
        json!({"cache_control":{"type":"ephemeral"},"messages":[]}),
        json!({"system":"Return a short title","messages":[]}),
    ] {
        assert!(apply(&mut body.clone(), &json!({}), &HeaderMap::new(), false).is_err());
    }
}

#[test]
fn cache_limit_priority_and_ttl_order_match_go() {
    let cc = json!({"type":"ephemeral"});
    let mut body = json!({"tools":[{"cache_control":cc},{"cache_control":cc}],
        "system":[{"type":"text","text":"one","cache_control":cc}, {"type":"text","text":"two","cache_control":cc}],
        "messages":[{"role":"user","content":[{"type":"text","text":"a","cache_control":cc}]},
        {"role":"user","content":[{"type":"text","text":"b","cache_control":cc}]}]});
    cache::normalize(&mut body);
    assert!(body["system"][0].get("cache_control").is_none());
    assert!(body["tools"][0].get("cache_control").is_none());
    assert_eq!(body["tools"][1]["cache_control"]["ttl"], "1h");
    body["system"][1]["cache_control"]["ttl"] = "5m".into();
    cache::normalize(&mut body);
    assert!(
        body["messages"][0]["content"][0]["cache_control"]
            .get("ttl")
            .is_none()
    );
    assert_eq!(body["system"][1]["cache_control"]["ttl"], "5m");
}
