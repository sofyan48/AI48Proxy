//! Sanitized upstream rejection metadata. Never forward arbitrary provider text/headers.
use crate::providers::claude_limits;
use axum::{
    http::{HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use chrono::Utc;
use serde_json::Value;

/// Retain original bytes only for local matching. Oversized/incomplete bodies fail
/// closed rather than matching a misleading prefix. Never log or serialize this value.
pub(super) async fn raw_error_body(
    response: &mut reqwest::Response,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return None,
            chunk = response.chunk() => chunk,
        };
        match chunk {
            Ok(Some(chunk)) if chunk.len() <= (64 * 1024usize).saturating_sub(bytes.len()) => {
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => return Some(bytes),
            _ => return None,
        }
    }
}

pub(super) fn claude_limit(
    status: u16,
    headers: &HeaderMap,
    body: &Value,
) -> claude_limits::ClaudeLimit {
    claude_limits::classify(
        status,
        headers,
        body.to_string().as_bytes(),
        Utc::now(),
        false,
    )
}

pub(super) fn response(status: StatusCode, provider: &str, headers: &HeaderMap) -> Response {
    let (code, message) = match status.as_u16() {
        429 => (
            "rate_limit_error",
            "Upstream rate limit reached; retry after the indicated reset",
        ),
        529 if provider == "claude" => ("overloaded_error", "Upstream is overloaded"),
        _ => ("upstream_error", "Upstream rejected the request"),
    };
    let mut response = super::error(status, code, message);
    // Validate numeric and HTTP-date forms without reflecting arbitrary header content.
    if let Some(value) = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
    {
        let valid = value
            .parse::<f64>()
            .is_ok_and(|number| number.is_finite() && number > 0.0 && number <= 608400.0)
            || chrono::DateTime::parse_from_rfc2822(value).is_ok_and(|date| {
                let seconds = date.signed_duration_since(Utc::now()).num_seconds();
                (1..=608400).contains(&seconds)
            });
        if valid && let Ok(value) = HeaderValue::from_str(value) {
            response.headers_mut().insert("retry-after", value);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rate_limit_is_distinct_and_only_valid_retry_metadata_survives() {
        for (value, expected) in [
            ("12", true),
            ("0.5", true),
            ("-1", false),
            ("NaN", false),
            ("https://private.invalid/token", false),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("retry-after", value.parse().unwrap());
            headers.insert("set-cookie", "secret=private".parse().unwrap());
            headers.insert("authorization", "Bearer private".parse().unwrap());
            let response = response(StatusCode::TOO_MANY_REQUESTS, "claude", &headers);
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(response.headers().contains_key("retry-after"), expected);
            assert!(!response.headers().contains_key("set-cookie"));
            assert!(!response.headers().contains_key("authorization"));
        }
    }
}
