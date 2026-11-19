use anyhow::{Context, Result, bail, ensure};
use serde_yaml::{Mapping, Value};
use std::time::Duration;

pub(super) fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_mapping()?.get(Value::String(key.into()))
}

pub(super) fn section<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    match field(value, key) {
        Some(v) => {
            ensure!(v.is_mapping(), "{key} must be a mapping");
            Ok(v)
        }
        None => Ok(&Value::Null),
    }
}

pub(super) fn integer(value: &Value, key: &str, default: usize) -> Result<usize> {
    match field(value, key) {
        None => Ok(default),
        Some(v) => usize::try_from(
            v.as_u64()
                .with_context(|| format!("{key} must be a nonnegative integer"))?,
        )
        .with_context(|| format!("{key} exceeds usize")),
    }
}

pub(super) fn duration(value: &Value, key: &str, default: &str, nanos: bool) -> Result<Duration> {
    let parsed = match field(value, key) {
        None => parse_duration(default),
        Some(Value::String(s)) => parse_duration(s),
        Some(v) if nanos => v
            .as_u64()
            .context("expected nonnegative nanoseconds")
            .and_then(|n| {
                ensure!(n <= i64::MAX as u64, "duration overflow");
                Ok(Duration::from_nanos(n))
            }),
        _ => bail!(
            "{key} must be a Go duration string{}",
            if nanos { " or integer nanoseconds" } else { "" }
        ),
    }
    .with_context(|| format!("invalid {key}"))?;
    ensure!(!parsed.is_zero(), "{key} must be positive");
    Ok(parsed)
}

// Go duration grammar, including compound and fractional units; bounded by int64 ns.
fn parse_duration(input: &str) -> Result<Duration> {
    let mut s = input.strip_prefix('+').unwrap_or(input);
    if s == "0" {
        return Ok(Duration::ZERO);
    }
    ensure!(!s.is_empty(), "empty duration");
    let mut total = 0u128;
    while !s.is_empty() {
        let end = s
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(s.len());
        let number = &s[..end];
        ensure!(
            !number.is_empty() && number.chars().any(|c| c.is_ascii_digit()),
            "invalid duration number"
        );
        s = &s[end..];
        let (unit, factor) = [
            ("ns", 1u128),
            ("us", 1000),
            ("µs", 1000),
            ("μs", 1000),
            ("ms", 1_000_000),
            ("s", 1_000_000_000),
            ("m", 60_000_000_000),
            ("h", 3_600_000_000_000),
        ]
        .into_iter()
        .find(|(u, _)| s.starts_with(u))
        .context("invalid or missing duration unit")?;
        s = &s[unit.len()..];
        let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
        ensure!(!fraction.contains('.'), "invalid decimal duration");
        let whole: u128 = if whole.is_empty() { 0 } else { whole.parse()? };
        let mut ns = whole.checked_mul(factor).context("duration overflow")?;
        if !fraction.is_empty() {
            // Compute the fractional contribution right-to-left without float rounding.
            let mut fractional = 0u128;
            for digit in fraction.bytes().rev() {
                fractional = (u128::from(digit - b'0') * factor + fractional) / 10;
            }
            ns = ns.checked_add(fractional).context("duration overflow")?;
        }
        total = total.checked_add(ns).context("duration overflow")?;
        ensure!(
            total <= i64::MAX as u128,
            "duration exceeds Go int64 nanoseconds"
        );
    }
    Ok(Duration::from_nanos(total as u64))
}

pub(super) fn normalize(mut root: Value) -> Result<Value> {
    ensure!(
        root.is_mapping(),
        "Home configuration must be a YAML mapping"
    );
    validate_keys(&root)?;
    // Verified in Home internal/config/config_layout.go. No speculative aliases.
    for (flat, nested) in [
        ("host", "server.host"),
        ("port", "server.port"),
        ("request-retry", "routing.retry.request-retry"),
        (
            "max-retry-credentials",
            "routing.retry.max-retry-credentials",
        ),
        ("max-retry-interval", "routing.retry.max-retry-interval"),
        ("proxy-url", "requests.proxy-url"),
        ("streaming", "requests.streaming"),
        (
            "nonstream-keepalive-interval",
            "requests.nonstream-keepalive-interval",
        ),
        (
            "tls-handshake-timeout-seconds",
            "requests.tls-handshake-timeout-seconds",
        ),
        ("max-request-body-bytes", "server.max-request-body-bytes"),
        ("payload", "requests.payload"),
        ("credential-concurrency", "credentials.concurrency"),
        ("credential-in-flight", "credentials.in-flight"),
    ] {
        if let Some(v) = lookup(&root, nested)?.cloned() {
            let map = root.as_mapping_mut().expect("mapping checked");
            let key = Value::String(flat.into());
            if let Some(existing) = map.get_mut(&key) {
                merge(existing, v, flat)?;
            } else {
                map.insert(key, v);
            }
            remove(&mut root, &nested.split('.').collect::<Vec<_>>());
        }
    }
    Ok(root)
}

fn merge(old: &mut Value, new: Value, path: &str) -> Result<()> {
    if let (Some(a), Some(b)) = (old.as_mapping_mut(), new.as_mapping()) {
        for (k, v) in b {
            if let Some(existing) = a.get_mut(k) {
                merge(
                    existing,
                    v.clone(),
                    &format!("{path}.{}", k.as_str().unwrap_or("?")),
                )?;
            } else {
                a.insert(k.clone(), v.clone());
            }
        }
    } else {
        // Reject ambiguous mixed layouts rather than discarding an execution setting.
        ensure!(
            *old == new,
            "conflicting flattened/v8 configuration at {path}"
        );
    }
    Ok(())
}

fn lookup<'a>(root: &'a Value, path: &str) -> Result<Option<&'a Value>> {
    let mut v = root;
    for key in path.split('.') {
        ensure!(v.is_mapping(), "v8 container for {path} must be a mapping");
        match field(v, key) {
            Some(next) => v = next,
            None => return Ok(None),
        }
    }
    Ok(Some(v))
}

fn remove(root: &mut Value, path: &[&str]) {
    if let Some(map) = root.as_mapping_mut() {
        let key = Value::String(path[0].into());
        if path.len() == 1 {
            map.remove(&key);
        } else if let Some(child) = map.get_mut(&key) {
            remove(child, &path[1..]);
            if child.as_mapping().is_some_and(Mapping::is_empty) {
                map.remove(&key);
            }
        }
    }
}

fn validate_keys(v: &Value) -> Result<()> {
    match v {
        Value::Mapping(map) => {
            for (key, value) in map {
                let key = key
                    .as_str()
                    .context("configuration mapping keys must be strings")?;
                ensure!(
                    key != "<<",
                    "YAML merge keys are unsupported; use explicit configuration"
                );
                validate_keys(value)?;
            }
        }
        Value::Sequence(values) => {
            for value in values {
                validate_keys(value)?;
            }
        }
        Value::Tagged(_) => bail!("YAML tags are unsupported in Home configuration"),
        _ => {}
    }
    Ok(())
}
