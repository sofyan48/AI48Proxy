//! Claude upstream error classification, ported from CPA's Claude rate-limit helpers.
//! Not wired automatically. Call only for Claude responses; never log input bodies/headers.
use chrono::{DateTime, Utc};
use http::HeaderMap;
use serde_json::Value;
use std::time::Duration;

const PREFIX: &str = "anthropic-ratelimit-unified-";
const MAX_WINDOW: f64 = (7 * 24 * 3600 + 3600) as f64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitScope {
    /// No rate-limit scope inferred (including non-429 upstream failures).
    None,
    Model,
    Credential,
    /// Fast-mode entitlement refusal, not proof of exhausted shared quota.
    Request,
}

/// Safe to inspect: contains no upstream body, message, identifiers, or headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClaudeLimit {
    pub status: u16,
    pub scope: LimitScope,
    /// Latest applicable future reset, without scheduling jitter.
    pub retry_after: Option<Duration>,
}

/// Classify HTTP failures, or a decoded Claude SSE error with its effective status.
/// `model_level_cooling` mirrors CPA's explicit override; it is not a quota bypass.
/// The caller must preserve the upstream error and obey Home's scheduling policy.
pub fn classify(
    status: u16,
    headers: &HeaderMap,
    body: &[u8],
    now: DateTime<Utc>,
    model_level_cooling: bool,
) -> ClaudeLimit {
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let scope = if status != 429 {
        LimitScope::None
    } else if !model_level_cooling && shared_quota_rejected(headers) {
        LimitScope::Credential
    } else if fast_entitlement(parsed.as_ref(), body) {
        LimitScope::Request
    } else {
        LimitScope::Model
    };
    ClaudeLimit {
        status,
        scope,
        retry_after: (400..600)
            .contains(&status)
            .then(|| retry_after(headers, parsed.as_ref(), now))
            .flatten(),
    }
}

/// Explicit shared rejection outranks a body mentioning fast mode.
pub fn shared_quota_rejected(headers: &HeaderMap) -> bool {
    let status = window(headers, "status");
    let five = window(headers, "5h-status");
    let seven = window(headers, "7d-status");
    five == "rejected" || seven == "rejected" || (status == "rejected" && !overage_only(headers))
}

fn raw<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

fn window(headers: &HeaderMap, suffix: &str) -> String {
    raw(headers, &format!("{PREFIX}{suffix}"))
        .trim()
        .to_ascii_lowercase()
}

fn allowed(status: &str) -> bool {
    matches!(status, "allowed" | "allowed_warning")
}

fn healthy(raw: &str) -> bool {
    raw.trim()
        .parse::<f64>()
        .is_ok_and(|v| v.is_finite() && (0.0..1.0).contains(&v))
}

// Match CPA's narrow overage/Fable exception, not a general quota exemption.
fn overage_only(headers: &HeaderMap) -> bool {
    let status = window(headers, "status");
    let five = window(headers, "5h-status");
    let seven = window(headers, "7d-status");
    if five == "rejected" || seven == "rejected" {
        return false;
    }
    let claim = window(headers, "representative-claim");
    let overage = window(headers, "7d_oi-status") == "rejected"
        || window(headers, "overage-status") == "rejected"
        || !window(headers, "overage-disabled-reason").is_empty()
        || claim.contains("overage");
    if !overage {
        return false;
    }
    let util5 = raw(headers, "anthropic-ratelimit-unified-5h-utilization");
    let util7 = raw(headers, "anthropic-ratelimit-unified-7d-utilization");
    (allowed(&five) && allowed(&seven))
        || (allowed(&seven)
            && five.is_empty()
            && (healthy(util5)
                || (status == "rejected"
                    && !headers.contains_key("anthropic-ratelimit-unified-5h-utilization"))))
        || (allowed(&five) && seven.is_empty() && healthy(util7))
        || (status == "rejected"
            && five.is_empty()
            && seven.is_empty()
            && claim.contains("overage")
            && (util5.trim().is_empty() || healthy(util5))
            && (util7.trim().is_empty() || healthy(util7)))
}

fn fast_entitlement(parsed: Option<&Value>, body: &[u8]) -> bool {
    let message = parsed
        .and_then(|v| v.pointer("/error/message"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned())
        .to_lowercase();
    message.contains("fast request rejected")
        || (message.contains("fast")
            && (message.contains("usage credits") || message.contains("credits are required")))
}

fn per_minute(parsed: Option<&Value>) -> bool {
    let Some(error) = parsed.and_then(|v| v.get("error")) else {
        return false;
    };
    if error.get("type").and_then(Value::as_str) != Some("rate_limit_error") {
        return false;
    }
    matches!(
        error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "this request would exceed the requests per minute rate limit. please try again later."
            | "this request would exceed the input tokens per minute rate limit. please try again later."
            | "this request would exceed the output tokens per minute rate limit. please try again later."
    )
}

fn retry_after(
    headers: &HeaderMap,
    parsed: Option<&Value>,
    now: DateTime<Utc>,
) -> Option<Duration> {
    let status = window(headers, "status");
    let five = window(headers, "5h-status");
    let seven = window(headers, "7d-status");
    let oi = window(headers, "7d_oi-status");
    let overage = overage_only(headers);
    let mut candidates = Vec::new();
    let retry = retry_delay(raw(headers, "retry-after"), now);
    if !overage {
        candidates.extend(retry);
    }
    for (state, reset) in [
        (&five, "5h-reset"),
        (&seven, "7d-reset"),
        (&oi, "7d_oi-reset"),
    ] {
        if state == "rejected" && (reset != "7d_oi-reset" || !overage) {
            candidates.extend(reset_delay(raw(headers, &format!("{PREFIX}{reset}")), now));
        }
    }
    let quota_declared = [
        "status",
        "5h-status",
        "7d-status",
        "7d_oi-status",
        "overage-status",
    ]
    .iter()
    .any(|suffix| {
        headers
            .get_all(format!("{PREFIX}{suffix}"))
            .iter()
            .any(|v| v.to_str().is_ok_and(|s| !s.trim().is_empty()))
    });
    let explicit_minute = per_minute(parsed)
        && !quota_declared
        && headers.get_all("retry-after").iter().count() == 1
        && retry.is_some();
    let unified_rejected = !overage
        && (status == "rejected"
            || five == "rejected"
            || seven == "rejected"
            || oi == "rejected"
            || (status.is_empty() && !allowed(&five) && !allowed(&seven)));
    let unified = raw(headers, "anthropic-ratelimit-unified-reset");
    let billing_boundary = window(headers, "representative-claim").contains("overage")
        && timestamp(unified).is_some_and(|t| {
            Some(t) == timestamp(raw(headers, "anthropic-ratelimit-unified-overage-reset"))
        });
    if unified_rejected && !billing_boundary && !explicit_minute {
        candidates.extend(reset_delay(unified, now));
    }
    candidates.into_iter().max()
}

fn numeric(raw: &str) -> Option<f64> {
    raw.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
}

fn timestamp(raw: &str) -> Option<f64> {
    numeric(raw).or_else(|| {
        let raw = raw.trim();
        let date = DateTime::parse_from_rfc3339(raw)
            .or_else(|_| DateTime::parse_from_rfc2822(raw))
            // HTTP obsolete date formats accepted by Go's http.ParseTime.
            .map(|v| v.with_timezone(&Utc))
            .or_else(|_| {
                chrono::NaiveDateTime::parse_from_str(raw, "%A, %d-%b-%y %H:%M:%S GMT")
                    .map(|v| v.and_utc())
            })
            .or_else(|_| {
                chrono::NaiveDateTime::parse_from_str(raw, "%a %b %e %H:%M:%S %Y")
                    .map(|v| v.and_utc())
            })
            .ok()?;
        Some(date.timestamp() as f64 + f64::from(date.timestamp_subsec_nanos()) / 1e9)
    })
}

fn bounded(seconds: f64) -> Option<Duration> {
    (seconds.is_finite() && seconds > 0.0 && seconds <= MAX_WINDOW)
        .then(|| Duration::from_secs_f64(seconds))
}

fn reset_delay(raw: &str, now: DateTime<Utc>) -> Option<Duration> {
    bounded(
        timestamp(raw)? - now.timestamp() as f64 - f64::from(now.timestamp_subsec_nanos()) / 1e9,
    )
}

fn retry_delay(raw: &str, now: DateTime<Utc>) -> Option<Duration> {
    if let Some(seconds) = numeric(raw) {
        bounded(seconds)
    } else {
        reset_delay(raw, now)
    }
}

#[cfg(test)]
#[path = "claude_limits_tests.rs"]
mod tests;
