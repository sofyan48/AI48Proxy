use super::wire::field;
use serde_yaml::Value;

pub(super) fn features(raw: &Value) -> Vec<String> {
    let mut out = Vec::new();
    // Only execution families are inspected. Home scheduling/observability and
    // arbitrary metadata are not proxy execution controls.
    for key in [
        "upstream",
        "client",
        "providers",
        "codex",
        "claude",
        "claude-code",
        "antigravity",
        "xai",
        "devin",
        "codex-header-defaults",
        "claude-header-defaults",
        "gemini-api-key",
        "interactions-api-key",
        "vertex-api-key",
        "codex-api-key",
        "claude-api-key",
        "xai-api-key",
        "meta-api-key",
        "openai-compatibility",
        "multimedia",
        "oauth-request-scoped-errors",
        "disable-claude-cloak-mode",
        "force-model-prefix",
        "request-log",
        "ws-auth",
        "api-keys",
        "shutdown-timeout",
        "redis-usage-queue-retention-seconds",
    ] {
        if let Some(value) = field(raw, key) {
            leaves(value, key, &mut out);
        }
    }
    if let Some(plugins) = field(raw, "plugins") {
        // Home and CPA default the master switch to false. Directories, store
        // credentials/revisions and even enabled instances cannot execute then.
        match field(plugins, "enabled") {
            None if plugins.is_mapping() || plugins.is_null() => {}
            Some(Value::Bool(false)) => {}
            _ => out.push("plugins.enabled".into()),
        }
    }
    if let Some(oauth) = field(raw, "oauth") {
        for key in ["providers", "request-scoped-errors"] {
            if let Some(value) = field(oauth, key) {
                leaves(value, &format!("oauth.{key}"), &mut out);
            }
        }
    }
    for prefix in ["", "requests."] {
        let container = if prefix.is_empty() {
            Some(raw)
        } else {
            field(raw, "requests")
        };
        if let Some(container) = container {
            if let Some(value) = field(container, "streaming") {
                streaming(value, &format!("{prefix}streaming"), &mut out);
            }
            for key in [
                "nonstream-keepalive-interval",
                "passthrough-headers",
                "tls-handshake-timeout-seconds",
            ] {
                if let Some(value) = field(container, key) {
                    leaves(value, &format!("{prefix}{key}"), &mut out);
                }
            }
        }
    }
    for prefix in ["", "server."] {
        let container = if prefix.is_empty() {
            Some(raw)
        } else {
            field(raw, "server")
        };
        if let Some(container) = container {
            for key in ["response-compression", "trusted-proxies", "tls"] {
                if let Some(value) = field(container, key) {
                    // Compression options have no effect when explicitly disabled.
                    if key == "response-compression"
                        && field(value, "enabled") == Some(&Value::Bool(false))
                    {
                        continue;
                    }
                    if key == "tls" && field(value, "enable") == Some(&Value::Bool(false)) {
                        continue;
                    }
                    leaves(value, &format!("{prefix}{key}"), &mut out);
                }
            }
        }
    }
    for key in [
        "disable-image-generation",
        "gpt-image-2-base-model",
        "video-result-auth-cache-ttl",
        "antigravity-signature-cache-enabled",
        "antigravity-signature-bypass-strict",
    ] {
        if let Some(value) = field(raw, key) {
            leaves(value, key, &mut out);
        }
    }
    // Additional canonical v8 execution controls not projected by normalization.
    for (container, keys) in [
        ("routing", &["force-model-prefix"][..]),
        ("access", &["api-keys"][..]),
        ("server", &["shutdown-timeout"][..]),
    ] {
        if let Some(value) = field(raw, container) {
            for key in keys {
                if let Some(v) = field(value, key) {
                    leaves(v, &format!("{container}.{key}"), &mut out);
                }
            }
        }
    }
    if let Some(observability) = field(raw, "observability") {
        for (container, key) in [
            ("logs", "request-log"),
            ("usage", "redis-usage-queue-retention-seconds"),
        ] {
            if let Some(value) = field(observability, container).and_then(|v| field(v, key)) {
                leaves(value, &format!("observability.{container}.{key}"), &mut out);
            }
        }
    }
    // Home owns routing strategy, cooldowns, aliases, model catalogs, credentials,
    // cluster/KV metadata, management and usage switches; these are not local
    // payload/transport consumers and must not reject sanitized Home defaults.
    // max-retry-interval is consumed by the coordinated gateway retry loop.
    // The runtime's reqwest build implements HTTP(S) proxies, not SOCKS.
    if field(raw, "proxy-url")
        .and_then(Value::as_str)
        .is_some_and(|v| v.trim().starts_with("socks5:") || v.trim().starts_with("socks5h:"))
    {
        out.push("proxy-url".into());
    }
    out.sort();
    out.dedup();
    out
}

fn streaming(value: &Value, path: &str, out: &mut Vec<String>) {
    let Some(map) = value.as_mapping() else {
        leaves(value, path, out);
        return;
    };
    let keepalive = field(value, "keepalive-seconds");
    for (key, v) in map {
        let Some(key) = key.as_str() else {
            continue;
        };
        if matches!(key, "keepalive-seconds" | "bootstrap-retries")
            && v.as_i64().is_some_and(|n| n <= 0)
        {
            continue;
        }
        // The pre-first-token switch is ineffective without positive keepalives.
        if key == "send-keepalive-before-ft"
            && keepalive.is_none_or(|v| v.as_i64().is_some_and(|n| n <= 0))
        {
            continue;
        }
        leaves(v, &format!("{path}.{key}"), out);
    }
}

fn inert(value: &Value, path: &str) -> bool {
    if value.is_null() || value.as_str() == Some("") {
        return true;
    }
    let key = path.rsplit('.').next().unwrap_or(path);
    // Home injects this disabled default unconditionally, including the retained
    // OAuth scope. A positive window is NOT safe to ignore.
    if key == "tos-pause-window" {
        return value.as_str().is_some_and(|s| {
            matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "0" | "off" | "false" | "none" | "disabled"
            )
        });
    }
    // These are explicitly opt-in/positive controls in the Go contract, not a
    // general rule that every false/zero unknown setting is supported.
    if matches!(
        key,
        "keepalive-seconds"
            | "bootstrap-retries"
            | "nonstream-keepalive-interval"
            | "tls-handshake-timeout-seconds"
            | "max-request-body-bytes"
    ) && value.as_i64().is_some_and(|n| n <= 0)
    {
        return true;
    }
    if value == &Value::Bool(false)
        && matches!(
            key,
            "passthrough-headers"
                | "force-model-prefix"
                | "request-log"
                | "ws-auth"
                | "send-keepalive-before-ft"
                | "disable-image-generation"
                | "disable-codex-cloaking"
                | "disable-claude-cloak-mode"
                | "stream-bootstrap-buffering"
                | "optimize-multi-agent-v2"
                | "orphan-delegation-compatibility"
                | "response-steering"
                | "model-level-cooling"
                | "inject-x-search"
                | "signature-bypass-strict"
                | "antigravity-signature-bypass-strict"
                | "upstream-request-compression"
                | "websocket-compression"
        )
    {
        return true;
    }
    false
}

fn leaves(value: &Value, path: &str, out: &mut Vec<String>) {
    if inert(value, path) {
        return;
    }
    match value {
        Value::Mapping(map) => {
            for (key, value) in map {
                if let Some(key) = key.as_str() {
                    leaves(value, &format!("{path}.{key}"), out);
                }
            }
        }
        Value::Sequence(values) => {
            for (index, value) in values.iter().enumerate() {
                leaves(value, &format!("{path}[{index}]"), out);
            }
        }
        _ => out.push(path.to_owned()),
    }
}
