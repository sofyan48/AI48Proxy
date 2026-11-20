//! Execution policies shared with gateway owners; no request bodies are retained.
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RequestScopedRule {
    #[serde(default)]
    pub status: i64,
    #[serde(default, rename = "match")]
    pub substrings: Vec<String>,
    #[serde(default, rename = "match-regexr")]
    pub regexes: Vec<String>,
    #[serde(default)]
    pub action: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorAction {
    Stop,
    Continue,
    StopAndCooldown,
    ContinueAndCooldown,
}

impl ErrorAction {
    pub fn stops(self) -> bool {
        matches!(self, Self::Stop | Self::StopAndCooldown)
    }

    pub fn fail_code(self) -> &'static str {
        match self {
            Self::Stop | Self::Continue => "request_scoped",
            Self::StopAndCooldown | Self::ContinueAndCooldown => "force_cooldown",
        }
    }
}

impl RequestScopedRule {
    /// Go matches the original error body case-sensitively, not normalized JSON leaves.
    pub fn matches(&self, status: u16, body: &str) -> Option<ErrorAction> {
        if self.status <= 0 || self.status != i64::from(status) {
            return None;
        }
        let matched = self
            .substrings
            .iter()
            .any(|s| !s.is_empty() && body.contains(s))
            || self
                .regexes
                .iter()
                .any(|s| !s.is_empty() && regex::Regex::new(s).is_ok_and(|r| r.is_match(body)));
        if !matched {
            return None;
        }
        match self.action.trim().to_ascii_lowercase().as_str() {
            "stop" => Some(ErrorAction::Stop),
            "continue" => Some(ErrorAction::Continue),
            "stop-and-cooldown" => Some(ErrorAction::StopAndCooldown),
            "continue-and-cooldown" => Some(ErrorAction::ContinueAndCooldown),
            _ => None,
        }
    }
}

pub type OAuthErrorRules = BTreeMap<String, Vec<RequestScopedRule>>;

pub(super) fn oauth_rules(value: Option<&serde_yaml::Value>) -> Result<OAuthErrorRules> {
    value.map_or_else(
        || Ok(BTreeMap::new()),
        |v| serde_yaml::from_value(v.clone()).context("invalid oauth-request-scoped-errors"),
    )
}

/// Credential metadata takes precedence, including API-key credentials. Global
/// provider rules apply only to OAuth credentials, as in CPA's rule resolver.
pub fn resolve_rules(
    config: &super::Config,
    provider: &str,
    auth: &serde_json::Value,
) -> Vec<RequestScopedRule> {
    if let Some(metadata) = auth.get("metadata") {
        let raw = metadata
            .get("request_scoped_errors")
            .or_else(|| metadata.get("request-scoped-errors"));
        if let Some(rules) = raw
            .and_then(|v| serde_json::from_value::<Vec<RequestScopedRule>>(v.clone()).ok())
            .filter(|rules| !rules.is_empty())
        {
            return rules;
        }
    }
    let kind = auth
        .pointer("/attributes/auth_kind")
        .and_then(serde_json::Value::as_str);
    let api_key = auth
        .pointer("/attributes/api_key")
        .and_then(serde_json::Value::as_str);
    let oauth = match kind {
        Some("oauth") => true,
        Some("api_key" | "apikey") => false,
        _ => api_key.is_none_or(|key| key.trim().is_empty()),
    };
    if oauth {
        config
            .oauth_request_scoped_errors
            .get(&provider.trim().to_ascii_lowercase())
            .cloned()
            .unwrap_or_default()
    } else {
        Vec::new()
    }
}
