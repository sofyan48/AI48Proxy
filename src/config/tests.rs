use super::*;

const BASE: &str = "credential-concurrency:\n  lifecycle-config-revision: 7\n";
fn parse(extra: &str) -> Result<Config> {
    Config::parse(format!("{BASE}{extra}").as_bytes())
}

#[test]
fn defaults_match_home() {
    let c = parse("").unwrap();
    assert_eq!(c.port, 8317);
    assert_eq!(c.host, "");
    assert_eq!(c.request_retry, 0);
    assert_eq!(c.max_retry_credentials, 0);
    assert_eq!(c.max_retry_interval, Duration::ZERO);
    assert_eq!(c.lifecycle.lifecycle_config_revision, 7);
    assert_eq!(c.lifecycle.observation_barrier_revision, 0);
    assert_eq!(c.lifecycle.heartbeat_timeout, Duration::from_secs(3));
    assert_eq!(c.lifecycle.cancel_bound, Duration::from_secs(5));
    assert_eq!(
        c.lifecycle.release_flush_interval,
        Duration::from_millis(250)
    );
    assert_eq!(c.observation.snapshot_interval, Duration::from_secs(2));
    assert_eq!(c.observation.max_part_bytes, 262_144);
    assert_eq!(c.observation.max_part_count, 64);
    assert!(c.unsupported_features().is_empty());
}

#[test]
fn flattened_and_verified_v8_are_equivalent() {
    let flat = parse("host: localhost\nport: 9000\nrequest-retry: 2\nmax-retry-credentials: 3\nmax-retry-interval: 4\nproxy-url: direct\n").unwrap();
    let nested = Config::parse(b"server: {host: localhost, port: 9000}\nrouting: {retry: {request-retry: 2, max-retry-credentials: 3, max-retry-interval: 4}}\nrequests: {proxy-url: direct}\ncredentials: {concurrency: {lifecycle-config-revision: 7}}\n").unwrap();
    assert_eq!(flat.raw, nested.raw);
    assert_eq!(nested.max_retry_interval, Duration::from_secs(4));
}

#[test]
fn revisions_are_mandatory_and_nonnegative() {
    for yaml in [
        "{}",
        "credential-concurrency: {}",
        "credential-concurrency: {lifecycle-config-revision: 0}",
        "credential-concurrency: {lifecycle-config-revision: -1}",
    ] {
        assert!(
            Config::parse(yaml.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("revision")
        );
    }
    assert!(parse("  observation-barrier-revision: -1\n").is_err());
}

#[test]
fn duplicate_keys_and_mixed_layout_conflicts_fail() {
    for extra in [
        "port: 1\nport: 2\n",
        "metadata: {x: 1, x: 2}\n",
        "port: 9000\nserver: {port: 9001}\n",
        "requests: null\n",
        "<<: {}\n",
    ] {
        assert!(parse(extra).is_err(), "{extra}");
    }
    assert!(parse("port: 9000\nserver: {port: 9000}\n").is_ok());
}

#[test]
fn durations_support_go_strings_and_wire_nanoseconds() {
    let c = parse("  cpa-heartbeat-timeout: 3000000000\n  cpa-cancel-bound: 1m2.5s\n  release-flush-interval: 250000us\n").unwrap();
    assert_eq!(c.lifecycle.heartbeat_timeout, Duration::from_secs(3));
    assert_eq!(c.lifecycle.cancel_bound, Duration::from_millis(62_500));
    assert_eq!(
        c.lifecycle.release_flush_interval,
        Duration::from_millis(250)
    );
    for value in ["0s", "-1s", "1d", "99999999999999999999h", "1.2.3s", "null"] {
        assert!(
            parse(&format!("  cpa-heartbeat-timeout: {value}\n")).is_err(),
            "{value}"
        );
    }
    assert!(parse("  busy-retry-min: 1.1ms\n").is_err());
    assert!(parse("  busy-retry-max: 100ms\n").is_err());
    assert!(parse("  release-max-backoff: 1ms\n").is_err());
}

#[test]
fn invalid_bounds_and_types_fail_but_semantic_zeros_survive() {
    for extra in [
        "port: 0\n",
        "port: 65536\n",
        "request-retry: -1\n",
        "request-retry: 4294967296\n",
        "max-retry-credentials: -1\n",
        "max-retry-interval: 2s\n",
        "proxy-url: ftp://example.com\n",
        "credential-in-flight: {max-part-count: 0}\n",
        "credential-in-flight: {max-details: 10001}\n",
        "credential-in-flight: {max-part-count: 1}\n",
        "credential-in-flight: {max-string-bytes: 0}\n",
        "credential-in-flight: {snapshot-interval: 4s}\n",
    ] {
        assert!(parse(extra).is_err(), "{extra}");
    }
    assert!(parse("request-retry: 0\nmax-retry-credentials: 0\nmax-retry-interval: 0\ncredential-in-flight: {max-details: 0}\n").is_ok());
}

#[test]
fn sanitized_home_defaults_pass_support_validation() {
    // config_yaml.go injects antigravity and forces these Home-mode scalars.
    // Disabled plugins retain store/config metadata in the distributed document.
    let c = parse(
        r#"
usage-statistics-enabled: true
disable-cooling: true
save-cooldown-status: false
antigravity: {tos-pause-window: '0'}
oauth:
  providers:
    antigravity: {tos-pause-window: '0'}
plugins:
  enabled: false
  dir: plugins
  auth-revision: 42
  store-sources: [https://registry.example.invalid]
  configs: {example: {enabled: true, priority: 10, secret: ignored-while-disabled}}
streaming:
  keepalive-seconds: 0
  bootstrap-retries: 0
  send-keepalive-before-ft: false
nonstream-keepalive-interval: 0
passthrough-headers: false
tls-handshake-timeout-seconds: 0
multimedia: {disable-image-generation: false}
max-retry-interval: 0
proxy-url: ''
metadata: {future: true}
"#,
    )
    .unwrap();
    assert_eq!(c.unsupported_features(), Vec::<String>::new());
    let c = parse("requests: {streaming: {keepalive-seconds: -1, bootstrap-retries: -1, send-keepalive-before-ft: true}, nonstream-keepalive-interval: 0, passthrough-headers: false}\nplugins: {dir: plugins, configs: {example: {enabled: true}}}\n").unwrap();
    assert!(c.unsupported_features().is_empty());
}

#[test]
fn effective_unsupported_settings_still_fail_support_validation() {
    for (yaml, expected) in [
        (
            "antigravity: {tos-pause-window: 6h}\n",
            "antigravity.tos-pause-window",
        ),
        ("plugins: {enabled: true}\n", "plugins.enabled"),
        ("proxy-url: socks5://localhost:1080\n", "proxy-url"),
        (
            "upstream: {codex: {response-steering: true}}\n",
            "upstream.codex.response-steering",
        ),
        (
            "upstream: {claude: {header-defaults: {user-agent: custom}}}\n",
            "upstream.claude.header-defaults.user-agent",
        ),
        (
            "tls-handshake-timeout-seconds: 9\n",
            "tls-handshake-timeout-seconds",
        ),
    ] {
        assert!(
            parse(yaml)
                .unwrap()
                .unsupported_features()
                .contains(&expected.to_owned()),
            "{yaml}"
        );
    }
    for proxy in ["direct", "http://localhost:3128", "https://localhost:3128"] {
        assert!(
            parse(&format!("proxy-url: {proxy}\n"))
                .unwrap()
                .unsupported_features()
                .is_empty()
        );
    }
}

#[test]
fn unsupported_inventory_is_precise_and_never_contains_values() {
    let c = parse("metadata: {future: true}\nplugins: {enabled: true}\ncodex: {response-steering: true}\nrequests: {streaming: {keepalive-seconds: 5}, passthrough-headers: true}\n").unwrap();
    assert_eq!(
        c.unsupported_features(),
        vec!["codex.response-steering", "plugins.enabled"]
    );
    assert!(c.raw.get("metadata").is_some());
}
