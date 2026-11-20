//! Safe Home request-log envelope construction. Bodies and credential headers are omitted.
use serde::Serialize;
use std::{collections::BTreeMap, time::Duration};

/// Metadata for one complete client request, not one upstream retry.
/// All setters bound retained data; never pass credentials as model/provider/ID metadata.
#[derive(Clone, Serialize)]
pub struct RequestLogRecord {
    pub(crate) request_id: String,
    pub(crate) headers: BTreeMap<String, Vec<String>>,
    #[serde(skip)]
    path: String,
    #[serde(skip)]
    model: String,
    #[serde(skip)]
    provider: String,
    #[serde(skip)]
    status: u16,
    #[serde(skip)]
    latency: Duration,
    pub(crate) request_log: String,
}

impl RequestLogRecord {
    pub fn new(request_id: &str, path: &str, model: &str) -> Self {
        // Only a URL path is retained: query strings can contain API keys.
        let path = path.split(['?', '#']).next().unwrap_or("/");
        Self {
            request_id: identifier(request_id),
            headers: BTreeMap::new(),
            path: identifier(path),
            model: identifier(model),
            provider: String::new(),
            status: 0,
            latency: Duration::ZERO,
            request_log: String::new(),
        }
    }

    pub fn provider(mut self, provider: &str) -> Self {
        self.provider = identifier(provider);
        self
    }

    /// Only an allowlist of non-credential headers is retained, in both envelope and text.
    pub fn headers(mut self, headers: &http::HeaderMap) -> Self {
        for name in ["content-type", "accept"] {
            if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
                // Header parameters are not needed for diagnostics and may carry arbitrary data.
                let value = identifier(value.split(';').next().unwrap_or_default());
                self.headers.insert(name.into(), vec![value]);
            }
        }
        self
    }

    pub fn finish(mut self, status: u16, latency: Duration) -> Self {
        self.status = status;
        self.latency = latency;
        self
    }

    pub(crate) fn render(mut self) -> Self {
        use std::fmt::Write;
        self.request_log = format!(
            "=== REQUEST INFO ===\nURL: {}\nMethod: POST\nRequest ID: {}\nModel: {}\nProvider: {}\nLatency: {}ms\n\n=== REQUEST HEADERS ===\n",
            self.path,
            self.request_id,
            self.model,
            self.provider,
            self.latency.as_millis()
        );
        for (name, values) in &self.headers {
            for value in values {
                let _ = writeln!(self.request_log, "{name}: {value}");
            }
        }
        let _ = write!(
            self.request_log,
            "\n=== REQUEST BODY ===\n[omitted by worker security policy]\n\n=== RESPONSE ===\nStatus: {}\nBody: [omitted by worker security policy]\n",
            self.status
        );
        self
    }
}

fn identifier(value: &str) -> String {
    value
        .chars()
        .take(256)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '+') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes_exclude_credentials_queries_and_line_injection() {
        let mut headers = http::HeaderMap::new();
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        headers.insert("cookie", "secret".parse().unwrap());
        headers.insert(
            "content-type",
            "application/json; secret=value".parse().unwrap(),
        );
        let record = RequestLogRecord::new("id\nStatus: 200", "/v1/responses?key=secret", "model")
            .headers(&headers)
            .provider("provider")
            .finish(502, Duration::from_millis(12))
            .render();
        let raw = serde_json::to_string(&record).unwrap();
        assert!(!raw.contains("secret"));
        assert!(!raw.contains("authorization"));
        assert!(record.request_log.contains("Status: 502"));
        assert!(record.request_log.contains("Latency: 12ms"));
        assert!(record.request_log.contains("URL: /v1/responses\n"));
    }

    #[test]
    fn record_memory_and_wire_size_are_bounded() {
        let huge = "🦀".repeat(100_000);
        let record = RequestLogRecord::new(&huge, &huge, &huge)
            .provider(&huge)
            .render();
        assert!(serde_json::to_vec(&record).unwrap().len() < 8192);
    }
}
