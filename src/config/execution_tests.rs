use super::*;
use crate::config::execution::{ErrorAction, RequestScopedRule};
use serde_json::json;

fn config(extra: &str) -> Config {
    Config::parse(
        format!("credential-concurrency: {{lifecycle-config-revision: 1}}\n{extra}").as_bytes(),
    )
    .unwrap()
}

#[test]
fn home_execution_layouts_and_support_boundary() {
    for extra in [
        "shutdown-timeout: 30\nforce-model-prefix: true\nstreaming: {bootstrap-retries: 5}\npassthrough-headers: true\noauth-request-scoped-errors: {codex: [{status: 404, match: [missing], action: stop}]}\n",
        "server: {shutdown-timeout: 30}\nrouting: {force-model-prefix: true}\nrequests: {streaming: {bootstrap-retries: 5}, passthrough-headers: true}\noauth: {request-scoped-errors: {codex: [{status: 404, match: [missing], action: stop}]}}\n",
    ] {
        let c = config(extra);
        assert_eq!(c.shutdown_timeout, Duration::from_secs(30));
        assert!(c.force_model_prefix);
        assert!(c.passthrough_headers);
        assert_eq!(c.streaming.bootstrap_retries, 5);
        // Every control in these Home layouts is consumed by the worker.
        assert!(c.unsupported_features().is_empty());
        let rules =
            execution::resolve_rules(&c, " Codex ", &json!({"attributes":{"auth_kind":"oauth"}}));
        assert_eq!(
            rules[0].matches(404, "missing item"),
            Some(ErrorAction::Stop)
        );
        assert!(
            execution::resolve_rules(&c, "codex", &json!({"attributes":{"api_key":"fixture"}}))
                .is_empty()
        );
        let override_auth = json!({"metadata":{"request_scoped_errors":[{"status":429,"match":["limit"],"action":"continue-and-cooldown"}]}});
        let rules = execution::resolve_rules(&c, "codex", &override_auth);
        assert_eq!(
            rules[0].matches(429, "limit"),
            Some(ErrorAction::ContinueAndCooldown)
        );
        assert_eq!(rules[0].matches(404, "missing"), None);
    }
    assert_eq!(config("").shutdown_timeout, Duration::ZERO);
    assert_eq!(
        config("shutdown-timeout: -1\n").shutdown_timeout,
        Duration::ZERO
    );
    assert!(Config::parse(b"credential-concurrency: {lifecycle-config-revision: 1}\nshutdown-timeout: 2\nserver: {shutdown-timeout: 3}").is_err());
}

#[test]
fn matching_is_ordered_case_sensitive_and_never_reports_raw_body() {
    let rules: Vec<RequestScopedRule> = serde_json::from_value(json!([
        {"status":400,"match":["secret"],"action":"invalid"},
        {"status":400,"match-regexr":["[", "secret-[0-9]+"],"action":" STOP-AND-COOLDOWN "},
        {"status":400,"match":["secret"],"action":"stop"}
    ]))
    .unwrap();
    let mut usage = crate::reporting::UsageRecord::default();
    let action = usage
        .upstream_failure_with_rules(400, "secret-12345", &rules)
        .unwrap();
    assert!(action.stops());
    assert_eq!(usage.fail_code, "force_cooldown");
    assert!(!usage.fail_body.contains("secret"));
    assert_eq!(rules[1].matches(400, "SECRET-12345"), None);
    assert_eq!(rules[1].matches(500, "secret-12345"), None);
    assert_eq!(
        usage.upstream_failure_with_rules(400, "other", &rules),
        None
    );
    assert!(usage.fail_code.is_empty());
    assert_eq!(ErrorAction::Continue.fail_code(), "request_scoped");
    assert!(!ErrorAction::Continue.stops());
}
