use super::*;

pub(super) fn parse_config(bytes: &[u8], operation: &'static str) -> Result<Config> {
    diagnosed(
        Config::parse(bytes).map_err(|_| TerminalConfig.into()),
        operation,
    )
}
// unsupported_features includes arbitrary nested YAML keys. Emit only known schema
// families, deduplicated and bounded, never a user-controlled path or value.
fn unsupported_field_names(paths: &[String]) -> Vec<&'static str> {
    const FIELDS: &[&str] = &[
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
        "plugins",
        "oauth",
        "requests",
        "streaming",
        "nonstream-keepalive-interval",
        "passthrough-headers",
        "tls-handshake-timeout-seconds",
        "server",
        "response-compression",
        "trusted-proxies",
        "tls",
        "disable-image-generation",
        "gpt-image-2-base-model",
        "video-result-auth-cache-ttl",
        "antigravity-signature-cache-enabled",
        "antigravity-signature-bypass-strict",
        "routing",
        "access",
        "observability",
        "proxy-url",
    ];
    FIELDS
        .iter()
        .copied()
        .filter(|field| {
            paths.iter().any(|path| {
                path == field
                    || path
                        .strip_prefix(field)
                        .is_some_and(|suffix| suffix.starts_with(['.', '[']))
            })
        })
        .collect()
}
pub(super) fn validate_support(config: &Config) -> Result<()> {
    let unsupported = config.unsupported_features();
    if !unsupported.is_empty() {
        for field in unsupported_field_names(&unsupported) {
            tracing::error!(
                operation = "config_validate",
                reason = "unsupported_field",
                field,
                "Home configuration unsupported"
            );
        }
        return Err(TerminalConfig.into());
    }
    Ok(())
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    use std::io::Write;

    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }

    #[test]
    fn diagnostics_do_not_format_errors_yaml_or_arbitrary_keys() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(capture.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let secret = "https://private.example/eyJ-secret-token";
        let result: Result<()> = diagnosed(Err(anyhow::anyhow!(secret)), "worker_ping");
        assert!(result.is_err());
        assert!(parse_config(secret.as_bytes(), "config_initial").is_err());
        let config = Config::parse(format!("credential-concurrency:\n  lifecycle-config-revision: 7\nproviders:\n  '{secret}': secret-value\n").as_bytes()).unwrap();
        assert!(validate_support(&config).is_err());
        let bytes = capture.0.lock().unwrap();
        let log = std::str::from_utf8(&bytes).unwrap();
        assert!(log.contains("worker_ping"));
        assert!(log.contains("config_initial"));
        assert!(log.contains("providers"));
        for forbidden in [
            secret,
            "secret-value",
            "private.example",
            "eyJ-secret-token",
        ] {
            assert!(!log.contains(forbidden), "unsafe diagnostic");
        }
    }

    #[test]
    fn first_fence_logs_once_even_after_admission_stopped() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(capture.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let ready = AtomicBool::new(false);
        let cancel = CancellationToken::new();
        fence_membership(&ready, &cancel, "heartbeat_timeout");
        fence_membership(&ready, &cancel, "subscription_socket");
        assert!(cancel.is_cancelled());
        assert!(!ready.load(Ordering::Acquire));
        let bytes = capture.0.lock().unwrap();
        let log = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(log.matches("Home membership health lost").count(), 1);
        assert!(log.contains("heartbeat_timeout"));
        assert!(!log.contains("subscription_socket"));
    }

    #[test]
    fn unsupported_names_are_bounded_schema_families_only() {
        let paths = [
            "providers.secret.token",
            "providers[0].password",
            "plugins.enabled",
            "unknown.secret",
            "providers-secret",
        ]
        .map(str::to_owned);
        assert_eq!(
            unsupported_field_names(&paths),
            vec!["providers", "plugins"]
        );
    }
}
