use super::*;
use serde_json::json;

fn event(id: &str) -> Value {
    json!({"execution_id": id, "model": "model", "tokens": {"total_tokens": 10}})
}

#[test]
fn ambiguous_delivery_retries_identical_bytes_and_acknowledges_only_exact_id() {
    let queue = ReportingQueue::default();
    queue.enqueue(event("one")).unwrap();
    queue.enqueue(event("two")).unwrap();
    let first = queue.pending_batch(10, usize::MAX);
    let retry = queue.pending_batch(10, usize::MAX);
    assert_eq!(first[0].payload, retry[0].payload);
    assert_eq!(first[0].command()[0], b"LPUSH");
    assert_eq!(first[0].command()[1], b"usage");
    assert!(!queue.acknowledge("unknown"));
    assert!(queue.acknowledge("two"));
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.pending_batch(10, usize::MAX)[0].id, "one");
    queue.enqueue(event("two")).unwrap();
    assert_eq!(queue.len(), 1);
    assert!(queue.acknowledge("one"));
    assert!(!queue.acknowledge("one"));
    assert_eq!(queue.pending_bytes(), 0);
}

#[test]
fn stable_id_deduplication_rejects_conflicting_payloads() {
    let queue = ReportingQueue::default();
    let id = queue.enqueue(json!({"model": "model"})).unwrap();
    assert!(Uuid::parse_str(&id).is_ok());
    let delivery = queue.pending_batch(1, usize::MAX).remove(0);
    let payload: Value = serde_json::from_slice(&delivery.payload).unwrap();
    assert_eq!(payload["execution_id"], id);
    assert_eq!(queue.enqueue(payload).unwrap(), id);
    assert_eq!(queue.len(), 1);
    assert_eq!(
        queue.enqueue(json!({"execution_id": id, "model": "different"})),
        Err(EnqueueError::ConflictingId)
    );
}

#[test]
fn capacity_is_bounded_without_eviction_and_recent_dedupe_is_bounded() {
    let queue = ReportingQueue::new(QueueLimits {
        max_items: 1,
        max_bytes: 1024,
        max_payload_bytes: 512,
        max_recent_ids: 1,
    });
    queue.enqueue(event("one")).unwrap();
    assert_eq!(queue.enqueue(event("two")), Err(EnqueueError::Full));
    assert_eq!(queue.len(), 1);
    let bytes = queue.pending_bytes();
    assert!(queue.pending_batch(1, bytes - 1).is_empty());
    assert_eq!(queue.pending_batch(1, bytes).len(), 1);
    queue.acknowledge("one");
    queue.enqueue(event("two")).unwrap();
    queue.acknowledge("two");
    queue.enqueue(event("one")).unwrap();
    assert_eq!(queue.len(), 1); // Older ID aged out locally; Home still dedupes its execution_id.
    assert_eq!(queue.state().recent.len(), 1);
    assert_eq!(
        queue.enqueue(json!({"execution_id": "large", "data": "x".repeat(1024)})),
        Err(EnqueueError::TooLarge)
    );
    assert_eq!(queue.enqueue(json!([])), Err(EnqueueError::NotObject));
    assert_eq!(
        queue.enqueue(json!({"execution_id": " "})),
        Err(EnqueueError::InvalidId)
    );
}

#[test]
fn concurrent_enqueue_deduplicates_without_races() {
    let queue = Arc::new(ReportingQueue::default());
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                for _ in 0..100 {
                    queue.enqueue(event("same")).unwrap();
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(queue.len(), 1);
}

#[test]
fn usage_v2_buckets_do_not_overlap_and_payload_has_no_raw_secrets() {
    let breakdown = TokenBreakdown::subset(100, 20, 10, 40, 15, 140);
    assert!(breakdown.valid());
    assert_eq!(breakdown.input.uncached_tokens, 70);
    assert_eq!(breakdown.output.non_reasoning_tokens, 25);
    let record = UsageRecord {
        execution_id: "execution".into(),
        request_id: "request".into(),
        provider: "openai".into(),
        model: "model".into(),
        auth_index: "credential-index".into(),
        breakdown,
        timestamp: chrono::DateTime::parse_from_rfc3339("2026-07-21T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
        ..UsageRecord::default()
    };
    let payload = usage_payload(&record).unwrap();
    assert_eq!(payload, usage_payload(&record).unwrap());
    assert_eq!(payload["accounting_version"], 2);
    assert_eq!(payload["token_breakdown"]["schema_version"], 2);
    assert_eq!(payload["tokens"]["cache_creation_tokens"], 10);
    assert_eq!(payload["fail"]["status_code"], 200);
    assert!(payload.get("api_key").is_none());
    assert!(payload.get("response_headers").is_none());
    let queue = ReportingQueue::default();
    queue.enqueue(payload.clone()).unwrap();
    queue.enqueue(payload).unwrap();
    assert_eq!(queue.len(), 1);
}

#[test]
fn media_billing_fields_preserve_go_types_presence_and_validate_duration() {
    use super::usage::MediaCounts;
    let mut record = UsageRecord {
        execution_id: "media".into(),
        ..Default::default()
    };
    assert!(
        usage_payload(&record)
            .unwrap()
            .get("media_counts")
            .is_none()
    );
    record.media_counts = Some(MediaCounts {
        prompt_media: Some(0),
        completion_media: Some(1),
        prompt_audio: Some(2),
        audio_seconds: Some(3.25),
    });
    assert_eq!(
        usage_payload(&record).unwrap()["media_counts"],
        json!({"prompt_media":0,"completion_media":1,"prompt_audio":2,"audio_seconds":3.25})
    );
    record.media_counts = Some(MediaCounts {
        audio_seconds: Some(0.0),
        ..Default::default()
    });
    assert_eq!(
        usage_payload(&record).unwrap()["media_counts"],
        json!({"audio_seconds":0.0})
    );
    for seconds in [-1.0, f64::NAN, f64::INFINITY] {
        record.media_counts.as_mut().unwrap().audio_seconds = Some(seconds);
        assert_eq!(usage_payload(&record), Err("invalid media counts"));
    }
    record.media_counts = Some(MediaCounts {
        prompt_media: Some(-1),
        ..Default::default()
    });
    assert_eq!(usage_payload(&record), Err("invalid media counts"));
}

#[test]
fn local_failures_are_request_scoped_but_real_upstream_failures_are_not() {
    let mut record = UsageRecord {
        execution_id: "attribution".into(),
        executor_type: "ai48proxy".into(),
        ..Default::default()
    };
    for status in [400, 499, 501, 502, 503] {
        record.local_failure(status);
        let payload = usage_payload(&record).unwrap();
        assert_eq!(payload["fail"]["status_code"], status);
        assert_eq!(payload["fail"]["code"], "request_scoped");
        assert_eq!(payload["failed"], true);
    }
    for status in [401, 403, 429, 500, 502, 503] {
        record.upstream_status(status);
        let payload = usage_payload(&record).unwrap();
        assert_eq!(payload["fail"]["status_code"], status);
        assert_eq!(payload["fail"]["code"], "");
        assert_eq!(payload["failed"], true);
    }
    record.upstream_status(200);
    assert!(!record.failed);
    assert_eq!(usage_payload(&record).unwrap()["fail"]["status_code"], 200);
}

#[test]
fn codex_quota_feedback_preserves_only_home_retry_hints() {
    let mut record = UsageRecord {
        execution_id: "quota".into(),
        provider: "codex".into(),
        ..Default::default()
    };
    record.upstream_failure(
        429,
        &json!({"error":{
        "type":"usage_limit_reached", "resets_at":1900000000,
        "resets_in_seconds":120, "message":"secret", "access_token":"secret"
    }, "api_key":"secret"}),
    );
    let payload = usage_payload(&record).unwrap();
    let body: Value = serde_json::from_str(payload["fail"]["body"].as_str().unwrap()).unwrap();
    assert_eq!(
        body,
        json!({"error":{"type":"usage_limit_reached", "resets_at":1900000000, "resets_in_seconds":120}})
    );
    assert_eq!(payload["fail"]["status_code"], 429);
    assert_eq!(payload["fail"]["code"], "");
    assert!(!payload.to_string().contains("secret"));
    // Refresh failure must retain 401 attribution; it must not inherit old quota hints.
    record.upstream_status(401);
    assert_eq!(
        usage_payload(&record).unwrap()["fail"],
        json!({"status_code":401,"body":"","code":""})
    );
    record.local_failure(502);
    assert_eq!(record.fail_code, "request_scoped");
    assert!(record.fail_body.is_empty());
}

#[test]
fn retry_hints_are_scoped_to_provider_status_type_and_valid_values() {
    let mut record = UsageRecord {
        execution_id: "scoped".into(),
        provider: "codex".into(),
        ..Default::default()
    };
    let body = json!({"error":{"type":"usage_limit_reached", "resets_in_seconds":120}});
    record.upstream_failure(401, &body);
    assert!(record.fail_body.is_empty());
    record.provider = "openai".into();
    record.upstream_failure(429, &body);
    assert!(record.fail_body.is_empty());
    record.provider = "codex".into();
    record.upstream_failure(
        429,
        &json!({"error":{"type":"rate_limit_error","resets_in_seconds":120}}),
    );
    assert!(record.fail_body.is_empty());
    record.upstream_failure(429, &json!({"error":{"type":"usage_limit_reached","resets_in_seconds":-1,"resets_at":u64::MAX}}));
    assert_eq!(
        serde_json::from_str::<Value>(&record.fail_body).unwrap(),
        json!({"error":{"type":"usage_limit_reached"}})
    );
}

#[test]
fn antigravity_retry_feedback_is_typed_and_redacted() {
    let mut record = UsageRecord {
        execution_id: "google-quota".into(),
        provider: "antigravity".into(),
        ..Default::default()
    };
    record.upstream_failure(429, &json!({"error":{"message":"secret", "details":[
        {"@type":"type.googleapis.com/google.rpc.RetryInfo", "retryDelay":"2.5s", "token":"secret"},
        {"@type":"type.googleapis.com/google.rpc.ErrorInfo", "metadata":{"quotaResetTimeStamp":"2030-01-01T00:00:00Z","token":"secret"}},
        {"@type":"unknown", "retryDelay":"60s"},
        {"@type":"type.googleapis.com/google.rpc.RetryInfo", "retryDelay":"secret"}
    ]}}));
    let payload = usage_payload(&record).unwrap();
    let body: Value = serde_json::from_str(&record.fail_body).unwrap();
    let details = body["error"]["details"].as_array().unwrap();
    assert_eq!(details.len(), 2);
    assert_eq!(details[0]["retryDelay"], "2.5s");
    assert_eq!(
        details[1]["metadata"]["quotaResetTimeStamp"],
        "2030-01-01T00:00:00+00:00"
    );
    assert!(!payload.to_string().contains("secret"));
}

fn failure_body(provider: &str, status: u16, body: Value) -> String {
    let mut record = UsageRecord {
        execution_id: "canonical".into(),
        provider: provider.into(),
        ..Default::default()
    };
    record.upstream_failure(status, &body);
    // Re-deriving from the emitted canonical body must be stable (stream event round trip).
    let reparsed: Value = serde_json::from_str(&record.fail_body).unwrap_or_default();
    let mut again = record.clone();
    again.upstream_failure(status, &reparsed);
    assert_eq!(again.fail_body, record.fail_body);
    assert!(!record.fail_body.contains("secret"));
    record.fail_body
}

#[test]
fn model_support_failures_emit_matched_home_pattern_only() {
    for status in [400, 422] {
        let body = failure_body(
            "openai",
            status,
            json!({"error":{"message":"The Requested Model Is Not Supported for key secret","code":"secret"}}),
        );
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({"error":{"message":"requested model is not supported"}})
        );
    }
    let body = failure_body(
        "claude",
        400,
        json!({"error":{"detail":["x", {"m":"Not available for your plan secret"}]}}),
    );
    assert!(body.contains("not available for your plan"));
    // Other statuses or unrelated 400 text keep the body empty.
    assert!(
        failure_body(
            "openai",
            500,
            json!({"error":{"message":"model not supported"}})
        )
        .is_empty()
    );
    assert!(
        failure_body(
            "openai",
            400,
            json!({"error":{"message":"secret prompt too long"}})
        )
        .is_empty()
    );
    assert!(failure_body("openai", 400, Value::Null).is_empty());
}

#[test]
fn store_false_not_found_emits_fixed_sentence() {
    let body = failure_body(
        "codex",
        404,
        json!({"error":{"message":"Item with id 'rs_secret' not found. Items are not persisted when `store` is set to false. Try again with `store` set to true, or remove this item from your input."}}),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({"error":{"message":"Item with id not found. Items are not persisted when `store` is set to false."}})
    );
    let message = body.to_lowercase();
    assert!(message.contains("item with id") && message.contains("not found"));
    assert!(message.contains("items are not persisted when `store` is set to false"));
    assert!(
        failure_body(
            "codex",
            404,
            json!({"error":{"message":"model secret not found"}})
        )
        .is_empty()
    );
    assert!(failure_body("codex", 400, json!({"error":{"message":"Item with id x not found. Items are not persisted when `store` is set to false."}})).is_empty());
}

#[test]
fn antigravity_tos_violation_emits_minimal_error_info() {
    let upstream = json!({"error":{"code":403,"status":"PERMISSION_DENIED","message":"secret","details":[
        {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":" tos_violation ","domain":"secret","metadata":{"email":"secret"}}
    ]}});
    let body = failure_body("antigravity", 403, upstream.clone());
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({"error":{"code":403,"status":"PERMISSION_DENIED","details":[
            {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"TOS_VIOLATION"}
        ]}})
    );
    assert!(failure_body("claude", 403, upstream).is_empty());
    for other in [
        json!({"error":{"code":403,"status":"PERMISSION_DENIED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SECRET"}]}}),
        json!({"error":{"code":403,"status":"UNAUTHENTICATED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"TOS_VIOLATION"}]}}),
        json!({"error":{"code":"403","status":"PERMISSION_DENIED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"TOS_VIOLATION"}]}}),
        json!({"error":{"code":403,"status":"PERMISSION_DENIED","details":[{"@type":"other","reason":"TOS_VIOLATION"}]}}),
    ] {
        assert!(failure_body("antigravity", 403, other).is_empty());
    }
}

#[test]
fn claude_and_openai_rate_limits_keep_empty_body() {
    // Home has no Claude/OpenAI 429 body parser; quota backoff uses status alone.
    let rate_limit = json!({"type":"error","error":{"type":"rate_limit_error","message":"secret"}});
    assert!(failure_body("claude", 429, rate_limit.clone()).is_empty());
    assert!(failure_body("openai", 429, rate_limit).is_empty());
}

#[tokio::test]
async fn dropped_output_owner_releases_once_and_reporting_does_not_release_again() {
    use crate::execution::Registry;
    use tokio_util::sync::CancellationToken;

    let registry = Registry::new(CancellationToken::new());
    let guard = registry.begin(
        "credential".into(),
        "model".into(),
        true,
        "request".into(),
        "stream".into(),
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    drop(rx);
    // A dispatch result that cannot reach its output owner must still drop its guard.
    let mut returned_guard = tx.send(guard).unwrap_err();
    assert_eq!(returned_guard.finish().unwrap().release_seq, 1);
    assert!(returned_guard.finish().is_none());
    drop(returned_guard);
    assert_eq!(registry.active_count(), 0);
    let releases = registry.pending_releases();
    assert_eq!(releases.len(), 1);
    assert_eq!(releases[0].release_seq, 1);

    let mut record = UsageRecord {
        execution_id: "dropped-output".into(),
        ..Default::default()
    };
    record.local_failure(499);
    let queue = ReportingQueue::default();
    let payload = usage_payload(&record).unwrap();
    queue.enqueue(payload.clone()).unwrap();
    queue.enqueue(payload).unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(registry.pending_releases(), releases);
    registry.acknowledge(&releases[0]);
    assert!(registry.is_drained());
}

#[test]
fn raw_scoped_matching_preserves_order_status_regex_and_json_escapes() {
    use crate::config::execution::{ErrorAction, RequestScopedRule};
    let mut record = UsageRecord {
        execution_id: "raw-rules".into(),
        ..Default::default()
    };
    let raw = r#"{ "message" : "Sensitive\u0020Text" }"#;
    let rules = vec![
        RequestScopedRule {
            status: 403,
            substrings: vec![raw.into()],
            action: "stop".into(),
            ..Default::default()
        },
        RequestScopedRule {
            status: 401,
            substrings: vec!["sensitive".into()],
            action: "stop".into(),
            ..Default::default()
        },
        RequestScopedRule {
            status: 401,
            regexes: vec![
                "[".into(),
                r#"^\{ \"message\" : \"Sensitive\\u0020Text\" \}$"#.into(),
            ],
            action: "unknown".into(),
            ..Default::default()
        },
        RequestScopedRule {
            status: 401,
            substrings: vec![raw.into()],
            action: " CONTINUE-AND-COOLDOWN ".into(),
            ..Default::default()
        },
        RequestScopedRule {
            status: 401,
            substrings: vec![raw.into()],
            action: "stop".into(),
            ..Default::default()
        },
    ];
    assert_eq!(
        record.upstream_failure_with_rules(401, raw, &rules),
        Some(ErrorAction::ContinueAndCooldown)
    );
    assert_eq!(record.fail_code, "force_cooldown");
    let mut regex_rules = rules[2..3].to_vec();
    regex_rules[0].action = "stop".into();
    assert_eq!(
        record.upstream_failure_with_rules(401, raw, &regex_rules),
        Some(ErrorAction::Stop)
    );
    let serialized = serde_json::from_str::<Value>(raw).unwrap().to_string();
    assert_eq!(
        record.upstream_failure_with_rules(401, &serialized, &rules),
        None
    );
    assert!(record.fail_code.is_empty());
    assert!(record.fail_body.is_empty());
    let invalid_utf8 = b"before\xffafter";
    let replacement = [RequestScopedRule {
        status: 401,
        substrings: vec!["\u{fffd}".into()],
        action: "stop".into(),
        ..Default::default()
    }];
    assert_eq!(
        record.upstream_failure_with_rule_bytes(401, invalid_utf8, &replacement),
        None
    );
    let replacement_regex = [RequestScopedRule {
        status: 401,
        regexes: vec!["\u{fffd}".into()],
        action: "stop".into(),
        ..Default::default()
    }];
    assert_eq!(
        record.upstream_failure_with_rule_bytes(401, invalid_utf8, &replacement_regex),
        Some(ErrorAction::Stop)
    );
    assert!(
        !usage_payload(&record)
            .unwrap()
            .to_string()
            .contains("Sensitive")
    );
}

#[test]
fn inconsistent_usage_preserves_total_as_unclassified() {
    for breakdown in [
        TokenBreakdown::subset(10, 20, 0, 5, 0, 15),
        TokenBreakdown::subset(10, 0, 0, 5, 6, 15),
        TokenBreakdown::subset(i64::MAX, 0, 0, 5, 0, 42),
        TokenBreakdown::subset(-1, 0, 0, 5, 0, 10),
    ] {
        assert!(breakdown.valid());
        assert_eq!(breakdown.quality, "inconsistent");
        assert_eq!(breakdown.unclassified_tokens, breakdown.total_tokens);
        assert_eq!(breakdown.input.total_tokens, 0);
    }
    let record = UsageRecord {
        execution_id: "id".into(),
        breakdown: TokenBreakdown {
            schema_version: 1,
            ..TokenBreakdown::default()
        },
        ..UsageRecord::default()
    };
    assert!(usage_payload(&record).is_err());
}
