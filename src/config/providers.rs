//! Typed provider-wide execution settings distributed by Home (flat CPA layout).
use super::wire::{field, section};
use anyhow::{Context, Result};
use serde_yaml::Value;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProviderSettings {
    pub codex_user_agent: String,
    pub codex_beta_features: String,
    pub codex_request_compression: bool,
    pub codex_request_compression_min_bytes: usize,
    /// Hold pre-generation Codex SSE frames so overload rejections can fail over.
    pub codex_stream_bootstrap_buffering: bool,
    /// Optional wall-clock ceiling for that hold; None = bounded by frames/bytes only.
    pub codex_stream_bootstrap_timeout: Option<std::time::Duration>,
    /// `client.codex.optimize-multi-agent-v2`: official Codex client multi-agent rewrites.
    pub codex_optimize_multi_agent_v2: bool,
    /// `codex.orphan-delegation-compatibility`: orphan codex_app outputs become user messages.
    pub codex_orphan_delegation: bool,
    /// Full-duplex native Codex WebSockets: `response.steer` while a response is
    /// active, bound to the session's single Home selection and upstream socket.
    pub codex_response_steering: bool,
    /// Go permessage-deflate preference (unset = true). The worker's WebSocket
    /// stack never negotiates compression, so only `false` matches Go exactly.
    pub codex_websocket_compression: bool,
    pub xai_inject_x_search: bool,
    pub disable_claude_cloak_mode: bool,
    pub claude_headers: ClaudeHeaderDefaults,
    /// None = short-lived connections (Go default when not explicitly enabled).
    pub antigravity_pool: Option<AntigravityPool>,
    pub antigravity_sensitive: AntigravitySensitive,
}

/// Antigravity system-instruction obfuscation rules (`antigravity.sensitive-*`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AntigravitySensitive {
    /// Words obfuscated with U+034F per alphanumeric subword.
    pub words: Vec<String>,
    /// Literal replacements: trimmed keys, sorted by key (Go NormalizeSensitiveWordsReplace).
    pub replace: Vec<(String, String)>,
    /// Ordered (regex, replacement) rules; invalid or blank patterns are dropped.
    pub patterns: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AntigravityPool {
    pub idle: std::time::Duration,
    pub max_idle_per_host: usize,
}

/// Fallbacks used only when the caller did not send the header itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClaudeHeaderDefaults {
    pub user_agent: String,
    pub package_version: String,
    pub runtime_version: String,
    pub os: String,
    pub arch: String,
    pub timeout: String,
    pub timezone: String,
    pub beta_strip: Vec<String>,
    pub beta_append: Vec<String>,
    /// ""/"on" (default), "off" or "passthrough".
    pub client_request_id: String,
}

impl ClaudeHeaderDefaults {
    pub fn pairs(&self) -> [(&'static str, &str); 6] {
        [
            ("user-agent", &self.user_agent),
            ("x-stainless-package-version", &self.package_version),
            ("x-stainless-runtime-version", &self.runtime_version),
            ("x-stainless-os", &self.os),
            ("x-stainless-arch", &self.arch),
            ("x-stainless-timeout", &self.timeout),
        ]
    }
}

/// Paths consumed by `ProviderSettings`; the unsupported-feature gate skips them.
pub(super) const CONSUMED: &[&str] = &[
    "codex.upstream-request-compression",
    "codex.upstream-request-compression-min-bytes",
    "codex.stream-bootstrap-buffering",
    "codex.stream-bootstrap-timeout",
    "codex.orphan-delegation-compatibility",
    "client.codex.optimize-multi-agent-v2",
    "codex.response-steering",
    "codex.websocket-compression",
    "codex-header-defaults.user-agent",
    "codex-header-defaults.beta-features",
    "xai.inject-x-search",
    "claude-header-defaults.user-agent",
    "claude-header-defaults.package-version",
    "claude-header-defaults.runtime-version",
    "claude-header-defaults.os",
    "claude-header-defaults.arch",
    "claude-header-defaults.timeout",
    "claude-header-defaults.client-request-id",
    "claude-header-defaults.timezone",
    "disable-claude-cloak-mode",
    "antigravity.connection-pool.enabled",
    "antigravity.connection-pool.idle-conn-timeout",
    "antigravity.connection-pool.max-idle-conns-per-host",
    "antigravity.sensitive-words-replace",
];

/// Mapping-valued consumed fields (keys are reported as `path.<key>`).
pub(super) const CONSUMED_PREFIXES: &[&str] = &["antigravity.sensitive-words-replace."];

fn scalar(value: &Value, what: &str) -> Result<String> {
    match value {
        Value::Null => Ok(String::new()),
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => anyhow::bail!("{what} must be a string"),
    }
}

/// Go SanitizeAntigravity: replacement keys are trimmed (blank dropped, the
/// untrimmed key wins over a trimmed duplicate); values are kept verbatim so an
/// empty value deletes the phrase. Invalid RE2 patterns are logged and dropped.
fn antigravity_sensitive(raw: &Value) -> Result<AntigravitySensitive> {
    let ag = section(raw, "antigravity")?;
    let words = list(ag, "sensitive-words")?;

    let mut entries: Vec<(String, String)> = Vec::new();
    for (key, value) in section(ag, "sensitive-words-replace")?
        .as_mapping()
        .into_iter()
        .flatten()
    {
        entries.push((
            scalar(key, "sensitive-words-replace keys")?,
            scalar(value, "sensitive-words-replace values")?,
        ));
    }
    entries.sort();
    let mut replace: Vec<(String, String)> = Vec::new();
    for (raw_key, value) in entries {
        let key = raw_key.trim();
        if key.is_empty() {
            continue;
        }
        match replace.iter_mut().find(|(k, _)| k == key) {
            Some(_) if raw_key != key => {}
            Some(slot) => slot.1 = value,
            None => replace.push((key.to_owned(), value)),
        }
    }
    replace.sort();

    let mut patterns = Vec::new();
    match field(ag, "sensitive-patterns-replace") {
        None | Some(Value::Null) => {}
        Some(Value::Sequence(items)) => {
            for (index, item) in items.iter().enumerate() {
                anyhow::ensure!(
                    item.is_mapping(),
                    "sensitive-patterns-replace entries must be mappings"
                );
                let pattern = scalar(field(item, "pattern").unwrap_or(&Value::Null), "pattern")?;
                let replacement = scalar(
                    field(item, "replacement").unwrap_or(&Value::Null),
                    "replacement",
                )?;
                if pattern.trim().is_empty() {
                    continue;
                }
                if regex::Regex::new(&pattern).is_err() {
                    // Never log the pattern itself: it may embed prompt text.
                    tracing::warn!(
                        rule_index = index + 1,
                        "sensitive-patterns-replace rule dropped: invalid regex"
                    );
                    continue;
                }
                patterns.push((pattern, replacement));
            }
        }
        Some(_) => anyhow::bail!("sensitive-patterns-replace must be a sequence"),
    }
    Ok(AntigravitySensitive {
        words,
        replace,
        patterns,
    })
}

/// Go resolveAntigravityPoolSettings: opt-in, 30s/2 defaults, 210s/100 caps,
/// non-positive timeout or negative idle count selects short mode.
fn antigravity_pool(raw: &Value) -> Result<Option<AntigravityPool>> {
    let pool = section(section(raw, "antigravity")?, "connection-pool")?;
    if !flag(pool, "enabled")? {
        return Ok(None);
    }
    let mut idle = std::time::Duration::from_secs(30);
    let timeout = string(pool, "idle-conn-timeout")?;
    if !timeout.is_empty() {
        // Go logs and keeps the default on an unparsable value.
        if let Ok(parsed) = super::wire::parse_go_duration(&timeout) {
            if parsed.is_zero() {
                return Ok(None);
            }
            idle = parsed.min(std::time::Duration::from_secs(210));
        }
    }
    let mut max = 2usize;
    if let Some(v) = field(pool, "max-idle-conns-per-host").filter(|v| !v.is_null()) {
        let n = v
            .as_i64()
            .context("max-idle-conns-per-host must be an integer")?;
        if n < 0 {
            return Ok(None);
        }
        max = (n as usize).min(100);
    }
    Ok(Some(AntigravityPool {
        idle,
        max_idle_per_host: max,
    }))
}

/// Sequence-valued consumed fields (entries are reported as `path[i]`).
pub(super) const CONSUMED_LISTS: &[&str] = &[
    "claude-header-defaults.anthropic-beta-strip",
    "claude-header-defaults.anthropic-beta-append",
    "antigravity.sensitive-words",
    "antigravity.sensitive-patterns-replace",
];

fn list(value: &Value, key: &str) -> Result<Vec<String>> {
    match field(value, key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Sequence(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(|s| s.trim().to_owned())
                    .with_context(|| format!("{key} entries must be strings"))
            })
            .filter(|v| !matches!(v, Ok(s) if s.is_empty()))
            .collect(),
        Some(_) => anyhow::bail!("{key} must be a sequence"),
    }
}

fn string(value: &Value, key: &str) -> Result<String> {
    match field(value, key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.trim().to_owned()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        Some(_) => anyhow::bail!("{key} must be a string"),
    }
}

/// Go StreamBootstrapTimeoutDuration: Go durations or integer seconds; zero,
/// negative, disabled keywords and invalid values all mean unlimited (None).
fn bootstrap_timeout(codex: &Value) -> Result<Option<std::time::Duration>> {
    let raw = string(codex, "stream-bootstrap-timeout")?;
    if matches!(
        raw.to_ascii_lowercase().as_str(),
        "" | "0" | "none" | "unlimited" | "disabled" | "off" | "never"
    ) {
        return Ok(None);
    }
    let parsed = super::wire::parse_go_duration(&raw)
        .ok()
        .or_else(|| raw.parse::<u64>().ok().map(std::time::Duration::from_secs));
    Ok(parsed.filter(|d| !d.is_zero()))
}

fn flag(value: &Value, key: &str) -> Result<bool> {
    flag_or(value, key, false)
}

fn flag_or(value: &Value, key: &str, default: bool) -> Result<bool> {
    match field(value, key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_bool()
            .with_context(|| format!("{key} must be a boolean")),
    }
}

impl ProviderSettings {
    pub(super) fn parse(raw: &Value) -> Result<Self> {
        let codex = section(raw, "codex")?;
        let codex_headers = section(raw, "codex-header-defaults")?;
        let claude = section(raw, "claude-header-defaults")?;
        let xai = section(raw, "xai")?;
        let client_codex = section(section(raw, "client")?, "codex")?;
        let min = match field(codex, "upstream-request-compression-min-bytes") {
            None | Some(Value::Null) => 0,
            Some(v) => v
                .as_i64()
                .context("upstream-request-compression-min-bytes must be an integer")?,
        };
        Ok(Self {
            codex_user_agent: string(codex_headers, "user-agent")?,
            codex_beta_features: string(codex_headers, "beta-features")?,
            codex_request_compression: flag(codex, "upstream-request-compression")?,
            // Go: zero or negative means 1024.
            codex_request_compression_min_bytes: if min <= 0 { 1024 } else { min as usize },
            codex_stream_bootstrap_buffering: flag(codex, "stream-bootstrap-buffering")?,
            codex_stream_bootstrap_timeout: bootstrap_timeout(codex)?,
            codex_optimize_multi_agent_v2: flag(client_codex, "optimize-multi-agent-v2")?,
            codex_orphan_delegation: flag(codex, "orphan-delegation-compatibility")?,
            codex_response_steering: flag(codex, "response-steering")?,
            codex_websocket_compression: flag_or(codex, "websocket-compression", true)?,
            xai_inject_x_search: flag(xai, "inject-x-search")?,
            antigravity_pool: antigravity_pool(raw)?,
            antigravity_sensitive: antigravity_sensitive(raw)?,
            disable_claude_cloak_mode: flag(raw, "disable-claude-cloak-mode")?,
            claude_headers: ClaudeHeaderDefaults {
                user_agent: string(claude, "user-agent")?,
                package_version: string(claude, "package-version")?,
                runtime_version: string(claude, "runtime-version")?,
                os: string(claude, "os")?,
                arch: string(claude, "arch")?,
                timeout: string(claude, "timeout")?,
                timezone: string(claude, "timezone")?,
                beta_strip: list(claude, "anthropic-beta-strip")?,
                beta_append: list(claude, "anthropic-beta-append")?,
                client_request_id: {
                    let mode = string(claude, "client-request-id")?.to_ascii_lowercase();
                    anyhow::ensure!(
                        matches!(mode.as_str(), "" | "on" | "off" | "passthrough"),
                        "claude-header-defaults.client-request-id must be on, off or passthrough"
                    );
                    mode
                },
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_home_projection_layout() {
        let raw: Value = serde_yaml::from_str(
            "codex: {upstream-request-compression: true, upstream-request-compression-min-bytes: 0}\n\
             codex-header-defaults: {user-agent: ua, beta-features: 'a,b'}\n\
             claude-header-defaults: {os: Linux, timeout: 300}\n\
             xai: {inject-x-search: true}\n",
        )
        .unwrap();
        let s = ProviderSettings::parse(&raw).unwrap();
        assert!(s.codex_request_compression && s.xai_inject_x_search);
        assert_eq!(s.codex_request_compression_min_bytes, 1024);
        assert_eq!(s.codex_beta_features, "a,b");
        assert_eq!(s.claude_headers.os, "Linux");
        assert_eq!(s.claude_headers.timeout, "300");
        assert_eq!(
            ProviderSettings::parse(&Value::Null).unwrap(),
            ProviderSettings {
                codex_request_compression_min_bytes: 1024,
                codex_websocket_compression: true,
                ..Default::default()
            }
        );
    }

    #[test]
    fn codex_websocket_flags_match_go_defaults() {
        let parse = |yaml: &str| {
            ProviderSettings::parse(&serde_yaml::from_str::<Value>(yaml).unwrap()).unwrap()
        };
        let unset = parse("codex: {}");
        assert!(!unset.codex_response_steering && unset.codex_websocket_compression);
        let set = parse("codex: {response-steering: true, websocket-compression: false}");
        assert!(set.codex_response_steering && !set.codex_websocket_compression);
        assert!(
            ProviderSettings::parse(
                &serde_yaml::from_str::<Value>("codex: {response-steering: on}").unwrap()
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod pool_tests {
    use super::*;
    use std::time::Duration;

    fn pool(yaml: &str) -> Option<AntigravityPool> {
        antigravity_pool(&serde_yaml::from_str(yaml).unwrap()).unwrap()
    }

    #[test]
    fn antigravity_pool_matches_go_resolution() {
        assert_eq!(pool("{}"), None);
        assert_eq!(
            pool("antigravity: {connection-pool: {enabled: false}}"),
            None
        );
        assert_eq!(
            pool("antigravity: {connection-pool: {enabled: true}}"),
            Some(AntigravityPool {
                idle: Duration::from_secs(30),
                max_idle_per_host: 2
            })
        );
        assert_eq!(
            pool(
                "antigravity: {connection-pool: {enabled: true, idle-conn-timeout: 1h, max-idle-conns-per-host: 500}}"
            ),
            Some(AntigravityPool {
                idle: Duration::from_secs(210),
                max_idle_per_host: 100
            })
        );
        assert_eq!(
            pool("antigravity: {connection-pool: {enabled: true, idle-conn-timeout: 0s}}"),
            None
        );
        assert_eq!(
            pool("antigravity: {connection-pool: {enabled: true, idle-conn-timeout: -5s}}"),
            None
        );
        assert_eq!(
            pool("antigravity: {connection-pool: {enabled: true, max-idle-conns-per-host: -1}}"),
            None
        );
        // Unparsable timeout keeps the default, as Go logs and continues.
        assert_eq!(
            pool("antigravity: {connection-pool: {enabled: true, idle-conn-timeout: bogus}}")
                .unwrap()
                .idle,
            Duration::from_secs(30)
        );
    }

    #[test]
    fn codex_multi_agent_flags_are_consumed() {
        let config = crate::config::Config::parse(
            b"credential-concurrency: {lifecycle-config-revision: 7}\n\
client: {codex: {optimize-multi-agent-v2: true}}\n\
codex: {orphan-delegation-compatibility: true}\n",
        )
        .unwrap();
        assert!(config.providers.codex_optimize_multi_agent_v2);
        assert!(config.providers.codex_orphan_delegation);
        assert!(
            config.unsupported_features().is_empty(),
            "{:?}",
            config.unsupported_features()
        );
        let unset = ProviderSettings::parse(&Value::Null).unwrap();
        assert!(!unset.codex_optimize_multi_agent_v2 && !unset.codex_orphan_delegation);
    }

    #[test]
    fn consumed_provider_fields_pass_the_support_gate() {
        let config = crate::config::Config::parse(
            b"credential-concurrency: {lifecycle-config-revision: 7}\n\
antigravity: {tos-pause-window: '0', connection-pool: {enabled: true, idle-conn-timeout: 60s, max-idle-conns-per-host: 4}}\n\
antigravity-signature-cache-enabled: true\nantigravity-signature-bypass-strict: true\n\
claude-header-defaults: {anthropic-beta-strip: [x], anthropic-beta-append: [y], client-request-id: 'off', os: Linux}\n",
        )
        .unwrap();
        assert!(
            config.unsupported_features().is_empty(),
            "{:?}",
            config.unsupported_features()
        );
    }

    #[test]
    fn codex_stream_bootstrap_settings_match_go_resolution() {
        let parse = |yaml: &str| {
            let config = crate::config::Config::parse(
                format!("credential-concurrency: {{lifecycle-config-revision: 7}}\n{yaml}")
                    .as_bytes(),
            )
            .unwrap();
            assert!(
                config.unsupported_features().is_empty(),
                "{:?}",
                config.unsupported_features()
            );
            config.providers
        };
        let s = parse("codex: {stream-bootstrap-buffering: true, stream-bootstrap-timeout: 20s}");
        assert!(s.codex_stream_bootstrap_buffering);
        assert_eq!(
            s.codex_stream_bootstrap_timeout,
            Some(Duration::from_secs(20))
        );
        assert_eq!(
            parse("codex: {stream-bootstrap-timeout: 15}").codex_stream_bootstrap_timeout,
            Some(Duration::from_secs(15))
        );
        for value in ["'0'", "0s", "-5s", "''", "off", "bogus"] {
            let s = parse(&format!("codex: {{stream-bootstrap-timeout: {value}}}"));
            assert_eq!(s.codex_stream_bootstrap_timeout, None, "{value}");
            assert!(!s.codex_stream_bootstrap_buffering);
        }
    }

    #[test]
    fn antigravity_sensitive_matches_go_normalization() {
        let config = crate::config::Config::parse(
            b"credential-concurrency: {lifecycle-config-revision: 7}\nantigravity:\n  sensitive-words: [Codex, ' ', GPT-5]\n  sensitive-words-replace: {' Claude ': A, Claude: B, '  ': C, RemoveMe: ''}\n  sensitive-patterns-replace:\n    - {pattern: '[invalid', replacement: x}\n    - {pattern: 'secret-\\d+', replacement: '[redacted]'}\n    - {pattern: ' ', replacement: y}\n",
        )
        .unwrap();
        let sensitive = &config.providers.antigravity_sensitive;
        assert_eq!(sensitive.words, vec!["Codex", "GPT-5"]);
        assert_eq!(
            sensitive.replace,
            vec![
                ("Claude".to_owned(), "B".to_owned()),
                ("RemoveMe".to_owned(), String::new()),
            ]
        );
        assert_eq!(
            sensitive.patterns,
            vec![(r"secret-\d+".to_owned(), "[redacted]".to_owned())]
        );
        assert!(
            config.unsupported_features().is_empty(),
            "{:?}",
            config.unsupported_features()
        );
    }
}
