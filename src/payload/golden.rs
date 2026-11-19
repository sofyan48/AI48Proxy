//! Golden cases transcribed from the current Go payload helper contract.
use super::*;
use serde_json::json;

fn rules(s: &str) -> PayloadRules {
    PayloadRules::parse(&serde_yaml::from_str(s).unwrap()).unwrap()
}

#[test]
fn go_model_and_header_globs_only_expand_star() {
    let r = rules(
        "override: [{models: [{name: 'gpt-?', headers: {X: 'agent-?'}}], params: {ok: true}}]",
    );
    let mut headers = http::HeaderMap::new();
    headers.insert("x", " agent-? ".parse().unwrap());
    let mut body = json!({});
    let ctx = ApplyContext {
        model: "gpt-5",
        headers: Some(&headers),
        ..Default::default()
    };
    r.apply_context(&mut body, ctx).unwrap();
    assert_eq!(body, json!({}));
    r.apply_context(
        &mut body,
        ApplyContext {
            model: "gpt-?",
            ..ctx
        },
    )
    .unwrap();
    assert_eq!(body, json!({"ok":true}));
}

#[test]
fn root_and_original_defaults_preserve_context() {
    let r = rules(
        "default: [{models: [{name: '*'}], params: {existing: 1, created: 2}}]\nreplace: [{models: [{name: '*'}], target: {role: user, all: true}, replacements: [{old: new}]}]",
    );
    let original = json!({"request":{"existing":99}});
    let mut body = json!({"request":{"input":"old"}});
    r.apply_context(
        &mut body,
        ApplyContext {
            model: "m",
            protocol: "responses",
            root: "request",
            original: Some(&original),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(body, json!({"request":{"input":"new","created":2}}));
}

#[test]
fn go_replace_order_regex_captures_duplicates_and_filter_barrier() {
    let r = rules(
        r#"
override:
  - models: [{name: '*'}]
    params: {marker: ready}
replace:
  - models: [{name: '*', match: [{marker: ready}]}]
    target: {paths: ['messages.#(role=="user")#.content', 'messages.0.content']}
    replacements:
      - {secret: 'token-42'}
      - {pattern: 'token-(?P<id>[0-9]+)', replacement: '${id}/$1/$$'}
      - {pattern: '[invalid', replacement: ignored}
filter:
  - models: [{name: '*', match: [{'messages.0.content': '42/42/$'}]}]
    params: [marker]
"#,
    );
    let mut body = json!({"messages":[{"role":"user","content":"secret"},{"role":"assistant","content":"secret"}]});
    r.apply(&mut body, "m", "openai").unwrap();
    assert_eq!(
        body,
        json!({"messages":[{"role":"user","content":"42/42/$"},{"role":"assistant","content":"secret"}]})
    );
}

#[test]
fn go_gates_are_conjunctive_and_observe_prior_writes() {
    let r = rules(
        r#"
override:
  - models: [{name: '*'}]
    params: {phase: 1}
  - models:
      - name: alias
        protocol: OpenAI
        from-protocol: openai-response
        headers: {' X-Client ': 'agent-*', X-Mode: strict}
        match: [{phase: 1.0}, {'items.#(kind=="a"&&n>=2)#.value': {x: [1.0, true]}}]
        not-match: [{missing: null}, {phase: 2}]
        exist: [phase, 'items.#(kind=="a")#.value']
        not-exist: [missing, nullable]
    params: {gated: true}
"#,
    );
    let input = json!({"nullable":null,"items":[{"kind":"a","n":2,"value":{"x":[1,true]}}]});
    let mut headers = http::HeaderMap::new();
    headers.append("x-client", "other".parse().unwrap());
    headers.append("x-client", "agent-test".parse().unwrap());
    headers.insert("x-mode", "strict".parse().unwrap());
    let mut body = input.clone();
    let ctx = ApplyContext {
        model: "upstream",
        requested_model: "alias(high)",
        protocol: "openai",
        from_protocol: "responses",
        headers: Some(&headers),
        ..Default::default()
    };
    r.apply_context(&mut body, ctx).unwrap();
    assert_eq!(body["gated"], true);
    for ctx in [
        ApplyContext {
            headers: None,
            ..ctx
        },
        ApplyContext {
            from_protocol: "",
            ..ctx
        },
        ApplyContext {
            protocol: "claude",
            ..ctx
        },
    ] {
        let mut b = input.clone();
        r.apply_context(&mut b, ctx).unwrap();
        assert!(b.get("gated").is_none());
    }
    let mut body = input;
    r.apply(&mut body, "alias", "openai").unwrap();
    assert!(body.get("gated").is_none());
}

#[test]
fn go_roles_count_uneditable_messages_and_preserve_nontext() {
    let r = rules(
        r#"
replace:
  - models: [{name: '*'}]
    target: {role: user, positions: [2]}
    replacements: [{old: new}]
"#,
    );
    let mut body = json!({"messages":[{"role":"user","content":[{"type":"image","text":"old"}]},{"role":"user","content":[{"type":"text","text":"old"},{"type":"text","text":"old","thought":true},{"type":"image","text":"old"}]}]});
    r.apply(&mut body, "m", "claude").unwrap();
    assert_eq!(body["messages"][1]["content"][0]["text"], "new");
    assert_eq!(body["messages"][1]["content"][1]["text"], "old");
    assert_eq!(body["messages"][0]["content"][0]["text"], "old");
}

#[test]
fn go_top_level_system_and_protocol_shapes() {
    let r = rules(
        "replace: [{models: [{name: '*'}], target: {role: system, positions: [2]}, replacements: [{old: new}]}]",
    );
    for (protocol, mut body, key) in [
        (
            "claude",
            json!({"system":[],"messages":[{"role":"system","content":"old"}]}),
            "messages",
        ),
        (
            "responses",
            json!({"instructions":false,"input":[{"type":"message","role":"system","content":[{"type":"input_text","text":"old"}]}]}),
            "input",
        ),
        (
            "gemini",
            json!({"system_instruction":{},"contents":[{"role":"system","parts":[{"text":"old"},{"text":"old","functionCall":{}},{"text":"old","thought":true}]}]}),
            "contents",
        ),
    ] {
        r.apply(&mut body, "m", protocol).unwrap();
        let text = if protocol == "claude" {
            &body[key][0]["content"]
        } else if protocol == "gemini" {
            &body[key][0]["parts"][0]["text"]
        } else {
            &body[key][0]["content"][0]["text"]
        };
        assert_eq!(text, "new", "{protocol}");
        if protocol == "gemini" {
            assert_eq!(body[key][0]["parts"][1]["text"], "old");
        }
    }
}

#[test]
fn go_paths_nested_queries_first_match_escaped_keys_and_snapshot_dedup() {
    let r = rules(
        r#"
replace:
  - models: [{name: '*'}]
    target: {paths: ['groups.#(enabled==true)#.items.#(role=="user").content', 'a\.b', 'groups.1.items.0.content']}
    replacements: [{a: aa}]
"#,
    );
    let mut body = json!({"a.b":"a","groups":[{"enabled":true,"items":[{"role":"user"},{"role":"user","content":"a"}]},{"enabled":true,"items":[{"role":"user","content":"a"},{"role":"user","content":"a"}]}]});
    r.apply(&mut body, "m", "openai").unwrap();
    assert_eq!(body["a.b"], "aa");
    assert_eq!(body["groups"][0]["items"][1]["content"], "a");
    assert_eq!(body["groups"][1]["items"][0]["content"], "aa");
    assert_eq!(body["groups"][1]["items"][1]["content"], "a");
}
