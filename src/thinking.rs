//! Pure thinking normalization, not wired into the same-format gateway yet.
//!
//! Mirrors Go's `internal/thinking`: suffix > body, canonical config, dispatch
//! capability validation, provider encoding. Unlike Go's permissive fallbacks,
//! unknown capabilities, unsupported modes and explicit out-of-range budgets fail.
//! No registry lookup, model-name heuristics or user-defined-model bypass is used.
//! Call before the final payload-rule barrier if integrated in the future.
use serde_json::{Map, Value, json};
use std::{error::Error, fmt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAiChat,
    Claude,
    Gemini,
}

impl TryFrom<&str> for Provider {
    type Error = ThinkingError;
    fn try_from(value: &str) -> Result<Self> {
        match value {
            "openai" => Ok(Self::OpenAiChat),
            "claude" => Ok(Self::Claude),
            "gemini" => Ok(Self::Gemini),
            _ => Err(ThinkingError::Unsupported(format!("provider {value:?}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    // Go's documented canonical conversion, not a claim of token/effort parity.
    fn budget(self) -> u64 {
        match self {
            Self::Minimal => 512,
            Self::Low => 1024,
            Self::Medium => 8192,
            Self::High => 24576,
            Self::XHigh => 32768,
            Self::Max => 128000,
        }
    }

    fn from_budget(budget: u64) -> Self {
        match budget {
            0..=512 => Self::Minimal,
            513..=1024 => Self::Low,
            1025..=8192 => Self::Medium,
            8193..=24576 => Self::High,
            _ => Self::XHigh,
        }
    }
}

/// An enum prevents contradictory mode/budget/level combinations. Budget(0)
/// is invalid; use Disabled. Auto is distinct from an absent configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingConfig {
    Disabled,
    Auto,
    Budget(u64),
    Level(Level),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThinkingError {
    Invalid(String),
    Unsupported(String),
}
impl fmt::Display for ThinkingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(s) => write!(f, "invalid thinking configuration: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported thinking configuration: {s}"),
        }
    }
}
impl Error for ThinkingError {}
type Result<T> = std::result::Result<T, ThinkingError>;
fn invalid(s: &str) -> ThinkingError {
    ThinkingError::Invalid(s.into())
}
fn unsupported(s: &str) -> ThinkingError {
    ThinkingError::Unsupported(s.into())
}

/// Parses only a terminal `(value)` suffix. Unknown or empty terminal suffixes
/// are errors, never ignored in favor of a body configuration.
pub fn parse_suffix(model: &str) -> Result<(&str, Option<ThinkingConfig>)> {
    if model.is_empty() {
        return Err(invalid("empty model"));
    }
    if !model.ends_with(')') {
        return Ok((model, None));
    }
    let Some(open) = model.rfind('(') else {
        return Ok((model, None));
    };
    if open == 0 {
        return Err(invalid("empty base model"));
    }
    let raw = &model[open + 1..model.len() - 1];
    let config = match raw.parse::<i64>() {
        Ok(n) => numeric(n)?,
        Err(_) => named(raw)?,
    };
    Ok((&model[..open], Some(config)))
}

fn named(raw: &str) -> Result<ThinkingConfig> {
    Ok(match raw.to_ascii_lowercase().as_str() {
        "none" => ThinkingConfig::Disabled,
        "auto" => ThinkingConfig::Auto,
        "minimal" => ThinkingConfig::Level(Level::Minimal),
        "low" => ThinkingConfig::Level(Level::Low),
        "medium" => ThinkingConfig::Level(Level::Medium),
        "high" => ThinkingConfig::Level(Level::High),
        "xhigh" => ThinkingConfig::Level(Level::XHigh),
        "max" => ThinkingConfig::Level(Level::Max),
        _ => return Err(invalid("unknown thinking level or suffix")),
    })
}
fn numeric(n: i64) -> Result<ThinkingConfig> {
    match n {
        -1 => Ok(ThinkingConfig::Auto),
        0 => Ok(ThinkingConfig::Disabled),
        1.. => Ok(ThinkingConfig::Budget(n as u64)),
        _ => Err(invalid("budget must be -1, zero, or a positive integer")),
    }
}
fn object(value: &Value) -> Result<&Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| invalid("expected JSON object"))
}
fn keys(value: &Value, allowed: &[&str]) -> Result<()> {
    for key in object(value)?.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(ThinkingError::Unsupported(format!(
                "thinking field {key:?}"
            )));
        }
    }
    Ok(())
}

/// Extracts native body configuration. Only OpenAI Chat, Claude explicit budget
/// thinking and Gemini camelCase thinkingConfig are supported. Adaptive Claude,
/// Responses reasoning and snake_case Gemini variants are explicitly unsupported.
pub fn extract(provider: Provider, body: &Value) -> Result<Option<ThinkingConfig>> {
    object(body)?;
    let foreign: &[&str] = match provider {
        Provider::OpenAiChat => &[
            "thinking",
            "generationConfig",
            "generation_config",
            "reasoning",
            "output_config",
        ],
        Provider::Claude => &[
            "reasoning_effort",
            "reasoning",
            "generationConfig",
            "generation_config",
            "output_config",
        ],
        Provider::Gemini => &[
            "thinking",
            "reasoning_effort",
            "reasoning",
            "generation_config",
            "output_config",
        ],
    };
    if foreign.iter().any(|key| body.get(*key).is_some()) {
        return Err(unsupported("foreign or unimplemented reasoning fields"));
    }
    match provider {
        Provider::OpenAiChat => body
            .get("reasoning_effort")
            .map(|v| {
                named(
                    v.as_str()
                        .ok_or_else(|| invalid("reasoning_effort must be a string"))?,
                )
            })
            .transpose(),
        Provider::Claude => {
            let Some(t) = body.get("thinking") else {
                return Ok(None);
            };
            keys(t, &["type", "budget_tokens"])?;
            match t["type"].as_str() {
                Some("disabled") if t.get("budget_tokens").is_none() => {
                    Ok(Some(ThinkingConfig::Disabled))
                }
                Some("enabled") => {
                    let n = t["budget_tokens"].as_i64().ok_or_else(|| {
                        invalid("enabled Claude thinking requires integer budget_tokens")
                    })?;
                    if n <= 0 {
                        return Err(invalid("Claude budget_tokens must be positive"));
                    }
                    Ok(Some(numeric(n)?))
                }
                Some("adaptive") => Err(unsupported("Claude adaptive thinking")),
                _ => Err(invalid("invalid Claude thinking.type or budget")),
            }
        }
        Provider::Gemini => {
            let Some(g) = body.get("generationConfig") else {
                return Ok(None);
            };
            object(g)?;
            if g.get("thinking_config").is_some() {
                return Err(unsupported("snake_case thinking_config"));
            }
            let Some(t) = g.get("thinkingConfig") else {
                return Ok(None);
            };
            keys(t, &["thinkingBudget", "thinkingLevel", "includeThoughts"])?;
            if let Some(v) = t.get("includeThoughts")
                && !v.is_boolean()
            {
                return Err(invalid("includeThoughts must be boolean"));
            }
            match (t.get("thinkingBudget"), t.get("thinkingLevel")) {
                (Some(_), Some(_)) => Err(invalid(
                    "both Gemini thinkingBudget and thinkingLevel supplied",
                )),
                (Some(v), None) => {
                    Ok(Some(numeric(v.as_i64().ok_or_else(|| {
                        invalid("thinkingBudget must be integer")
                    })?)?))
                }
                (None, Some(v)) => {
                    Ok(Some(named(v.as_str().ok_or_else(|| {
                        invalid("thinkingLevel must be string")
                    })?)?))
                }
                (None, None) => Err(unsupported("Gemini summary-only thinkingConfig")),
            }
        }
    }
}

/// Exact capability subset from Home dispatch `selection.model_info`. Other
/// metadata is ignored. Missing thinking metadata does not imply support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingSupport {
    pub min: u64,
    pub max: u64,
    pub zero_allowed: bool,
    pub dynamic_allowed: bool,
    pub levels: Vec<String>,
}
impl ThinkingSupport {
    pub fn from_model_info(model_info: Option<&Value>) -> Result<Self> {
        let info = model_info.ok_or_else(|| unsupported("dispatch model_info missing"))?;
        object(info)?;
        let t = info
            .get("thinking")
            .filter(|v| !v.is_null())
            .ok_or_else(|| unsupported("dispatch model has no thinking support"))?;
        object(t)?;
        // Custom disable encodings cannot safely use Claude's standard encoder.
        if t.get("disable_type")
            .is_some_and(|v| v != "disabled" && v != "")
        {
            return Err(unsupported("custom thinking disable_type"));
        }
        let number = |key: &str| -> Result<u64> {
            t.get(key).map_or(Ok(0), |v| {
                v.as_u64()
                    .ok_or_else(|| invalid("invalid capability budget"))
            })
        };
        let boolean = |key: &str| -> Result<bool> {
            t.get(key).map_or(Ok(false), |v| {
                v.as_bool()
                    .ok_or_else(|| invalid("invalid capability flag"))
            })
        };
        let levels = match t.get("levels") {
            None => Vec::new(),
            Some(v) => v
                .as_array()
                .ok_or_else(|| invalid("invalid capability levels"))?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(|s| s.trim().to_ascii_lowercase())
                        .ok_or_else(|| invalid("invalid capability level"))
                })
                .collect::<Result<Vec<_>>>()?,
        };
        let support = Self {
            min: number("min")?,
            max: number("max")?,
            zero_allowed: boolean("zero_allowed")?,
            dynamic_allowed: boolean("dynamic_allowed")?,
            levels,
        };
        if support.min > support.max {
            return Err(invalid("capability min exceeds max"));
        }
        Ok(support)
    }
    fn has_level(&self, level: &str) -> bool {
        self.levels.iter().any(|s| s.eq_ignore_ascii_case(level))
    }
}

/// Normalizes Go's budget/level conversions only when the target model needs
/// the other representation. Derived budgets and suffix budgets are clamped;
/// body budgets are strict. Explicit unsupported levels/modes are never replaced
/// by a lower level or a made-up auto budget. No adaptive Claude parity is claimed.
pub fn validate(
    config: ThinkingConfig,
    provider: Provider,
    support: &ThinkingSupport,
    from_suffix: bool,
) -> Result<ThinkingConfig> {
    use ThinkingConfig::*;
    if support.min > support.max {
        return Err(invalid("capability min exceeds max"));
    }
    let budget_capable = support.max > 0;
    let mut derived = false;
    let config = match config {
        Budget(0) => return Err(invalid("use Disabled instead of Budget(0)")),
        Level(level) if support.levels.is_empty() && budget_capable => {
            derived = true;
            Budget(level.budget())
        }
        Budget(n) if !budget_capable && !support.levels.is_empty() => {
            Level(self::Level::from_budget(n))
        }
        other => other,
    };
    let config = match config {
        Disabled if !support.zero_allowed && !support.has_level("none") => {
            return Err(unsupported("model cannot disable thinking"));
        }
        Auto if !support.dynamic_allowed => {
            return Err(unsupported("model cannot use dynamic thinking"));
        }
        Level(level) if !support.has_level(level.as_str()) => {
            return Err(unsupported("level absent from dispatch capabilities"));
        }
        Budget(_) if !budget_capable => return Err(unsupported("model has no budget range")),
        Budget(n) if from_suffix || derived => Budget(n.clamp(support.min.max(1), support.max)),
        Budget(n) if n < support.min || n > support.max => {
            return Err(invalid("budget outside dispatch model range"));
        }
        other => other,
    };
    match (provider, config) {
        (Provider::OpenAiChat, Budget(_) | Auto) => Err(unsupported(
            "OpenAI Chat supports explicit effort levels, not budgets or auto",
        )),
        (Provider::Claude, Level(_) | Auto) => Err(unsupported(
            "Claude adaptive/automatic thinking not implemented; use a supported budget",
        )),
        (Provider::Claude, Disabled) if !support.zero_allowed => {
            Err(unsupported("Claude explicit disabling not allowed"))
        }
        (Provider::Gemini, Disabled) if !support.zero_allowed => {
            Err(unsupported("Gemini zero budget not allowed"))
        }
        _ => Ok(config),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppliedThinking {
    /// Base model for dispatch resolution; body.model is changed only if present.
    pub model: String,
    pub config: Option<ThinkingConfig>,
    pub body: Value,
}

/// Pure same-format entry point. `model` is the original client model (including
/// suffix), `model_info` is the selected dispatch model metadata, not the whole
/// selection. The caller must resolve aliases/dispatch using `parse_suffix` first.
/// Returns a copy; errors leave inputs unchanged. A valid suffix overrides even
/// malformed native thinking in the body, but foreign reasoning is not discarded.
pub fn apply(
    model: &str,
    provider: Provider,
    body: &Value,
    model_info: Option<&Value>,
) -> Result<AppliedThinking> {
    object(body)?;
    let (base, suffix) = parse_suffix(model)?;
    let mut clean = body.clone();
    if suffix.is_some() {
        match provider {
            Provider::OpenAiChat => {
                clean.as_object_mut().unwrap().remove("reasoning_effort");
            }
            Provider::Claude => {
                clean.as_object_mut().unwrap().remove("thinking");
            }
            Provider::Gemini => {
                if let Some(g) = clean.get_mut("generationConfig") {
                    let g = g
                        .as_object_mut()
                        .ok_or_else(|| invalid("generationConfig must be object"))?;
                    g.remove("thinkingConfig");
                }
            }
        }
    }
    let native = extract(provider, &clean)?;
    let Some(config) = suffix.or(native) else {
        return Ok(AppliedThinking {
            model: base.into(),
            config: None,
            body: body.clone(),
        });
    };
    let support = ThinkingSupport::from_model_info(model_info)?;
    let config = validate(config, provider, &support, suffix.is_some())?;
    let mut out = body.clone();
    let obj = out.as_object_mut().unwrap();
    match (provider, config) {
        (Provider::OpenAiChat, ThinkingConfig::Level(level)) => {
            obj.insert("reasoning_effort".into(), json!(level.as_str()));
        }
        (Provider::OpenAiChat, ThinkingConfig::Disabled) => {
            obj.insert("reasoning_effort".into(), json!("none"));
        }
        (Provider::Claude, ThinkingConfig::Disabled) => {
            obj.insert("thinking".into(), json!({"type":"disabled"}));
        }
        (Provider::Claude, ThinkingConfig::Budget(n)) => {
            let max = match obj.get("max_tokens") {
                Some(v) => v
                    .as_u64()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| invalid("max_tokens must be positive integer"))?,
                None => model_info
                    .and_then(|i| i.get("max_completion_tokens"))
                    .and_then(Value::as_u64)
                    .filter(|n| *n > 0)
                    .ok_or_else(|| {
                        invalid(
                            "Claude thinking requires max_tokens or dispatch max_completion_tokens",
                        )
                    })?,
            };
            if n < 1024 || n >= max {
                return Err(invalid(
                    "Claude budget must be at least 1024 and less than max_tokens",
                ));
            }
            obj.insert("max_tokens".into(), json!(max));
            obj.insert(
                "thinking".into(),
                json!({"type":"enabled", "budget_tokens":n}),
            );
        }
        (Provider::Gemini, config) => {
            let g = obj.entry("generationConfig").or_insert_with(|| json!({}));
            let g = g
                .as_object_mut()
                .ok_or_else(|| invalid("generationConfig must be object"))?;
            let include = g
                .get("thinkingConfig")
                .and_then(|t| t.get("includeThoughts"))
                .cloned();
            let mut t = match config {
                ThinkingConfig::Level(level) => json!({"thinkingLevel":level.as_str()}),
                ThinkingConfig::Budget(n) => json!({"thinkingBudget":n}),
                ThinkingConfig::Disabled => json!({"thinkingBudget":0}),
                ThinkingConfig::Auto => json!({"thinkingBudget":-1}),
            };
            if let Some(include) = include {
                if !include.is_boolean() {
                    return Err(invalid("includeThoughts must be boolean"));
                }
                t["includeThoughts"] = include;
            }
            g.insert("thinkingConfig".into(), t);
        }
        _ => {
            return Err(unsupported(
                "provider encoder cannot represent canonical config",
            ));
        }
    }
    if obj.contains_key("model") {
        obj.insert("model".into(), json!(base));
    }
    Ok(AppliedThinking {
        model: base.into(),
        config: Some(config),
        body: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn budget_info() -> Value {
        json!({"max_completion_tokens":32768,"thinking":{"min":1024,"max":24576,"zero_allowed":true,"dynamic_allowed":true}})
    }
    fn level_info() -> Value {
        json!({"thinking":{"levels":["low","medium","high"],"zero_allowed":true}})
    }

    #[test]
    fn suffixes_and_invalid_inputs() {
        for (suffix, expected) in [
            ("0", ThinkingConfig::Disabled),
            ("-1", ThinkingConfig::Auto),
            ("AUTO", ThinkingConfig::Auto),
            ("HIGH", ThinkingConfig::Level(Level::High)),
            ("08192", ThinkingConfig::Budget(8192)),
        ] {
            assert_eq!(
                parse_suffix(&format!("model({suffix})")).unwrap(),
                ("model", Some(expected))
            );
        }
        for model in [
            "m()",
            "m(ultra)",
            "m(-2)",
            "m(999999999999999999999)",
            "(high)",
            "",
        ] {
            assert!(parse_suffix(model).is_err(), "{model}");
        }
        assert_eq!(
            parse_suffix("m(high)-alias").unwrap(),
            ("m(high)-alias", None)
        );
    }
    #[test]
    fn precedence_and_purity() {
        let input = json!({"model":"m(high)","reasoning_effort":{},"messages":[]});
        let out = apply("m(high)", Provider::OpenAiChat, &input, Some(&level_info())).unwrap();
        assert_eq!(out.body["reasoning_effort"], "high");
        assert_eq!(out.body["model"], "m");
        assert_eq!(input["reasoning_effort"], json!({}));
        assert!(
            apply(
                "m(ultra)",
                Provider::OpenAiChat,
                &json!({"reasoning_effort":"low"}),
                Some(&level_info())
            )
            .is_err()
        );
        assert_eq!(
            apply("m", Provider::OpenAiChat, &out.body, Some(&level_info()))
                .unwrap()
                .body,
            out.body
        );
    }
    #[test]
    fn metadata_and_strict_rejections() {
        for info in [
            None,
            Some(json!({})),
            Some(json!({"thinking":null})),
            Some(json!({"thinking":{"min":9,"max":1}})),
        ] {
            assert!(apply("m(high)", Provider::OpenAiChat, &json!({}), info.as_ref()).is_err());
        }
        assert!(
            apply(
                "m(xhigh)",
                Provider::OpenAiChat,
                &json!({}),
                Some(&level_info())
            )
            .is_err()
        );
        assert!(
            apply(
                "m(auto)",
                Provider::OpenAiChat,
                &json!({}),
                Some(&level_info())
            )
            .is_err()
        );
        assert!(
            apply(
                "m(none)",
                Provider::Gemini,
                &json!({}),
                Some(&json!({"thinking":{"min":1,"max":99}}))
            )
            .is_err()
        );
        assert!(
            apply(
                "m",
                Provider::Claude,
                &json!({"thinking":{"type":"adaptive"}}),
                Some(&level_info())
            )
            .is_err()
        );
        assert!(Provider::try_from("codex").is_err());
        assert!(Provider::try_from("openai-response").is_err());
        assert!(extract(Provider::OpenAiChat, &json!({"reasoning_effort":9})).is_err());
        assert!(extract(Provider::Gemini, &json!({"generationConfig":{"thinkingConfig":{"thinkingBudget":1,"thinkingLevel":"high"}}})).is_err());
        assert_eq!(
            apply("m", Provider::Gemini, &json!({}), None)
                .unwrap()
                .config,
            None
        );
    }
    #[test]
    fn native_encodings_ranges_and_conversions() {
        let info = budget_info();
        let c = apply("m(high)", Provider::Claude, &json!({}), Some(&info)).unwrap();
        assert_eq!(
            c.body["thinking"],
            json!({"type":"enabled","budget_tokens":24576})
        );
        assert_eq!(c.body["max_tokens"], 32768);
        assert!(
            apply(
                "m",
                Provider::Claude,
                &json!({"thinking":{"type":"enabled","budget_tokens":99999}}),
                Some(&info)
            )
            .is_err()
        );
        assert!(
            apply(
                "m(8192)",
                Provider::Claude,
                &json!({"max_tokens":8192}),
                Some(&info)
            )
            .is_err()
        );
        assert_eq!(
            apply("m(99999)", Provider::Gemini, &json!({}), Some(&info))
                .unwrap()
                .body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            24576
        );
        assert_eq!(
            apply(
                "m(8192)",
                Provider::OpenAiChat,
                &json!({}),
                Some(&level_info())
            )
            .unwrap()
            .config,
            Some(ThinkingConfig::Level(Level::Medium))
        );
        let g = json!({"generationConfig":{"temperature":0.7,"thinkingConfig":{"thinkingBudget":8192,"includeThoughts":false}}});
        let applied = apply("m", Provider::Gemini, &g, Some(&info)).unwrap();
        assert_eq!(applied.body, g);
        assert_eq!(
            apply("m(auto)", Provider::Gemini, &g, Some(&info))
                .unwrap()
                .body["generationConfig"]["thinkingConfig"],
            json!({"thinkingBudget":-1,"includeThoughts":false})
        );
        assert_eq!(
            apply("m(none)", Provider::Claude, &json!({}), Some(&info))
                .unwrap()
                .body["thinking"],
            json!({"type":"disabled"})
        );
        assert_eq!(
            apply("m(high)", Provider::Gemini, &json!({}), Some(&level_info()))
                .unwrap()
                .body["generationConfig"]["thinkingConfig"],
            json!({"thinkingLevel":"high"})
        );
    }
}
