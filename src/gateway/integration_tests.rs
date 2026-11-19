use super::*;
use crate::{
    home::client::Selection,
    reporting::{QueueLimits, ReportingQueue},
};
use std::{sync::Arc, time::Duration};

fn config(extra: &str) -> crate::config::Config {
    crate::config::Config::parse(
        format!("credential-concurrency:\n  lifecycle-config-revision: 1\n{extra}").as_bytes(),
    )
    .unwrap()
}

fn selection() -> Selection {
    serde_json::from_value(
        json!({"model":"resolved", "provider":"custom", "auth_index":"index",
        "user_api_key":"caller-principal", "auth":{"id":"credential", "attributes":{
            "api_key":"provider-secret", "base_url":"https://trusted.invalid/v1"}}}),
    )
    .unwrap()
}

fn prepared_body(
    selection: &Selection,
    body: Value,
    config: &crate::config::Config,
) -> anyhow::Result<Value> {
    let prepared = upstream::prepare(
        config,
        selection,
        "/v1/chat/completions",
        &HeaderMap::new(),
        body,
        false,
    )?;
    let request = prepared.request.build()?;
    Ok(serde_json::from_slice(
        request.body().unwrap().as_bytes().unwrap(),
    )?)
}

#[test]
fn plain_requests_allow_missing_metadata_but_explicit_reasoning_requires_it() {
    let selected = selection();
    let plain = prepared_body(
        &selected,
        json!({"model":"alias","messages":[]}),
        &config(""),
    )
    .unwrap();
    assert_eq!(plain["model"], "resolved");
    for body in [
        json!({"model":"alias(high)"}),
        json!({"model":"alias","reasoning_effort":"high"}),
    ] {
        let err = prepared_body(&selected, body, &config("")).unwrap_err();
        assert!(
            err.downcast_ref::<crate::thinking::ThinkingError>()
                .is_some()
        );
    }
}

#[test]
fn original_suffix_wins_then_payload_rules_are_the_final_barrier() {
    let mut selected = selection();
    selected.model_info = Some(json!({"thinking":{"levels":["low","high"]}}));
    let value = prepared_body(
        &selected,
        json!({"model":"alias(high)", "reasoning_effort":"low"}),
        &config(""),
    )
    .unwrap();
    assert_eq!(value["model"], "resolved");
    assert_eq!(value["reasoning_effort"], "high");
    let value = prepared_body(&selected, json!({"model":"alias(high)"}), &config(
        "payload:\n  override:\n    - models: [{name: resolved, protocol: openai}]\n      params: {reasoning_effort: low, model: configured}\n"
    )).unwrap();
    assert_eq!(value["reasoning_effort"], "low");
    assert_eq!(value["model"], "configured");
    let value = prepared_body(&selected, json!({"model":"alias(high)"}), &config(
        "payload:\n  filter:\n    - models: [{name: resolved}]\n      params: [reasoning_effort]\n"
    )).unwrap();
    assert!(value.get("reasoning_effort").is_none());
}

#[test]
fn attribution_uses_downstream_principal_original_alias_and_only_token_digest() {
    let mut selected = selection();
    selected.auth["metadata"] = json!({"token":{"accessToken":" observed-token "}});
    let fields = lifecycle::attribution(&selected, "alias(high)");
    assert_eq!(fields["api_key"], "caller-principal");
    assert_eq!(fields["alias"], "alias(high)");
    use sha2::{Digest, Sha256};
    assert_eq!(
        fields["access_token_sha256"],
        hex::encode(Sha256::digest(b"observed-token"))
    );
    assert!(!fields.to_string().contains("provider-secret"));
    assert!(!fields.to_string().contains("observed-token"));
    assert!(fields.get("user_api_key").is_none());
    selected.auth["metadata"] = json!({});
    assert!(
        lifecycle::attribution(&selected, "alias")
            .get("access_token_sha256")
            .is_none()
    );
}

#[test]
fn session_hints_survive_home_and_upstream_without_arbitrary_header_forwarding() {
    let mut headers = HeaderMap::new();
    for name in SESSION_HEADERS {
        headers.insert(*name, "session-value".parse().unwrap());
    }
    headers.insert(
        "x-upstream-url",
        "https://attacker.invalid".parse().unwrap(),
    );
    headers.insert("authorization", "Bearer caller-secret".parse().unwrap());
    let home_headers = auth_headers(&headers);
    let prepared = upstream::prepare(
        &config(""),
        &selection(),
        "/v1/chat/completions",
        &headers,
        json!({"model":"alias"}),
        false,
    )
    .unwrap();
    let request = prepared.request.build().unwrap();
    for name in SESSION_HEADERS {
        assert_eq!(home_headers[*name], json!(["session-value"]));
        assert_eq!(request.headers()[*name], "session-value");
    }
    assert_eq!(request.headers()["authorization"], "Bearer provider-secret");
    assert!(!request.headers().contains_key("x-upstream-url"));
}

fn full_queue() -> Arc<ReportingQueue> {
    let queue = Arc::new(ReportingQueue::new(QueueLimits {
        max_items: 1,
        ..Default::default()
    }));
    queue.enqueue(json!({"execution_id":"first"})).unwrap();
    queue
}

#[tokio::test(start_paused = true)]
async fn full_queue_retention_expires_at_the_shutdown_budget() {
    let queue = full_queue();
    let started = tokio::time::Instant::now();
    assert!(
        !lifecycle::retain_for_drain(
            queue.clone(),
            json!({"execution_id":"second"}),
            Duration::from_millis(60)
        )
        .await
    );
    assert_eq!(started.elapsed(), Duration::from_millis(60));
    assert_eq!(queue.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn full_queue_retention_can_succeed_when_shutdown_drain_frees_capacity() {
    let queue = full_queue();
    let retained = queue.clone();
    let task = tokio::spawn(async move {
        lifecycle::retain_for_drain(
            retained,
            json!({"execution_id":"second"}),
            Duration::from_secs(1),
        )
        .await
    });
    tokio::task::yield_now().await;
    assert!(queue.acknowledge("first"));
    assert!(task.await.unwrap());
    assert_eq!(queue.pending_batch(1, 4096)[0].id, "second");
}
