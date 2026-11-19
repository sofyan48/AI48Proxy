use super::*;
use http::{HeaderName, HeaderValue};
use serde_json::json;

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).unwrap()
}

fn headers(entries: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (key, value) in entries {
        let name = if *key == "retry-after" {
            key.to_string()
        } else {
            format!("{PREFIX}{key}")
        };
        h.append(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    h
}

fn classify_body(h: &HeaderMap, message: &str) -> ClaudeLimit {
    let body = json!({"error":{"type":"rate_limit_error","message":message}});
    classify(429, h, body.to_string().as_bytes(), now(), false)
}

fn delay(h: &HeaderMap) -> Option<Duration> {
    classify_body(h, "Rate limit exceeded.").retry_after
}

#[test]
fn ordinary_429_is_model_scoped_not_proof_of_account_exhaustion() {
    let limit = classify_body(&HeaderMap::new(), "Rate limit exceeded.");
    assert_eq!(limit.scope, LimitScope::Model);
    assert_eq!(limit.retry_after, None);
    // Malformed bodies still retain genuine HTTP 429 attribution.
    assert_eq!(
        classify(429, &HeaderMap::new(), b"not JSON", now(), false).scope,
        LimitScope::Model
    );
}

#[test]
fn rejected_windows_choose_longest_and_allowed_windows_do_not_contribute() {
    let mut h = headers(&[
        ("5h-status", " ReJeCtEd "),
        ("5h-reset", "1800018000"),
        ("7d-status", "rejected"),
        ("7d-reset", "1800604800"),
        ("retry-after", "60"),
    ]);
    let limit = classify_body(&h, "Rate limit exceeded.");
    assert_eq!(limit.scope, LimitScope::Credential);
    assert_eq!(limit.retry_after, Some(Duration::from_secs(604800)));
    h.insert(
        "anthropic-ratelimit-unified-7d-status",
        HeaderValue::from_static("allowed"),
    );
    assert_eq!(delay(&h), Some(Duration::from_secs(18000)));
    assert_eq!(
        classify(429, &h, b"{}", now(), true).scope,
        LimitScope::Model
    );
}

#[test]
fn fast_entitlement_and_authoritative_quota_precedence() {
    for message in [
        "Usage credits are required for fast mode.",
        "Fast mode requires usage credits",
        "Fast request rejected",
    ] {
        let mut h = headers(&[("retry-after", "120")]);
        assert_eq!(classify_body(&h, message).scope, LimitScope::Request);
        assert_eq!(
            classify_body(&h, message).retry_after,
            Some(Duration::from_secs(120))
        );
        h.insert(
            "anthropic-ratelimit-unified-status",
            HeaderValue::from_static("rejected"),
        );
        assert_eq!(classify_body(&h, message).scope, LimitScope::Credential);
    }
    assert_eq!(
        classify_body(
            &HeaderMap::new(),
            "This organization has exceeded its usage limit."
        )
        .scope,
        LimitScope::Model
    );
}

#[test]
fn retry_after_formats_and_fractional_seconds() {
    for (value, expected) in [
        ("60", 60.0),
        (" 1.5 ", 1.5),
        ("2027-01-15T08:01:30Z", 90.0),
        ("Fri, 15 Jan 2027 08:01:30 GMT", 90.0),
        ("Friday, 15-Jan-27 08:01:30 GMT", 90.0),
        ("Fri Jan 15 08:01:30 2027", 90.0),
    ] {
        assert_eq!(
            delay(&headers(&[("retry-after", value)])),
            Some(Duration::from_secs_f64(expected)),
            "{value}"
        );
    }
}

#[test]
fn absolute_resets_support_unix_fractional_rfc3339_and_http_dates() {
    for value in [
        "1800000090",
        "2027-01-15T08:01:30Z",
        "Fri, 15 Jan 2027 08:01:30 GMT",
    ] {
        assert_eq!(
            delay(&headers(&[("reset", value)])),
            Some(Duration::from_secs(90)),
            "{value}"
        );
    }
    assert_eq!(
        delay(&headers(&[("reset", "1800000001.5")])),
        Some(Duration::from_millis(1500))
    );
}

#[test]
fn invalid_expired_and_billing_length_values_do_not_supply_cooldown() {
    for value in [
        "",
        "0",
        "-1",
        "NaN",
        "inf",
        "1e300",
        "nonsense",
        "1800000000",
        "1799999999",
        "1800608401",
    ] {
        assert_eq!(delay(&headers(&[("reset", value)])), None, "{value}");
    }
    for value in ["0", "-1", "NaN", "inf", "1e300", "608401", "60, 120"] {
        assert_eq!(delay(&headers(&[("retry-after", value)])), None, "{value}");
    }
    // An invalid longer candidate must not suppress a valid shorter one.
    assert_eq!(
        delay(&headers(&[("reset", "1e300"), ("retry-after", "30")])),
        Some(Duration::from_secs(30))
    );
}

#[test]
fn overage_only_rejection_does_not_become_shared_quota_cooldown() {
    let h = headers(&[
        ("status", "rejected"),
        ("5h-status", "allowed"),
        ("7d-status", "allowed_warning"),
        ("7d_oi-status", "rejected"),
        ("7d_oi-reset", "1800604800"),
        ("reset", "1800604800"),
        ("retry-after", "60"),
    ]);
    assert!(!shared_quota_rejected(&h));
    assert_eq!(
        classify_body(&h, "Rate limit exceeded.").scope,
        LimitScope::Model
    );
    assert_eq!(delay(&h), None);
}

#[test]
fn omitted_shared_windows_have_narrow_overage_rules() {
    let h = headers(&[
        ("status", "rejected"),
        ("representative-claim", "overage"),
        ("retry-after", "60"),
    ]);
    assert!(!shared_quota_rejected(&h));
    assert_eq!(delay(&h), None);
    let h = headers(&[
        ("status", "allowed"),
        ("representative-claim", "overage"),
        ("retry-after", "60"),
    ]);
    assert_eq!(delay(&h), Some(Duration::from_secs(60)));
    let h = headers(&[
        ("status", "rejected"),
        ("overage-status", "rejected"),
        ("retry-after", "60"),
    ]);
    assert!(shared_quota_rejected(&h));
    assert_eq!(delay(&h), Some(Duration::from_secs(60)));
    for util in ["1", "NaN", "inf", "-0.1", "bad"] {
        let h = headers(&[
            ("status", "rejected"),
            ("7d-status", "allowed"),
            ("7d_oi-status", "rejected"),
            ("5h-utilization", util),
        ]);
        assert!(shared_quota_rejected(&h), "{util}");
    }
}

#[test]
fn matching_overage_billing_boundary_is_excluded_but_shared_reset_survives() {
    let h = headers(&[
        ("5h-status", "rejected"),
        ("5h-reset", "1800000120"),
        ("representative-claim", "overage"),
        ("reset", "1800604800"),
        ("overage-reset", "2027-01-22T08:00:00Z"),
    ]);
    assert_eq!(delay(&h), Some(Duration::from_secs(120)));
    assert!(shared_quota_rejected(&h));
}

#[test]
fn exact_per_minute_message_ignores_only_unclassified_unified_reset() {
    for kind in ["requests", "input tokens", "output tokens"] {
        let message = format!(
            "This request would exceed the {kind} per minute rate limit. Please try again later."
        );
        let h = headers(&[("reset", "1800018000"), ("retry-after", "60")]);
        assert_eq!(
            classify_body(&h, &message).retry_after,
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            classify_body(&h, &format!("{message} Quota also exhausted.")).retry_after,
            Some(Duration::from_secs(18000))
        );
        let mut declared = h.clone();
        declared.append(
            "anthropic-ratelimit-unified-status",
            HeaderValue::from_static(""),
        );
        declared.append(
            "anthropic-ratelimit-unified-status",
            HeaderValue::from_static("rejected"),
        );
        assert_eq!(
            classify_body(&declared, &message).retry_after,
            Some(Duration::from_secs(18000))
        );
        let mut repeated = h.clone();
        repeated.append("retry-after", HeaderValue::from_static("60"));
        assert_eq!(
            classify_body(&repeated, &message).retry_after,
            Some(Duration::from_secs(18000))
        );
    }
}

#[test]
fn hints_on_other_http_errors_do_not_imply_rate_limit_scope() {
    let h = headers(&[("retry-after", "90"), ("5h-status", "rejected")]);
    for status in [400, 401, 403, 500, 503, 599] {
        let limit = classify(status, &h, b"{}", now(), false);
        assert_eq!(limit.scope, LimitScope::None);
        assert_eq!(limit.retry_after, Some(Duration::from_secs(90)));
    }
    for status in [200, 302, 600] {
        assert_eq!(classify(status, &h, b"{}", now(), false).retry_after, None);
    }
}
