use super::*;
use serde_json::json;

fn rules(yaml: &str) -> PayloadRules {
    PayloadRules::parse(&serde_yaml::from_str(yaml).unwrap()).unwrap()
}

#[test]
fn go_stage_order_first_default_last_override_filter_final() {
    let rules = rules(
        r#"
default:
  - models: [{name: "gpt-*"}]
    params: {temperature: 0.5, missing: 1, present: 7}
  - models: [{name: "*"}]
    params: {missing: 2}
default-raw:
  - models: [{name: "*"}]
    params: {missing: '3', object: '{"x":true}'}
override:
  - models: [{name: "*"}]
    params: {temperature: 1, removed: true}
  - models: [{name: "*"}]
    params: {temperature: 2}
override-raw:
  - models: [{name: "*"}]
    params: {temperature: '3', array: '[1,2]'}
filter:
  - models: [{name: "*"}]
    params: [removed]
"#,
    );
    let mut body = json!({"present":null});
    rules.apply(&mut body, "gpt-5", "responses").unwrap();
    assert_eq!(
        body,
        json!({"present":null,"missing":1,"temperature":3,"object":{"x":true},"array":[1,2]})
    );
}

#[test]
fn model_and_protocol_gates_follow_go_contract() {
    let r = rules(
        r#"
override:
  - models: [{name: "gpt-*", protocol: "Responses"}]
    params: {enabled: true}
"#,
    );
    for (model, protocol, matched) in [
        ("gpt-5", "responses", true),
        ("GPT-5", "responses", false),
        ("gpt-55", "responses", true),
        ("gpt-5", "gemini", false),
        ("gpt-5", "", true),
    ] {
        let mut body = json!({});
        r.apply(&mut body, model, protocol).unwrap();
        assert_eq!(body.get("enabled").is_some(), matched);
    }
    assert!(path::glob("a*b*c", "axbyc"));
    assert!(path::glob("a?c", "aéc"));
    assert!(path::glob(r"a\*c", "a*c"));
}

#[test]
fn nested_paths_escaped_keys_arrays_and_wildcards() {
    let r = rules(
        r#"
override:
  - models: [{name: "*"}]
    params:
      'settings.a\.b': 1
      'messages.#.temperature': 2
      'groups.*.enabled': true
      'items.0.name': first
  - models: [{name: "*"}]
    params:
      'items.-1.name': second
filter:
  - models: [{name: "*"}]
    params: ['messages.#.secret', 'drop.#']
"#,
    );
    let mut body =
        json!({"messages":[{"secret":"a"},{"secret":"b"}],"groups":{"a":{},"b":{}},"drop":[1,2,3]});
    r.apply(&mut body, "model", "openai").unwrap();
    assert_eq!(
        body,
        json!({"settings":{"a.b":1},"messages":[{"temperature":2},{"temperature":2}],
        "groups":{"a":{"enabled":true},"b":{"enabled":true}},"items":[{"name":"first"},{"name":"second"}],"drop":[]})
    );
}

#[test]
fn unsupported_features_and_invalid_raw_fail_at_parse() {
    for yaml in [
        "replace: [{models: [{name: '*'}], target: {role: user}, replacements: [{a: b}]}]",
        "override: [{models: [{name: '*', source-protocol: openai}], params: {x: 1}}]",
        "override: [{models: [{name: '*'}], params: {'messages.#(role==user.x': 1}}]",
        "override-raw: [{models: [{name: '*'}], params: {x: 'not json'}}]",
        "filter: [{models: [{name: '*'}], params: ['x..y']}]",
        "override: [{models: [{name: '*'}], params: {'x\\': 1}}]",
    ] {
        let value = serde_yaml::from_str(yaml).unwrap();
        assert!(PayloadRules::parse(&value).is_err(), "{yaml}");
    }
}

#[test]
fn application_errors_are_atomic_and_sparse_indices_are_bounded() {
    let r = rules("override: [{models: [{name: '*'}], params: {a: 1, z.child: 2}}]");
    let mut body = json!({"z": true});
    let original = body.clone();
    assert!(r.apply(&mut body, "model", "").is_err());
    assert_eq!(body, original);
    let r = rules("override: [{models: [{name: '*'}], params: {'x.999999999999': 1}}]");
    assert!(r.apply(&mut body, "model", "").is_err());
}

#[test]
fn empty_models_do_not_match_and_default_is_noop() {
    let mut body = json!({});
    rules("override: [{models: [], params: {x: 1}}]")
        .apply(&mut body, "a", "b")
        .unwrap();
    assert_eq!(body, json!({}));
    PayloadRules::default().apply(&mut body, "a", "b").unwrap();
    assert_eq!(body, json!({}));
}
