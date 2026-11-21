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

// Go TestCodexSpawnAgentModelsFromSourcesIncludesModelMetadata.
#[test]
fn codex_models_from_sources_include_metadata() {
    use models::{AvailableModel, ModelInfo, models_from_sources};
    let catalog = r#"{"models":[
        {"slug":"model-template","display_name":"Template","description":"Template model.","default_reasoning_level":"low","supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"}],"service_tiers":[{"id":"priority"}],"priority":1},
        {"slug":"gpt-5.5","display_name":"Default","description":"Default model.","default_reasoning_level":"medium","supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"high"}],"service_tiers":[{"id":"priority"}],"priority":2}
    ]}"#;
    let available = |id: &str, display: &str, description: &str| AvailableModel {
        id: id.into(),
        display_name: display.into(),
        description: description.into(),
    };
    let lookup = |id: &str| {
        (id == "custom-model").then(|| ModelInfo {
            description: "Dynamic model.".into(),
            thinking_levels: ["none", "low", "medium", "high"].map(String::from).to_vec(),
        })
    };
    let got = models_from_sources(
        &[
            available("custom-model", "Custom", "Registry description."),
            available("model-template", "", ""),
            available("custom-model", "", "duplicate"),
        ],
        catalog,
        &lookup,
    );
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].id, "model-template");
    assert_eq!(got[0].description, "Template model.");
    assert_eq!(got[0].default_reasoning_effort, "low");
    assert_eq!(got[0].service_tiers, ["priority"]);
    let custom = &got[1];
    assert_eq!(custom.id, "custom-model");
    assert_eq!(custom.description, "Dynamic model.");
    assert_eq!(custom.reasoning_efforts, ["none", "low", "medium", "high"]);
    assert_eq!(custom.default_reasoning_effort, "medium");
    assert!(custom.service_tiers.is_empty());

    // Without gpt-5.5 the first catalog entry is the default template.
    let without_default = r#"{"models":[
        {"slug":"gpt-6.1-sol","display_name":"First Fallback","description":"First fallback model.","default_reasoning_level":"high","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}],"service_tiers":[{"id":"priority"}],"priority":1},
        {"slug":"gpt-5.6-sol","display_name":"Second","description":"Second model.","default_reasoning_level":"medium","supported_reasoning_levels":[{"effort":"medium"}],"priority":2}
    ]}"#;
    let fallback = models_from_sources(
        &[available("third-party-model", "", "")],
        without_default,
        &|_| None,
    );
    assert_eq!(fallback.len(), 1);
    assert_eq!(fallback[0].id, "third-party-model");
    assert_eq!(fallback[0].default_reasoning_effort, "high");
    assert_eq!(fallback[0].description, "third-party-model");
    assert!(models_from_sources(&[available("x", "", "")], "{}", &|_| None).is_empty());
}

// Go TestDecodeCodexHomeAvailableModels.
#[test]
fn codex_models_decode_home_available_models_matches_go() {
    let raw = json!({
        "codex":[{"id":"model-b","display_name":"Model B"},{"id":"model-a"}],
        "other":[{"name":"models/model-c","displayName":"Model C"},{"id":"model-a","display_name":"duplicate"}]
    });
    let got = models::decode_home_available_models(&raw);
    let ids: Vec<_> = got.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["model-a", "model-b", "model-c"]);
    assert_eq!(got[1].description, "Model B");
    assert_eq!(got[2].display_name, "Model C");
    assert!(
        models::decode_home_available_models(&json!({"error":{"type":"no_credentials"}}))
            .is_empty()
    );
}

fn spawn_models() -> Vec<models::SpawnAgentModel> {
    vec![
        models::SpawnAgentModel {
            id: "model-alpha".into(),
            description: "Alpha model.".into(),
            reasoning_efforts: ["low", "medium", "high"].map(String::from).to_vec(),
            default_reasoning_effort: "medium".into(),
            service_tiers: vec!["priority".into()],
            ..Default::default()
        },
        models::SpawnAgentModel {
            id: "model-beta".into(),
            description: "Beta model".into(),
            reasoning_efforts: ["low", "high"].map(String::from).to_vec(),
            default_reasoning_effort: "low".into(),
            ..Default::default()
        },
    ]
}

// Go TestRewriteCodexSpawnAgentDescriptionNormalizesModelList.
#[test]
fn spawn_agent_description_normalizes_model_list() {
    let h = headers(&[("user-agent", CODEX_UA)]);
    let mut body = json!({"input":[{"type":"additional_tools","role":"developer","tools":[{
        "type":"namespace","name":"collaboration","tools":[
            {"type":"function","name":"send_message","description":"unchanged"},
            {"type":"function","name":"spawn_agent","description":"\n        Available model overrides (optional; inherited parent model is preferred):\n- old duplicate\n- old duplicate\n        Spawns an agent to work on a task.","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}}]}]}]});
    prepare_boundary_with_models(
        &config(true, false),
        "/v1/responses",
        &h,
        &mut body,
        &spawn_models(),
    );
    let tools = &body["input"][0]["tools"][0]["tools"];
    assert_eq!(
        tools[1]["description"],
        "\n        Available model overrides (optional; inherited parent model is preferred):\n\
         - `model-alpha`: Alpha model. Reasoning efforts: low, medium (default), high. Service tiers: priority.\n\
         - `model-beta`: Beta model. Reasoning efforts: low (default), high.\n        Spawns an agent to work on a task."
    );
    assert_eq!(tools[0]["description"], "unchanged");
    assert!(
        tools[1]
            .pointer("/parameters/properties/message/encrypted")
            .is_none()
    );
    // Namespace is renamed only at the executor stage.
    assert_eq!(body["input"][0]["tools"][0]["name"], "collaboration");
}

// Go TestRewriteCodexSpawnAgentDescriptionTopLevelWithoutMarker.
#[test]
fn spawn_agent_description_appends_without_marker() {
    let mut body = json!({"tools":[{"type":"namespace","name":"collaboration","tools":[
        {"type":"function","name":"spawn_agent","description":"Create a worker."}]}]});
    let list = models::format_models(&[models::SpawnAgentModel {
        id: "model-a".into(),
        description: "Model A.".into(),
        reasoning_efforts: vec!["medium".into()],
        default_reasoning_effort: "medium".into(),
        ..Default::default()
    }]);
    assert!(rewrite_spawn_agent_descriptions(&mut body, &list));
    assert_eq!(
        body["tools"][0]["tools"][0]["description"],
        format!(
            "Create a worker.\n\n{}\n- `model-a`: Model A. Reasoning efforts: medium (default).",
            models::MODELS_HEADING
        )
    );
}

// Go TestRewriteCodexSpawnAgentDescriptionWithoutModelsStillRemovesEncrypted
// and ...LeavesPayloadWithoutToolUnchanged.
#[test]
fn spawn_agent_description_without_models_or_tool_is_unchanged() {
    let h = headers(&[("user-agent", CODEX_UA)]);
    let mut body = json!({"tools":[{"type":"function","name":"spawn_agent","description":"unchanged",
        "parameters":{"properties":{"message":{"encrypted":true}}}}]});
    prepare_boundary_with_models(&config(true, false), "/v1/responses", &h, &mut body, &[]);
    assert_eq!(body["tools"][0]["description"], "unchanged");
    assert!(
        body.pointer("/tools/0/parameters/properties/message/encrypted")
            .is_none()
    );

    let mut other = json!({"tools":[{"type":"function","name":"other","description":"unchanged"}]});
    let original = other.clone();
    let list = models::format_models(&spawn_models());
    assert!(!rewrite_spawn_agent_descriptions(&mut other, &list));
    assert_eq!(other, original);
    assert!(!needs_spawn_agent_models(
        &config(true, false),
        "/v1/responses",
        &h,
        &other
    ));
}

// Conflicting requests only lose encryption (CPA hasCodexOptimizedCollaborationConflictDoc).
#[test]
fn spawn_agent_description_skips_namespace_conflict_and_other_routes() {
    let h = headers(&[("user-agent", CODEX_UA)]);
    let request = || {
        json!({"tools":[{"type":"namespace","name":"collaboration","tools":[
            {"type":"function","name":"spawn_agent","description":"Spawns an agent."}]},
            {"type":"namespace","name":"collaboration-optimize","tools":[]}]})
    };
    let mut body = request();
    assert!(!needs_spawn_agent_models(
        &config(true, false),
        "/v1/responses",
        &h,
        &body
    ));
    prepare_boundary_with_models(
        &config(true, false),
        "/v1/responses",
        &h,
        &mut body,
        &spawn_models(),
    );
    assert_eq!(body, request());

    let mut compact = collaboration_request();
    compact["tools"][0]["tools"][0]["description"] = "Spawns an agent.".into();
    let original = compact.clone();
    assert!(!needs_spawn_agent_models(
        &config(true, false),
        "/v1/responses/compact",
        &h,
        &compact
    ));
    prepare_boundary_with_models(
        &config(true, false),
        "/v1/responses/compact",
        &h,
        &mut compact,
        &spawn_models(),
    );
    assert_eq!(compact, original);
    for (cfg, ua) in [
        (config(false, false), CODEX_UA),
        (config(true, false), "curl/8"),
    ] {
        let h = headers(&[("user-agent", ua)]);
        assert!(!needs_spawn_agent_models(
            &cfg,
            "/v1/responses",
            &h,
            &original
        ));
    }
    assert!(needs_spawn_agent_models(
        &config(true, false),
        "/v1/responses",
        &h,
        &original
    ));
}

// Go TestReplaceCodexSpawnAgentModelsNormalizesSectionsAndPreservesInstructions.
#[test]
fn replace_models_normalizes_sections_and_preserves_instructions() {
    let heading = models::MODELS_HEADING;
    let description = format!(
        "{heading}\n- `old-model`: old\nKeep this multi-agent instruction.\nSpawns an agent.\n{heading}"
    );
    let got = models::replace_models(&description, "- `new-model`: New model.");
    assert!(!got.contains("old-model"), "{got}");
    assert_eq!(got.matches(heading).count(), 1, "{got}");
    assert!(got.contains("Keep this multi-agent instruction."), "{got}");
    assert_eq!(models::replace_models("keep", ""), "keep");
}

#[test]
fn format_models_handles_backticks_whitespace_and_empty_ids() {
    let got = models::format_models(&[
        models::SpawnAgentModel {
            id: "a`b".into(),
            description: "  Multi\n  line  ".into(),
            ..Default::default()
        },
        models::SpawnAgentModel {
            id: "  ".into(),
            ..Default::default()
        },
        models::SpawnAgentModel {
            id: "plain".into(),
            description: "Done!".into(),
            service_tiers: vec!["flex".into(), "priority".into()],
            ..Default::default()
        },
    ]);
    assert_eq!(
        got,
        "- `` a`b ``: Multi line.\n- `plain`: Done! Service tiers: flex, priority."
    );
}

// Home mode (Go TestRewriteCodexSpawnAgentDescriptionEnabledOptimizesTool with
// the model supplied by Home instead of the local registry).
#[test]
fn codex_models_home_resolves_against_embedded_catalog() {
    assert!(
        models::embedded_templates().is_some(),
        "embedded catalog parses"
    );
    let raw = json!({
        "codex-pro":[{"id":"gpt-5.5","display_name":"ignored"}],
        "claude":[{"id":"codex-spawn-agent-test-model","description":"Test agent model.",
            "thinking":{"levels":["low","medium","high"]}}]
    });
    let got = models::models_from_home(&raw);
    assert_eq!(got.len(), 2);
    // Catalog-backed models come first and keep catalog metadata.
    assert_eq!(got[0].id, "gpt-5.5");
    assert_eq!(
        got[0].description,
        "GPT-6 Chat Thinking for the paid CCA experiment."
    );
    assert_eq!(got[0].default_reasoning_effort, "medium");
    let list = models::format_models(&got);
    assert!(
        list.contains(
            "- `codex-spawn-agent-test-model`: Test agent model. Reasoning efforts: low, medium (default), high."
        ),
        "{list}"
    );
    // Without Home thinking levels, synthesized models inherit the default template.
    let plain = models::models_from_home(&json!({"x":[{"id":"zz-plain"}]}));
    assert_eq!(plain[0].description, "zz-plain");
    assert!(!plain[0].reasoning_efforts.is_empty());
    assert!(plain[0].service_tiers.is_empty());

    assert!(models::models_from_home(&json!({"error":{"type":"unauthorized"}})).is_empty());

    let h = headers(&[("user-agent", CODEX_UA)]);
    let mut body = json!({"tools":[{"type":"namespace","name":"collaboration","tools":[
        {"type":"function","name":"spawn_agent","description":"Spawns an agent.",
         "parameters":{"properties":{"message":{"type":"string","encrypted":true}}}}]}]});
    prepare_boundary_with_models(&config(true, false), "/v1/responses", &h, &mut body, &got);
    assert!(prepare_attempt(
        &config(true, false),
        &selection("codex", json!({})),
        "/v1/responses",
        &h,
        &mut body
    ));
    assert_eq!(body["tools"][0]["name"], "collaboration-optimize");
    let description = body["tools"][0]["tools"][0]["description"]
        .as_str()
        .unwrap();
    assert!(
        description.starts_with(models::MODELS_HEADING),
        "{description}"
    );
    assert!(description.ends_with("\nSpawns an agent."), "{description}");
    assert!(description.contains(&list), "{description}");
}

#[test]
fn codex_models_catalog_provenance_and_invalid_sources() {
    use sha2::{Digest, Sha256};
    let catalog = include_bytes!("codex_models_catalog.json");
    assert_eq!(
        hex::encode(Sha256::digest(catalog)),
        "5088dd5ed900cf7a420ba4651af7f747130d6380a7fe23365fd1b02f0ac2ca8c"
    );
    for raw in [
        "invalid",
        "null",
        "{}",
        r#"{"models":[]}"#,
        r#"{"models":[null,{}]}"#,
        r#"{"models":[{"slug":"valid"},42]}"#,
    ] {
        assert!(models::parse_templates(raw).is_none(), "{raw}");
    }
    for raw in [
        json!(null),
        json!([]),
        json!({"x":{}}),
        json!({"x":[42]}),
        json!({"valid":[{"id":"hidden"}],"error":"unauthorized"}),
    ] {
        assert!(models::models_from_home(&raw).is_empty(), "{raw}");
    }
    let got = models::decode_home_available_models(&json!({"empty":null,"x":[null,
        {"id":"  ","name":"models/fallback","displayName":" Fallback "},
        {"id":" explicit ","name":"models/ignored"}]}));
    assert_eq!(
        got.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["explicit", "fallback"]
    );
    assert_eq!(got[1].description, "Fallback");
}

#[test]
fn codex_models_normalize_metadata_and_sort_profiles() {
    let catalog = r#"{"models":[
        {"slug":"z","priority":1},
        {"slug":"a","priority":1},
        {"slug":"gpt-5.5","default_reasoning_level":"invalid",
         "supported_reasoning_levels":[{"effort":" LOW "},{"effort":"invalid"},{"effort":"ULTRA"}],
         "service_tiers":[null,{"id":" priority "},{"id":"priority"},{"id":""},{"id":"flex"}]}
    ]}"#;
    let available = |id: &str, display: &str| models::AvailableModel {
        id: id.into(),
        display_name: display.into(),
        ..Default::default()
    };
    let profiles = models::models_from_sources(
        &[
            available("z", ""),
            available("custom-z", "Beta"),
            available("a", ""),
            available("custom-b", "alpha"),
            available("custom-a", "Alpha"),
            available("gpt-5.5", ""),
            available(" a ", "duplicate"),
            available(" ", ""),
        ],
        catalog,
        &|id| {
            (id == "custom-a").then(|| models::ModelInfo {
                thinking_levels: vec!["invalid".into(), " NONE ".into()],
                ..Default::default()
            })
        },
    );
    assert_eq!(
        profiles.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["gpt-5.5", "a", "z", "custom-a", "custom-b", "custom-z"]
    );
    assert_eq!(profiles[0].reasoning_efforts, ["low", "ultra"]);
    assert_eq!(profiles[0].default_reasoning_effort, "low");
    assert_eq!(profiles[0].service_tiers, ["priority", "flex"]);
    assert_eq!(profiles[3].reasoning_efforts, ["none"]);
    assert_eq!(profiles[3].default_reasoning_effort, "none");
    assert!(profiles[3].service_tiers.is_empty());
    assert_eq!(profiles[4].reasoning_efforts, ["low", "ultra"]);
}

#[tokio::test]
async fn codex_models_runtime_catalog_changes_spawn_agent_description() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    let mut catalog: Value =
        serde_json::from_str(include_str!("codex_models_catalog.json")).unwrap();
    for model in catalog["models"].as_array_mut().unwrap() {
        if model["slug"] == "gpt-5.5" {
            model["description"] = "Runtime catalog description".into();
        }
    }
    std::fs::write(&path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    let mut cfg = config(true, false);
    let raw = serde_yaml::from_str(&format!(
        "models: {{codex-catalog: {:?}}}",
        path.to_str().unwrap()
    ))
    .unwrap();
    cfg.providers.codex_catalog =
        crate::config::providers::codex_catalog::Catalog::parse(&raw).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut updater = crate::config::providers::codex_catalog::Updater::start(
        cfg.providers.codex_catalog.clone(),
        &cancel,
    );
    let expected = serde_json::to_string(&catalog).unwrap();
    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = cfg.providers.codex_catalog.snapshot(&cancel).await.unwrap();
            if &*snapshot == expected {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    updater.stop().await;
    let templates = models::parse_templates(&snapshot).unwrap();
    let profiles =
        models::models_from_home_templates(&json!({"codex":[{"id":"gpt-5.5"}]}), &templates);
    let mut body =
        json!({"tools":[{"type":"function","name":"spawn_agent","description":"Spawns an agent"}]});
    let h = headers(&[("user-agent", CODEX_UA)]);
    prepare_boundary_with_models(&cfg, "/v1/responses", &h, &mut body, &profiles);
    assert!(
        body["tools"][0]["description"]
            .as_str()
            .unwrap()
            .contains("Runtime catalog description")
    );
}

#[test]
fn codex_models_request_preserves_auth_scope_and_empty_client_version() {
    let h = headers(&[
        ("authorization", "Bearer test-caller"),
        ("x-api-key", "test-key"),
        ("user-agent", CODEX_UA),
        ("x-untrusted-header", "do-not-forward"),
    ]);
    let request = spawn_agent_models_request(&h);
    assert_eq!(request["type"], "models");
    assert_eq!(request["query"], json!({"client_version":""}));
    assert_eq!(
        request["headers"]["authorization"],
        json!(["Bearer test-caller"])
    );
    assert_eq!(request["headers"]["x-api-key"], json!(["test-key"]));
    assert!(request["headers"].get("x-untrusted-header").is_none());
}

#[test]
fn spawn_agent_descriptions_are_idempotent_and_require_string_descriptions() {
    let h = headers(&[("user-agent", CODEX_UA)]);
    let cfg = config(true, false);
    let mut body = json!({"tools":[
        {"type":"function","name":"spawn_agent"},
        {"type":"function","name":"spawn_agent","description":null},
        {"type":"function","name":"spawn_agent","description":42}
    ]});
    assert!(!needs_spawn_agent_models(&cfg, "/v1/responses", &h, &body));
    body["input"] = json!([{"type":"additional_tools","tools":[
        {"type":"namespace","name":"nested","tools":[
            {"type":"function","name":"spawn_agent","description":"Spawns an agent."}
        ]}
    ]}]);
    assert!(needs_spawn_agent_models(&cfg, "/v1/responses", &h, &body));
    let profiles = spawn_models();
    prepare_boundary_with_models(&cfg, "/v1/responses", &h, &mut body, &profiles);
    let first = body.clone();
    prepare_boundary_with_models(&cfg, "/v1/responses", &h, &mut body, &profiles);
    assert_eq!(body, first);
    assert!(body["tools"][0].get("description").is_none());
    assert_eq!(body["tools"][1]["description"], Value::Null);
    assert_eq!(body["tools"][2]["description"], 42);
}
