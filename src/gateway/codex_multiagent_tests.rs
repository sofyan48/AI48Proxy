use super::*;
use futures_util::StreamExt;

const CODEX_UA: &str = "codex_cli_rs/0.120.0";

fn config(optimize: bool, orphan: bool) -> Config {
    let mut config =
        Config::parse(b"credential-concurrency: {lifecycle-config-revision: 1}\n").unwrap();
    config.providers.codex_optimize_multi_agent_v2 = optimize;
    config.providers.codex_orphan_delegation = orphan;
    config
}

fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.insert(*name, value.parse().unwrap());
    }
    headers
}

fn selection(provider: &str, auth: Value) -> Selection {
    serde_json::from_value(json!({"model":"m","provider":provider,"auth_index":"a","auth":auth}))
        .unwrap()
}

fn collaboration_request() -> Value {
    json!({"model":"m","tools":[{"type":"namespace","name":"collaboration","tools":[
        {"type":"function","name":"spawn_agent","parameters":{"properties":{"message":{"type":"string","encrypted":true}}}},
        {"type":"function","name":"send_message","parameters":{"properties":{"message":{"encrypted":true}}}}]},
        {"type":"function","name":"followup_task","parameters":{"properties":{"message":{"encrypted":true},"other":{"encrypted":true}}}}],
      "input":[{"type":"agent_message","author":"a","recipient":"b",
        "content":[{"type":"encrypted_content","encrypted_content":"hello"},{"type":"input_text","text":"keep"}]}]})
}

#[test]
fn codex_client_user_agents_match_go() {
    for ua in [
        "Codex Desktop/1.0",
        "codex-tui/0.1",
        "codex_cli_rs",
        "codex_cli_rs/0.1",
        "codex_exec/0.1",
        " codex_cli_rs/1 ",
    ] {
        assert!(is_codex_client(ua), "{ua}");
    }
    for ua in ["", "codex_cli_rsx", "curl/8", "Codex Desktop", "codex-tui"] {
        assert!(!is_codex_client(ua), "{ua}");
    }
}

#[test]
fn codex_attempt_optimizes_namespace_encryption_and_agent_content() {
    let mut body = collaboration_request();
    let h = headers(&[("user-agent", CODEX_UA)]);
    assert!(prepare_attempt(
        &config(true, false),
        &selection("codex", json!({})),
        "/v1/responses",
        &h,
        &mut body
    ));
    assert_eq!(body["tools"][0]["name"], "collaboration-optimize");
    for path in [
        "/tools/0/tools/0/parameters/properties/message",
        "/tools/0/tools/1/parameters/properties/message",
        "/tools/1/parameters/properties/message",
    ] {
        assert!(
            body.pointer(path).unwrap().get("encrypted").is_none(),
            "{path}"
        );
    }
    // Unrelated encrypted fields are preserved.
    assert_eq!(
        body["tools"][1]["parameters"]["properties"]["other"]["encrypted"],
        true
    );
    let item = &body["input"][0];
    // Codex keeps agent_message items and author metadata without is-compat.
    assert_eq!(item["type"], "agent_message");
    assert_eq!(item["author"], "a");
    assert_eq!(
        item["content"][0],
        json!({"type":"input_text","text":"hello"})
    );
    assert_eq!(
        item["content"][1],
        json!({"type":"input_text","text":"keep"})
    );
}

#[test]
fn optimization_requires_flag_and_codex_client() {
    for (cfg, ua) in [
        (config(false, false), CODEX_UA),
        (config(true, false), "curl/8"),
    ] {
        let mut body = collaboration_request();
        let original = body.clone();
        let h = headers(&[("user-agent", ua)]);
        assert!(!prepare_attempt(
            &cfg,
            &selection("codex", json!({})),
            "/v1/responses",
            &h,
            &mut body
        ));
        prepare_boundary(&cfg, "/v1/responses", &h, &mut body);
        assert_eq!(body, original);
    }
}

#[test]
fn namespace_conflict_blocks_rename_but_not_encryption_removal() {
    for conflict in [
        json!({"type":"function","name":"collaboration-optimize__x"}),
        json!({"type":"function","name":"collaboration-optimize.x"}),
        json!({"type":"namespace","name":"other","tools":[{"type":"function","name":"collaboration-optimize"}]}),
    ] {
        let mut body = collaboration_request();
        body["tools"].as_array_mut().unwrap().push(conflict);
        assert!(has_conflict(&body));
        let h = headers(&[("user-agent", CODEX_UA)]);
        assert!(!prepare_attempt(
            &config(true, false),
            &selection("codex", json!({})),
            "/v1/responses",
            &h,
            &mut body
        ));
        assert_eq!(body["tools"][0]["name"], "collaboration");
        assert!(
            body.pointer("/tools/0/tools/0/parameters/properties/message/encrypted")
                .is_none()
        );
    }
    let mut additional = json!({"input":[{"type":"additional_tools","tools":[
        {"type":"function","name":"collaboration-optimize__x"}]}]});
    assert!(has_conflict(&additional));
    assert!(!rename_collaboration_namespace(&mut additional));
}

#[test]
fn additional_tools_are_discovered_and_invalid_containers_ignored() {
    let mut body = json!({"tools":{"type":"function","name":"spawn_agent"},
        "input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"collaboration","tools":[
            {"type":"function","name":"spawn_agent","parameters":{"properties":{"message":{"encrypted":true}}}}]}]},
            {"type":"message","tools":[{"type":"function","name":"send_message",
                "parameters":{"properties":{"message":{"encrypted":true}}}}]}]});
    assert!(remove_message_encryption(&mut body));
    assert!(rename_collaboration_namespace(&mut body));
    assert_eq!(
        body["input"][0]["tools"][0]["name"],
        "collaboration-optimize"
    );
    // Only additional_tools items carry tool definitions.
    assert_eq!(
        body["input"][1]["tools"][0]["parameters"]["properties"]["message"]["encrypted"],
        true
    );
    assert!(!remove_message_encryption(&mut body));
}

#[test]
fn namespace_without_spawn_agent_is_not_renamed() {
    let mut body = json!({"tools":[{"type":"namespace","name":"collaboration","tools":[
        {"type":"function","name":"send_message"}]}]});
    assert!(!rename_collaboration_namespace(&mut body));
    assert_eq!(body["tools"][0]["name"], "collaboration");
}

#[test]
fn compat_codex_model_converts_agent_messages_and_strips_metadata() {
    let auth =
        json!({"metadata":{"credential_options":{"models":[{"name":"m","is-compat":true}]}}});
    let mut body = json!({"model":"m","input":[{"type":"agent_message","author":"a","recipient":"b",
        "internal_chat_message_metadata_passthrough":{},"content":[]},{"type":"message","author":"x"}]});
    let h = headers(&[("user-agent", "curl/8")]);
    assert!(!prepare_attempt(
        &config(false, false),
        &selection("codex", auth),
        "/v1/responses",
        &h,
        &mut body
    ));
    assert_eq!(
        body["input"],
        json!([{"type":"message","role":"user","content":[]},{"type":"message","role":"user"}])
            .as_array()
            .map(|_| body["input"].clone())
            .unwrap()
    );
    // Go RewriteCodexMultiAgentV2InputDoc(isCompat=true) enables optimize too.
    assert_eq!(body["input"][0]["type"], "message");
    assert_eq!(body["input"][0]["role"], "user");
    for key in [
        "author",
        "recipient",
        "internal_chat_message_metadata_passthrough",
    ] {
        assert!(body["input"][0].get(key).is_none(), "{key}");
    }
    assert!(body["input"][1].get("author").is_none());
}

#[test]
fn non_codex_targets_convert_agent_messages() {
    let h = headers(&[("user-agent", CODEX_UA)]);
    let mut body = collaboration_request();
    assert!(!prepare_attempt(
        &config(true, false),
        &selection("claude", json!({})),
        "/v1/responses",
        &h,
        &mut body
    ));
    let item = &body["input"][0];
    assert_eq!(item["type"], "message");
    assert_eq!(item["role"], "user");
    assert_eq!(item["author"], "a");
    assert_eq!(item["content"][0]["text"], "hello");
    // Tool definitions and the namespace stay untouched in the executor stage.
    assert_eq!(body["tools"][0]["name"], "collaboration");

    // Responses-native targets keep agent_message.
    let mut native = collaboration_request();
    prepare_attempt(
        &config(true, false),
        &selection("xai", json!({})),
        "/v1/responses",
        &h,
        &mut native,
    );
    assert_eq!(native["input"][0]["type"], "agent_message");
}

#[test]
fn boundary_removes_encryption_without_renaming() {
    let h = headers(&[("user-agent", CODEX_UA)]);
    let mut body = collaboration_request();
    prepare_boundary(&config(true, false), "/v1/responses", &h, &mut body);
    assert_eq!(body["tools"][0]["name"], "collaboration");
    assert!(
        body.pointer("/tools/0/tools/0/parameters/properties/message/encrypted")
            .is_none()
    );
    assert_eq!(body["input"][0]["type"], "agent_message");
    // Compact and other routes skip multi-agent tool preparation (CPA Compact handler).
    let mut compact = collaboration_request();
    prepare_boundary(
        &config(true, false),
        "/v1/responses/compact",
        &h,
        &mut compact,
    );
    assert_eq!(compact, collaboration_request());
}

fn orphan_request() -> Value {
    json!({"model":"m","input":[
        {"type":"function_call","call_id":"paired","name":"create_thread","namespace":"codex_app"},
        {"type":"function_call_output","call_id":"paired","name":"create_thread","namespace":"codex_app","output":"kept"},
        {"type":"function_call_output","call_id":"paired","name":"create_thread","namespace":"codex_app","output":"second"},
        {"type":"function_call_output","name":"send_message_to_thread","namespace":"codex_app","output":{"ok":true}},
        {"type":"function_call_output","name":"other","namespace":"codex_app","output":"x"},
        {"type":"function_call_output","name":"create_thread","namespace":"other","output":"x"},
        {"type":"function_call_output","name":"create_thread","namespace":"codex_app"}]})
}

#[test]
fn orphan_delegation_rewrites_only_unpaired_codex_app_outputs() {
    let h = headers(&[("x-openai-subagent", "Collab_Spawn")]);
    let mut body = orphan_request();
    prepare_boundary(&config(false, true), "/v1/responses/compact", &h, &mut body);
    let input = body["input"].as_array().unwrap();
    assert_eq!(input[1]["output"], "kept");
    assert_eq!(
        input[2],
        json!({"type":"message","role":"user","content":[{"type":"input_text",
            "text":"Tool output from codex_app__create_thread:\nsecond"}]})
    );
    assert_eq!(
        input[3]["content"][0]["text"],
        "Tool output from codex_app__send_message_to_thread:\n{\"ok\":true}"
    );
    assert_eq!(input[4]["type"], "function_call_output");
    assert_eq!(input[5]["type"], "function_call_output");
    assert_eq!(
        input[6]["content"][0]["text"],
        "Tool output from codex_app__create_thread:\n"
    );
}

#[test]
fn orphan_delegation_requires_flag_and_header() {
    for (cfg, h) in [
        (config(false, true), headers(&[])),
        (
            config(false, true),
            headers(&[("x-openai-subagent", "other")]),
        ),
        (
            config(false, false),
            headers(&[("x-openai-subagent", "collab_spawn")]),
        ),
    ] {
        let mut body = orphan_request();
        prepare_boundary(&cfg, "/v1/responses", &h, &mut body);
        assert_eq!(body, orphan_request());
    }
    // Not a Responses route.
    let mut body = orphan_request();
    prepare_boundary(
        &config(false, true),
        "/v1/chat/completions",
        &headers(&[("x-openai-subagent", "collab_spawn")]),
        &mut body,
    );
    assert_eq!(body, orphan_request());
}

#[test]
fn restore_matches_go_vectors() {
    let payload = br#"{"type":"response.completed","response":{"output":[
        {"type":"function_call","name":"spawn_agent","namespace":"collaboration-optimize","arguments":{"namespace":"collaboration-optimize","name":"collaboration-optimize__opaque"}},
        {"type":"function_call","name":"collaboration-optimize__send_message"},
        {"type":"message","namespace":"collaboration-optimize","name":"collaboration-optimize__plain"},
        {"type":"function_call","name":"collaboration-optimize.spawn_agent","namespace":null,"arguments":"{}"},
        {"type":"custom_tool_call","name":"collaboration-optimize.list_agents","input":"{}"},
        {"type":"function_call_output","output":{"type":"namespace","name":"collaboration-optimize"}}],
      "tools":[{"type":"namespace","name":"collaboration-optimize"}]}}"#;
    let got: Value = serde_json::from_slice(&restore_payload(payload).unwrap()).unwrap();
    let output = &got["response"]["output"];
    assert_eq!(output[0]["namespace"], "collaboration");
    assert_eq!(
        output[0]["arguments"]["namespace"],
        "collaboration-optimize"
    );
    assert_eq!(
        output[0]["arguments"]["name"],
        "collaboration-optimize__opaque"
    );
    assert_eq!(output[1]["name"], "collaboration__send_message");
    assert_eq!(output[2]["namespace"], "collaboration-optimize");
    assert_eq!(output[2]["name"], "collaboration-optimize__plain");
    assert_eq!(output[3]["namespace"], "collaboration");
    assert_eq!(output[3]["name"], "spawn_agent");
    assert_eq!(output[4]["namespace"], "collaboration");
    assert_eq!(output[4]["name"], "list_agents");
    assert_eq!(output[5]["output"]["name"], "collaboration-optimize");
    assert_eq!(got["response"]["tools"][0]["name"], "collaboration");
    assert!(restore_payload(br#"{"type":"message","name":"collaboration"}"#).is_none());
    assert!(restore_payload(b"not json").is_none());
}

#[test]
fn restorer_rewrites_fragmented_sse_and_passes_other_lines() {
    let wire = b"event: response.output_item.done\r\ndata: {\"type\":\"x\",\"item\":{\"type\":\"function_call\",\"name\":\"collaboration-optimize__a\"}}\r\n\r\ndata: [DONE]\n: keep\ndata: {\"type\":\"function_call\",\"namespace\":\"collaboration-optimize\"}";
    let mut restorer = Restorer::default();
    let mut out = Vec::new();
    for chunk in wire.chunks(7) {
        out.extend(restorer.feed(chunk));
    }
    out.extend(restorer.finish());
    let text = String::from_utf8(out).unwrap();
    // Restored payloads are re-encoded with sorted keys, like Go's map marshaling.
    assert_eq!(
        text,
        "event: response.output_item.done\r\ndata: {\"item\":{\"name\":\"collaboration__a\",\"type\":\"function_call\"},\"type\":\"x\"}\r\n\r\ndata: [DONE]\n: keep\ndata: {\"namespace\":\"collaboration\",\"type\":\"function_call\"}"
    );
}

#[tokio::test]
async fn restore_response_wraps_json_body_and_drops_content_length() {
    let body = br#"{"output":[{"type":"function_call","name":"collaboration-optimize__a"}]}"#;
    let upstream = http::Response::builder()
        .status(200)
        .header("content-length", body.len())
        .header("content-type", "application/json")
        .body(reqwest::Body::from(body.to_vec()))
        .unwrap();
    let restored = restore_response(upstream.into(), true);
    assert_eq!(restored.status(), 200);
    assert!(restored.headers().get("content-length").is_none());
    assert_eq!(restored.headers()["content-type"], "application/json");
    let mut bytes = Vec::new();
    let mut stream = restored.bytes_stream();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(
        bytes,
        br#"{"output":[{"name":"collaboration__a","type":"function_call"}]}"#
    );
}
