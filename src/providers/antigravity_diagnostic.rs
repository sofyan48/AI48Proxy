//! Strictly bounded diagnostics for the HTTP-error branch, before normalization.
//! Never return provider messages, metadata, project IDs, or arbitrary reason text.
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ForbiddenDiagnostic {
    pub code: Option<u16>,
    pub status: Option<&'static str>,
    pub reason: Option<&'static str>,
}

/// A 403 alone does not identify account permissions, URL, or envelope failure.
/// Call only with the upstream HTTP status and already bounded response bytes.
/// Unknown fields/reasons are discarded, not echoed or classified as permissions.
pub fn forbidden_diagnostic(http_status: u16, body: &[u8]) -> Option<ForbiddenDiagnostic> {
    if http_status != 403 || body.len() > 64 * 1024 {
        return None;
    }
    let parsed: Value = serde_json::from_slice(body).ok()?;
    let error = parsed.get("error")?.as_object()?;
    let code = error
        .get("code")
        .and_then(Value::as_u64)
        .filter(|code| (100..=599).contains(code))
        .map(|code| code as u16);
    let status = match error.get("status").and_then(Value::as_str) {
        Some("PERMISSION_DENIED") => Some("PERMISSION_DENIED"),
        Some("UNAUTHENTICATED") => Some("UNAUTHENTICATED"),
        Some("INVALID_ARGUMENT") => Some("INVALID_ARGUMENT"),
        Some("NOT_FOUND") => Some("NOT_FOUND"),
        Some("FAILED_PRECONDITION") => Some("FAILED_PRECONDITION"),
        Some("RESOURCE_EXHAUSTED") => Some("RESOURCE_EXHAUSTED"),
        _ => None,
    };
    let reason = error
        .get("details")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|detail| {
            detail.get("@type").and_then(Value::as_str)
                == Some("type.googleapis.com/google.rpc.ErrorInfo")
        })
        .find_map(
            |detail| match detail.get("reason").and_then(Value::as_str) {
                Some("TOS_VIOLATION") => Some("TOS_VIOLATION"),
                Some("SERVICE_DISABLED") => Some("SERVICE_DISABLED"),
                Some("BILLING_DISABLED") => Some("BILLING_DISABLED"),
                Some("CONSUMER_INVALID") => Some("CONSUMER_INVALID"),
                Some("ACCESS_TOKEN_SCOPE_INSUFFICIENT") => Some("ACCESS_TOKEN_SCOPE_INSUFFICIENT"),
                Some("IAM_PERMISSION_DENIED") => Some("IAM_PERMISSION_DENIED"),
                Some("RATE_LIMIT_EXCEEDED") => Some("RATE_LIMIT_EXCEEDED"),
                Some("QUOTA_EXHAUSTED") => Some("QUOTA_EXHAUSTED"),
                Some("INSUFFICIENT_G1_CREDITS_BALANCE") => Some("INSUFFICIENT_G1_CREDITS_BALANCE"),
                _ => None,
            },
        );
    Some(ForbiddenDiagnostic {
        code,
        status,
        reason,
    })
}
