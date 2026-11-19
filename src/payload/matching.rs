use super::{Value, path};
use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;

/// Request metadata for the final upstream semantic barrier. Empty metadata never
/// satisfies a configured source-protocol or header gate.
#[derive(Clone, Copy, Debug, Default)]
pub struct ApplyContext<'a> {
    pub model: &'a str,
    pub protocol: &'a str,
    pub from_protocol: &'a str,
    pub headers: Option<&'a http::HeaderMap>,
    pub requested_model: &'a str,
    pub root: &'a str,
    pub original: Option<&'a Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
#[derive(Default)]
pub(super) struct Model {
    pub name: String,
    pub protocol: String,
    pub from_protocol: String,
    pub headers: BTreeMap<String, String>,
    pub r#match: Vec<BTreeMap<String, Value>>,
    pub not_match: Vec<BTreeMap<String, Value>>,
    pub exist: Vec<String>,
    pub not_exist: Vec<String>,
}

pub(super) fn validate(models: &[Model]) -> Result<()> {
    for model in models {
        for key in model
            .r#match
            .iter()
            .chain(&model.not_match)
            .flat_map(|m| m.keys())
            .chain(model.exist.iter())
            .chain(model.not_exist.iter())
        {
            if !key.trim().is_empty() {
                path::Path::parse(key.trim())?;
            }
        }
    }
    Ok(())
}

// Go model/header patterns support only '*'; '?' and backslashes are literals.
fn model_glob(pattern: &str, text: &str) -> bool {
    let escaped = pattern.trim().replace('\\', "\\\\").replace('?', "\\?");
    !pattern.trim().is_empty() && path::glob(&escaped, text.trim())
}

fn source(s: &str) -> String {
    match s.trim().to_ascii_lowercase().as_str() {
        "openai-response" | "openai-responses" | "response" => "responses".into(),
        other => other.into(),
    }
}

// Go normalizes all JSON numbers through float64 before DeepEqual.
pub(super) fn equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len() && a.iter().all(|(k, a)| b.get(k).is_some_and(|b| equal(a, b)))
        }
        _ => a == b,
    }
}

fn values<'a>(body: &'a Value, key: &str) -> Vec<&'a Value> {
    path::Path::parse(key.trim())
        .map(|p| {
            p.resolve(body)
                .iter()
                .filter_map(|p| path::get(body, p))
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn matches(models: &[Model], ctx: ApplyContext<'_>, body: &Value) -> bool {
    let requested = ctx.requested_model.trim();
    let base = requested
        .rsplit_once('(')
        .filter(|_| requested.ends_with(')'))
        .map(|(b, _)| b)
        .unwrap_or(requested);
    models.iter().any(|m| {
        let protocol = m.protocol.trim();
        !m.name.trim().is_empty()
            && [ctx.model.trim(), base, requested]
                .iter()
                .any(|s| !s.is_empty() && model_glob(m.name.trim(), s))
            && (protocol.is_empty()
                || ctx.protocol.is_empty()
                || protocol.eq_ignore_ascii_case(ctx.protocol))
            && (source(&m.from_protocol).is_empty()
                || (!source(ctx.from_protocol).is_empty()
                    && source(&m.from_protocol) == source(ctx.from_protocol)))
            && m.headers.iter().all(|(k, p)| {
                k.trim().is_empty()
                    || ctx.headers.is_some_and(|h| {
                        h.iter().any(|(name, v)| {
                            name.as_str().eq_ignore_ascii_case(k.trim())
                                && v.to_str().is_ok_and(|s| model_glob(p, s))
                        })
                    })
            })
            && m.r#match
                .iter()
                .flat_map(|m| m.iter())
                .all(|(k, v)| k.trim().is_empty() || values(body, k).iter().any(|a| equal(a, v)))
            && m.not_match
                .iter()
                .flat_map(|m| m.iter())
                .all(|(k, v)| k.trim().is_empty() || !values(body, k).iter().any(|a| equal(a, v)))
            && m.exist
                .iter()
                .all(|k| k.trim().is_empty() || values(body, k).iter().any(|v| !v.is_null()))
            && m.not_exist
                .iter()
                .all(|k| k.trim().is_empty() || !values(body, k).iter().any(|v| !v.is_null()))
    })
}
