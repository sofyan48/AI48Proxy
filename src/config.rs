//! Typed Home-managed configuration. `raw` retains unknown fields for diagnostics.
use crate::payload::PayloadRules;
use anyhow::{Context, Result, ensure};
use serde_yaml::Value;
use std::time::Duration;

#[cfg(test)]
#[path = "config/consumer_tests.rs"]
mod consumer_tests;
pub mod execution;
#[cfg(test)]
#[path = "config/execution_tests.rs"]
mod execution_tests;
#[path = "gateway/keepalive.rs"]
pub mod keepalive;
#[path = "config/multimedia.rs"]
pub mod multimedia;
#[path = "config/providers.rs"]
pub mod providers;
#[cfg(test)]
#[path = "config/tests.rs"]
mod tests;
#[path = "config/unsupported.rs"]
mod unsupported;
#[path = "config/wire.rs"]
mod wire;
use wire::{duration, field, integer, section};

// Kept under config so this owned module compiles without changing lib.rs.
// The parent may re-export it from the crate root when wiring the gateway.
#[path = "runtime_controls.rs"]
pub mod runtime_controls;

#[cfg(test)]
#[path = "config/home_projection_tests.rs"]
mod home_projection_tests;

#[derive(Clone, Debug)]
pub struct Config {
    pub disable_image_generation: multimedia::ImageGenerationMode,
    pub gpt_image_2_base_model: String,
    pub host: String,
    pub port: u16,
    pub request_retry: u32,
    /// Home runtime metadata retained for worker observability and policy decisions.
    pub home_scheduler_protocol: u32,
    pub usage_statistics_enabled: bool,
    pub disable_cooling: bool,
    pub debug: bool,
    pub logging_to_file: bool,
    pub logs_max_total_size_mb: usize,
    pub error_logs_max_files: usize,
    pub auth_auto_refresh_workers: usize,
    pub trusted_proxies: Vec<String>,
    pub quota_exceeded_antigravity_credits: bool,
    pub quota_exceeded_switch_preview_model: bool,
    pub quota_exceeded_switch_project: bool,
    /// Opt-in Home request-log forwarding; raw bodies are never captured.
    pub request_log: bool,
    pub max_retry_credentials: usize,
    pub max_retry_interval: Duration,
    pub proxy_url: Option<String>,
    pub streaming: Streaming,
    pub nonstream_keepalive_interval: Duration,
    pub passthrough_headers: bool,
    /// Home owns prefix publication and dispatch; never re-resolve its selected model.
    pub force_model_prefix: bool,
    pub oauth_request_scoped_errors: execution::OAuthErrorRules,
    /// Zero closes immediately, matching CPA's opt-in graceful drain.
    pub shutdown_timeout: Duration,
    /// None disables the inbound body limit, matching Go's <= 0 contract.
    pub max_request_body_bytes: Option<usize>,
    /// Handshake-only bound; never a whole-request timeout.
    pub tls_handshake_timeout: Duration,
    pub lifecycle: Lifecycle,
    pub observation: Observation,
    pub payload: PayloadRules,
    pub providers: providers::ProviderSettings,
    pub raw: Value,
}

#[derive(Clone, Debug, Default)]
pub struct Streaming {
    pub keepalive_interval: Duration,
    pub bootstrap_retries: usize,
    pub send_keepalive_before_ft: bool,
}

#[derive(Clone, Debug)]
pub struct Lifecycle {
    pub lifecycle_config_revision: i64,
    pub observation_barrier_revision: i64,
    pub heartbeat_timeout: Duration,
    pub cancel_bound: Duration,
    pub release_flush_interval: Duration,
    pub release_max_backoff: Duration,
    pub busy_retry_min: Duration,
    pub busy_retry_max: Duration,
}

#[derive(Clone, Debug)]
pub struct Observation {
    pub snapshot_interval: Duration,
    pub max_part_bytes: usize,
    pub max_part_count: usize,
    pub max_revision_bytes: usize,
    pub max_aggregate_groups: usize,
    pub max_details: usize,
    pub max_string_bytes: usize,
}

impl Config {
    /// Parse a managed snapshot. Missing lifecycle revisions are unsafe, not defaults.
    pub fn parse(yaml: &[u8]) -> Result<Self> {
        let raw = wire::normalize(serde_yaml::from_slice(yaml).context("invalid Home YAML")?)?;
        let host = match field(&raw, "host") {
            Some(v) => v.as_str().context("host must be a string")?.to_owned(),
            None => String::new(),
        };
        let port = u16::try_from(integer(&raw, "port", 8317)?).context("port exceeds 65535")?;
        ensure!(port > 0, "port must be positive");
        let request_log = match field(&raw, "request-log") {
            None => false,
            Some(v) => v.as_bool().context("request-log must be a boolean")?,
        };
        let request_retry = u32::try_from(integer(&raw, "request-retry", 0)?)
            .context("request-retry exceeds u32")?;
        let home_scheduler_protocol = u32::try_from(integer(&raw, "home-scheduler-protocol", 0)?)
            .context("home-scheduler-protocol exceeds u32")?;
        let usage_statistics_enabled = boolean(&raw, "usage-statistics-enabled")?;
        let disable_cooling = boolean(&raw, "disable-cooling")?;
        let debug = boolean(&raw, "debug")?;
        let logging_to_file = boolean(&raw, "logging-to-file")?;
        let logs_max_total_size_mb = integer(&raw, "logs-max-total-size-mb", 0)?;
        let error_logs_max_files = integer(&raw, "error-logs-max-files", 0)?;
        let auth_auto_refresh_workers = integer(&raw, "auth-auto-refresh-workers", 0)?;
        let trusted_proxies = field(&raw, "trusted-proxies")
            .map(|value| {
                value
                    .as_sequence()
                    .context("trusted-proxies must be a sequence")?
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map(str::to_owned)
                            .context("trusted-proxies entries must be strings")
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        let quota = section(&raw, "quota-exceeded")?;
        let quota_exceeded_antigravity_credits = boolean(quota, "antigravity-credits")?;
        let quota_exceeded_switch_preview_model = boolean(quota, "switch-preview-model")?;
        let quota_exceeded_switch_project = boolean(quota, "switch-project")?;
        let max_retry_credentials = integer(&raw, "max-retry-credentials", 0)?;
        // Go's max-retry-interval is integer seconds, not a duration string.
        let max_retry_interval =
            Duration::from_secs(integer(&raw, "max-retry-interval", 0)?.try_into()?);
        let proxy_url = match field(&raw, "proxy-url") {
            None => None,
            Some(v) => {
                let value = v.as_str().context("proxy-url must be a string")?.trim();
                if value.is_empty() {
                    None
                } else {
                    if value != "direct" {
                        let url = url::Url::parse(value).context("invalid proxy-url")?;
                        ensure!(
                            matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
                                && url.host_str().is_some(),
                            "unsupported proxy-url scheme or missing host"
                        );
                    }
                    Some(value.to_owned())
                }
            }
        };
        let streaming_value = section(&raw, "streaming")?;
        let streaming = Streaming {
            keepalive_interval: positive_seconds(streaming_value, "keepalive-seconds")?,
            bootstrap_retries: positive_integer(streaming_value, "bootstrap-retries")?,
            send_keepalive_before_ft: match field(streaming_value, "send-keepalive-before-ft") {
                None => false,
                Some(v) => v
                    .as_bool()
                    .context("send-keepalive-before-ft must be a boolean")?,
            },
        };
        let nonstream_keepalive_interval = positive_seconds(&raw, "nonstream-keepalive-interval")?;
        let max_request_body_bytes = match positive_integer(&raw, "max-request-body-bytes")? {
            0 => None,
            n => Some(n),
        };
        let tls_handshake_timeout = positive_seconds(&raw, "tls-handshake-timeout-seconds")?;
        let passthrough_headers = boolean(&raw, "passthrough-headers")?;
        let force_model_prefix = boolean(&raw, "force-model-prefix")?;
        let oauth_request_scoped_errors =
            execution::oauth_rules(field(&raw, "oauth-request-scoped-errors"))?;
        let shutdown_timeout = positive_seconds(&raw, "shutdown-timeout")?;
        let lifecycle = Lifecycle::parse(section(&raw, "credential-concurrency")?)?;
        let observation = Observation::parse(section(&raw, "credential-in-flight")?)?;
        let payload = match field(&raw, "payload") {
            Some(v) => PayloadRules::parse(v)?,
            None => PayloadRules::default(),
        };
        Ok(Self {
            disable_image_generation: multimedia::ImageGenerationMode::parse(field(
                &raw,
                "disable-image-generation",
            ))?,
            gpt_image_2_base_model: multimedia::base_model(&raw)?,
            host,
            port,
            request_retry,
            home_scheduler_protocol,
            usage_statistics_enabled,
            disable_cooling,
            debug,
            logging_to_file,
            logs_max_total_size_mb,
            error_logs_max_files,
            auth_auto_refresh_workers,
            trusted_proxies,
            quota_exceeded_antigravity_credits,
            quota_exceeded_switch_preview_model,
            quota_exceeded_switch_project,
            request_log,
            max_retry_credentials,
            max_retry_interval,
            proxy_url,
            streaming,
            nonstream_keepalive_interval,
            passthrough_headers,
            force_model_prefix,
            oauth_request_scoped_errors,
            shutdown_timeout,
            max_request_body_bytes,
            tls_handshake_timeout,
            lifecycle,
            observation,
            payload,
            providers: providers::ProviderSettings::parse(&raw)?,
            raw,
        })
    }

    /// Known execution settings without a supported Rust consumer. Unknown metadata
    /// is deliberately not treated as an execution feature.
    pub fn unsupported_features(&self) -> Vec<String> {
        unsupported::features(&self.raw)
    }
}

fn boolean(value: &Value, key: &str) -> Result<bool> {
    field(value, key).map_or(Ok(false), |v| {
        v.as_bool()
            .with_context(|| format!("{key} must be a boolean"))
    })
}

fn positive_integer(value: &Value, key: &str) -> Result<usize> {
    let n = match field(value, key) {
        None => 0,
        Some(v) => v
            .as_i64()
            .with_context(|| format!("{key} must be an integer"))?,
    };
    Ok(usize::try_from(n.max(0))?)
}

fn positive_seconds(value: &Value, key: &str) -> Result<Duration> {
    let seconds = positive_integer(value, key)? as u64;
    // Go time.Duration is signed int64 nanoseconds; saturate like the Go consumer.
    Ok(Duration::from_nanos(
        seconds.saturating_mul(1_000_000_000).min(i64::MAX as u64),
    ))
}

impl Lifecycle {
    fn parse(value: &Value) -> Result<Self> {
        let revision = field(value, "lifecycle-config-revision")
            .and_then(Value::as_i64).context(
                "managed mode requires a positive credential-concurrency.lifecycle-config-revision from Home"
            )?;
        ensure!(
            revision > 0,
            "managed mode requires a positive lifecycle-config-revision from Home"
        );
        let barrier = integer(value, "observation-barrier-revision", 0)?;
        let result = Self {
            lifecycle_config_revision: revision,
            observation_barrier_revision: i64::try_from(barrier)
                .context("observation-barrier-revision exceeds i64")?,
            heartbeat_timeout: duration(value, "cpa-heartbeat-timeout", "3s", true)?,
            cancel_bound: duration(value, "cpa-cancel-bound", "5s", true)?,
            release_flush_interval: duration(value, "release-flush-interval", "250ms", false)?,
            release_max_backoff: duration(value, "release-max-backoff", "2s", false)?,
            busy_retry_min: duration(value, "busy-retry-min", "250ms", false)?,
            busy_retry_max: duration(value, "busy-retry-max", "1s", false)?,
        };
        ensure!(
            result.release_max_backoff >= result.release_flush_interval,
            "release-max-backoff must not be less than release-flush-interval"
        );
        ensure!(
            result.busy_retry_max >= result.busy_retry_min,
            "busy-retry-max must not be less than busy-retry-min"
        );
        for (name, duration) in [
            ("busy-retry-min", result.busy_retry_min),
            ("busy-retry-max", result.busy_retry_max),
        ] {
            ensure!(
                duration.as_nanos() % 1_000_000 == 0,
                "{name} must be a positive whole millisecond duration"
            );
        }
        // These Home-owned settings are not runtime fields here, but malformed
        // supplied values must still be rejected.
        duration(value, "reclaim-grace", "5s", true)?;
        duration(value, "cleanup-interval", "5s", true)?;
        if field(value, "max-limit").is_some() {
            let limit = integer(value, "max-limit", 0)?;
            ensure!(
                limit > 0 && limit <= 1_000_000,
                "max-limit outside hard bounds"
            );
        }
        Ok(result)
    }
}

impl Observation {
    fn parse(value: &Value) -> Result<Self> {
        let result = Self {
            snapshot_interval: duration(value, "snapshot-interval", "2s", false)?,
            max_part_bytes: integer(value, "max-part-bytes", 256 * 1024)?,
            max_part_count: integer(value, "max-part-count", 64)?,
            max_revision_bytes: integer(value, "max-revision-bytes", 16 * 1024 * 1024)?,
            max_aggregate_groups: integer(value, "max-aggregate-groups", 100_000)?,
            max_details: integer(value, "max-details", 10_000)?,
            max_string_bytes: integer(value, "max-string-bytes", 256)?,
        };
        let stale = duration(value, "stale-after", "10s", false)?;
        ensure!(
            result.snapshot_interval <= stale / 3,
            "credential-in-flight.stale-after must be at least three snapshot intervals"
        );
        duration(value, "staging-retention", "1m", false)?;
        ensure!(
            result.max_part_bytes >= 1024,
            "max-part-bytes must be at least 1024"
        );
        ensure!(
            (1..=64).contains(&result.max_part_count),
            "max-part-count must be in 1..=64"
        );
        ensure!(
            result.max_revision_bytes >= result.max_part_bytes
                && result.max_revision_bytes <= 16 * 1024 * 1024,
            "max-revision-bytes outside hard bounds"
        );
        ensure!(
            result.max_revision_bytes.div_ceil(result.max_part_bytes) <= result.max_part_count,
            "max-revision-bytes exceeds part capacity"
        );
        ensure!(
            (1..=100_000).contains(&result.max_aggregate_groups),
            "max-aggregate-groups outside hard bounds"
        );
        // Home permits zero details to disable detail collection.
        ensure!(result.max_details <= 10_000, "max-details exceeds 10000");
        ensure!(
            (1..=256).contains(&result.max_string_bytes),
            "max-string-bytes outside hard bounds"
        );
        Ok(result)
    }
}
