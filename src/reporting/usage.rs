//! Minimal Home/Go usage v2 payloads. Provider-specific media/tool extensions are not inferred.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InputTokens {
    pub total_tokens: i64,
    pub uncached_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OutputTokens {
    pub total_tokens: i64,
    pub non_reasoning_tokens: i64,
    pub reasoning_tokens: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenBreakdown {
    pub schema_version: u8,
    pub quality: String,
    pub total_tokens: i64,
    pub input: InputTokens,
    pub output: OutputTokens,
    pub unclassified_tokens: i64,
}

impl Default for TokenBreakdown {
    fn default() -> Self {
        Self::unclassified(0)
    }
}

impl TokenBreakdown {
    pub fn unclassified(total: i64) -> Self {
        Self {
            schema_version: 2,
            quality: "unclassified".into(),
            total_tokens: total.max(0),
            input: InputTokens::default(),
            output: OutputTokens::default(),
            unclassified_tokens: total.max(0),
        }
    }

    /// OpenAI-style subset semantics: caches are included in input, reasoning in output.
    /// Inconsistent data preserves the authoritative total, without double counting buckets.
    pub fn subset(
        input: i64,
        cache_read: i64,
        cache_write: i64,
        output: i64,
        reasoning: i64,
        total: i64,
    ) -> Self {
        let expected = input
            .checked_add(output)
            .filter(|_| input >= 0 && output >= 0);
        let cache = cache_read
            .checked_add(cache_write)
            .filter(|_| cache_read >= 0 && cache_write >= 0);
        let resolved = if total == 0 {
            expected.unwrap_or(0)
        } else {
            total
        };
        if expected.is_none()
            || cache.is_none()
            || cache.unwrap_or(i64::MAX) > input
            || reasoning < 0
            || reasoning > output
            || resolved < 0
            || Some(resolved) != expected
        {
            let mut result = Self::unclassified(resolved.max(expected.unwrap_or(0)));
            // Match Go: use a supplied positive authoritative total even when inconsistent.
            if total > 0 {
                result = Self::unclassified(total);
            }
            result.quality = "inconsistent".into();
            return result;
        }
        Self {
            schema_version: 2,
            quality: "complete".into(),
            total_tokens: resolved,
            input: InputTokens {
                total_tokens: input,
                uncached_tokens: input - cache.unwrap_or(0),
                cache_read_tokens: cache_read,
                cache_write_tokens: cache_write,
            },
            output: OutputTokens {
                total_tokens: output,
                non_reasoning_tokens: output - reasoning,
                reasoning_tokens: reasoning,
            },
            unclassified_tokens: 0,
        }
    }

    pub fn valid(&self) -> bool {
        let values = [
            self.total_tokens,
            self.input.total_tokens,
            self.input.uncached_tokens,
            self.input.cache_read_tokens,
            self.input.cache_write_tokens,
            self.output.total_tokens,
            self.output.non_reasoning_tokens,
            self.output.reasoning_tokens,
            self.unclassified_tokens,
        ];
        self.schema_version == 2
            && values.iter().all(|value| *value >= 0)
            && matches!(
                self.quality.as_str(),
                "complete" | "unclassified" | "inconsistent"
            )
            && self
                .input
                .uncached_tokens
                .checked_add(self.input.cache_read_tokens)
                .and_then(|sum| sum.checked_add(self.input.cache_write_tokens))
                == Some(self.input.total_tokens)
            && self
                .output
                .non_reasoning_tokens
                .checked_add(self.output.reasoning_tokens)
                == Some(self.output.total_tokens)
            && self
                .input
                .total_tokens
                .checked_add(self.output.total_tokens)
                .and_then(|sum| sum.checked_add(self.unclassified_tokens))
                == Some(self.total_tokens)
            && (self.quality != "complete" || self.unclassified_tokens == 0)
    }
}

/// IDs are metadata, not raw credentials/API keys. Allocate execution_id once per execution,
/// not once per inbound HTTP request (which can have multiple execution attempts).
#[derive(Clone, Debug, Default)]
pub struct UsageRecord {
    pub execution_id: String,
    pub request_id: String,
    pub provider: String,
    pub executor_type: String,
    pub model: String,
    pub auth_index: String,
    pub timestamp: DateTime<Utc>,
    pub latency_ms: i64,
    pub ttft_ms: i64,
    pub stream: bool,
    pub failed: bool,
    pub status_code: u16,
    /// Home recognizes request_scoped and stream_truncated as non-cooling failures.
    pub fail_code: String,
    /// Allowlisted scheduler hints only, never the raw upstream response body.
    pub fail_body: String,
    pub breakdown: TokenBreakdown,
}

impl UsageRecord {
    /// Call as soon as upstream headers arrive, before refresh, retry, or body handling.
    /// Local failures must use local_failure rather than inheriting upstream attribution.
    pub fn upstream_status(&mut self, status: u16) {
        self.status_code = status;
        self.failed = !(200..300).contains(&status);
        self.fail_code.clear();
        self.fail_body.clear();
    }

    pub fn local_failure(&mut self, status: u16) {
        self.status_code = status;
        self.failed = true;
        self.fail_code = "request_scoped".into();
        self.fail_body.clear();
    }

    /// Home derives scheduler decisions from fail.body text. Go CPA forwards the raw
    /// upstream body; here only fixed canonical bodies are emitted, and only when the
    /// matching Home classifier would fire on the original body. Upstream text is never
    /// copied, so the output is idempotent when re-derived from its own canonical body.
    pub fn upstream_failure(&mut self, status: u16, body: &Value) {
        self.upstream_status(status);
        let canonical = match status {
            400 | 422 => model_support_body(body),
            404 => store_not_persisted_body(body),
            403 if self.provider.eq_ignore_ascii_case("antigravity") => antigravity_tos_body(body),
            429 => self.quota_hint_body(body),
            _ => None,
        };
        if let Some(canonical) = canonical {
            self.fail_body = canonical;
        }
    }

    /// Match against the original bounded body before sanitization. The caller must
    /// honor stop/continue in its retry loop; no upstream text is persisted here.
    pub fn upstream_failure_with_rules(
        &mut self,
        status: u16,
        raw_body: &str,
        rules: &[crate::config::execution::RequestScopedRule],
    ) -> Option<crate::config::execution::ErrorAction> {
        let body =
            serde_json::from_str(raw_body).unwrap_or_else(|_| Value::String(raw_body.to_owned()));
        self.upstream_failure(status, &body);
        let action = rules.iter().find_map(|rule| rule.matches(status, raw_body));
        if let Some(action) = action {
            self.fail_code = action.fail_code().into();
        }
        action
    }

    /// Home currently parses 429 retry hints from provider-specific fail.body JSON,
    /// not Retry-After headers or generic retry_after fields. Copy only those hints.
    /// Claude/OpenAI 429 bodies stay empty: Home has no provider hint parser for them.
    fn quota_hint_body(&self, body: &Value) -> Option<String> {
        let mut error = serde_json::Map::new();
        match self.provider.to_ascii_lowercase().as_str() {
            "codex"
                if body.pointer("/error/type").and_then(Value::as_str)
                    == Some("usage_limit_reached") =>
            {
                error.insert("type".into(), json!("usage_limit_reached"));
                for key in ["resets_at", "resets_in_seconds"] {
                    if let Some(seconds) = body
                        .get("error")
                        .and_then(|e| e.get(key))
                        .and_then(Value::as_u64)
                        .filter(|s| *s <= i64::MAX as u64 / 1_000_000_000)
                    {
                        error.insert(key.into(), json!(seconds));
                    }
                }
            }
            "antigravity" => {
                let mut details = Vec::new();
                for detail in body
                    .pointer("/error/details")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    match detail.get("@type").and_then(Value::as_str) {
                        Some("type.googleapis.com/google.rpc.RetryInfo") => {
                            if let Some(delay) = detail
                                .get("retryDelay")
                                .and_then(Value::as_str)
                                .filter(|s| valid_retry_delay(s))
                            {
                                details.push(json!({"@type":"type.googleapis.com/google.rpc.RetryInfo", "retryDelay":delay}));
                            }
                        }
                        Some("type.googleapis.com/google.rpc.ErrorInfo") => {
                            if let Some(reset) = detail
                                .pointer("/metadata/quotaResetTimeStamp")
                                .and_then(Value::as_str)
                                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                            {
                                details.push(json!({"@type":"type.googleapis.com/google.rpc.ErrorInfo", "metadata":{"quotaResetTimeStamp":reset.to_rfc3339()}}));
                            }
                        }
                        _ => {}
                    }
                }
                if !details.is_empty() {
                    error.insert("details".into(), json!(details));
                }
            }
            _ => {}
        }
        (!error.is_empty()).then(|| json!({"error": error}).to_string())
    }
}

/// Copied verbatim from Home `isModelSupportErrorMessage` (internal/cliproxy/auth/result.go).
const MODEL_SUPPORT_PATTERNS: [&str; 10] = [
    "model_not_supported",
    "requested model is not supported",
    "requested model is unsupported",
    "requested model is unavailable",
    "model is not supported",
    "model not supported",
    "unsupported model",
    "model unavailable",
    "not available for your plan",
    "not available for your account",
];

/// Satisfies Home `isRequestScopedNotFoundMessage` without the upstream item ID.
const STORE_NOT_PERSISTED_MESSAGE: &str =
    "Item with id not found. Items are not persisted when `store` is set to false.";

const GOOGLE_ERROR_INFO: &str = "type.googleapis.com/google.rpc.ErrorInfo";

/// Lowercased string values of the bounded upstream body, newline-joined. Go matches the
/// raw body text; non-string JSON leaves and object keys carry no Home-relevant phrases.
fn body_text(body: &Value) -> String {
    fn walk(value: &Value, depth: usize, out: &mut String) {
        match value {
            Value::String(text) => {
                out.push_str(&text.to_lowercase());
                out.push('\n');
            }
            Value::Array(items) if depth < 32 => {
                items.iter().for_each(|item| walk(item, depth + 1, out));
            }
            Value::Object(map) if depth < 32 => {
                map.values().for_each(|item| walk(item, depth + 1, out));
            }
            _ => {}
        }
    }
    let mut out = String::new();
    walk(body, 0, &mut out);
    out
}

/// Emits the matched Home pattern constant itself, so Home suspends the model as unsupported.
fn model_support_body(body: &Value) -> Option<String> {
    let text = body_text(body);
    MODEL_SUPPORT_PATTERNS
        .iter()
        .find(|pattern| text.contains(*pattern))
        .map(|pattern| json!({"error": {"message": pattern}}).to_string())
}

/// Home treats this 404 as request-scoped instead of a 12h not_found model suspension.
fn store_not_persisted_body(body: &Value) -> Option<String> {
    let text = body_text(body);
    (text.contains("item with id")
        && text.contains("not found")
        && text.contains("items are not persisted when `store` is set to false"))
    .then(|| json!({"error": {"message": STORE_NOT_PERSISTED_MESSAGE}}).to_string())
}

/// Mirrors Home `recordAntigravityTOSPause`: error.code 403, error.status PERMISSION_DENIED,
/// and an ErrorInfo detail with reason TOS_VIOLATION. Emits only those fields.
fn antigravity_tos_body(body: &Value) -> Option<String> {
    let error = body.get("error")?;
    let code_ok = error.get("code").and_then(Value::as_i64) == Some(403);
    let status_ok = error
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("PERMISSION_DENIED"));
    let violation = error
        .get("details")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|detail| {
            detail.get("@type").and_then(Value::as_str) == Some(GOOGLE_ERROR_INFO)
                && detail
                    .get("reason")
                    .and_then(Value::as_str)
                    .is_some_and(|r| r.trim().eq_ignore_ascii_case("TOS_VIOLATION"))
        });
    (code_ok && status_ok && violation).then(|| {
        json!({"error": {"code": 403, "status": "PERMISSION_DENIED", "details": [
            {"@type": GOOGLE_ERROR_INFO, "reason": "TOS_VIOLATION"}
        ]}})
        .to_string()
    })
}

fn valid_retry_delay(delay: &str) -> bool {
    let Some(seconds) = delay.strip_suffix('s') else {
        return false;
    };
    !seconds.is_empty()
        && seconds.len() <= 24
        && seconds.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && seconds
            .parse::<f64>()
            .is_ok_and(|s| s.is_finite() && s >= 0.0 && s <= (i64::MAX / 1_000_000_000) as f64)
}

/// Explicit timestamp and stable execution ID make this pure and safe to reconstruct on retry.
/// Deliberately excludes raw keys, headers, upstream response bodies, and tokens/secret material.
pub fn usage_payload(record: &UsageRecord) -> Result<Value, &'static str> {
    if record.execution_id.trim().is_empty() || record.execution_id.len() > 256 {
        return Err("invalid execution ID");
    }
    if !record.breakdown.valid() {
        return Err("invalid v2 token breakdown");
    }
    if record.latency_ms < 0 || record.ttft_ms < 0 {
        return Err("negative usage duration");
    }
    let b = &record.breakdown;
    let status = if record.status_code != 0 {
        record.status_code
    } else if record.failed {
        500
    } else {
        200
    };
    Ok(json!({
        "accounting_version": 2, "token_breakdown": b,
        "execution_id": record.execution_id, "request_id": record.request_id,
        "provider": record.provider, "executor_type": record.executor_type, "model": record.model,
        "auth_index": record.auth_index, "timestamp": record.timestamp,
        "latency_ms": record.latency_ms, "ttft_ms": record.ttft_ms,
        "failed": record.failed, "stream": record.stream, "generate": true,
        "fail": {"status_code": status, "body": record.fail_body, "code": record.fail_code},
        "tokens": {
            "input_tokens": b.input.total_tokens, "output_tokens": b.output.total_tokens,
            "reasoning_tokens": b.output.reasoning_tokens, "cached_tokens": b.input.cache_read_tokens,
            "cache_read_tokens": b.input.cache_read_tokens, "cache_read_tokens_present": true,
            "cache_creation_tokens": b.input.cache_write_tokens, "total_tokens": b.total_tokens
        }
    }))
}
