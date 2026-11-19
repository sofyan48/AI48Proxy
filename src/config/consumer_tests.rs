use super::*;

fn parse(extra: &str) -> Config {
    Config::parse(
        format!("credential-concurrency: {{lifecycle-config-revision: 1}}\n{extra}").as_bytes(),
    )
    .unwrap()
}

#[test]
fn typed_transport_fields_do_not_pretend_to_have_consumers() {
    let c = parse(
        "requests: {streaming: {keepalive-seconds: 7, bootstrap-retries: 2, send-keepalive-before-ft: true}, nonstream-keepalive-interval: 5, tls-handshake-timeout-seconds: 9}\nserver: {max-request-body-bytes: 4096}\nrouting: {retry: {max-retry-interval: 30}}\n",
    );
    assert_eq!(c.streaming.keepalive_interval, Duration::from_secs(7));
    assert_eq!(c.streaming.bootstrap_retries, 2);
    assert!(c.streaming.send_keepalive_before_ft);
    assert_eq!(c.nonstream_keepalive_interval, Duration::from_secs(5));
    assert_eq!(c.tls_handshake_timeout, Duration::from_secs(9));
    assert_eq!(c.max_request_body_bytes, Some(4096));
    let unsupported = c.unsupported_features();
    for key in [
        "streaming.keepalive-seconds",
        "streaming.bootstrap-retries",
        "streaming.send-keepalive-before-ft",
        "nonstream-keepalive-interval",
        "tls-handshake-timeout-seconds",
    ] {
        assert!(unsupported.contains(&key.into()), "{key}");
    }
    assert!(!unsupported.contains(&"max-retry-interval".into()));
    assert!(!unsupported.contains(&"max-request-body-bytes".into()));
}

#[test]
fn disabled_fields_follow_go_signed_contract_and_saturate_handshake_duration() {
    let c = parse(
        "streaming: {keepalive-seconds: -1, bootstrap-retries: -2, send-keepalive-before-ft: true}\nmax-request-body-bytes: -1\nnonstream-keepalive-interval: -1\ntls-handshake-timeout-seconds: -1\n",
    );
    assert_eq!(c.max_request_body_bytes, None);
    assert_eq!(c.streaming.keepalive_interval, Duration::ZERO);
    assert_eq!(c.streaming.bootstrap_retries, 0);
    assert!(c.unsupported_features().is_empty());
    let c = parse("tls-handshake-timeout-seconds: 9223372036854775807\n");
    assert_eq!(
        c.tls_handshake_timeout,
        Duration::from_nanos(i64::MAX as u64)
    );
}

#[test]
fn added_inventory_is_key_only_for_legacy_and_canonical_layouts() {
    let c = parse(
        "force-model-prefix: true\nrequest-log: true\nws-auth: true\naccess: {api-keys: [never-print-this]}\nobservability: {logs: {request-log: true}}\n",
    );
    let keys = c.unsupported_features();
    for key in [
        "force-model-prefix",
        "request-log",
        "ws-auth",
        "access.api-keys[0]",
        "observability.logs.request-log",
    ] {
        assert!(keys.contains(&key.into()), "{key}");
    }
    assert!(!keys.join(",").contains("never-print-this"));
}
