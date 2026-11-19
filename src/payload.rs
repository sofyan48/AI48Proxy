//! Final semantic payload barrier. No business-payload mutations should follow `apply`.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

#[path = "payload/matching.rs"]
mod matching;
#[path = "payload/replace.rs"]
mod replace;
pub use matching::ApplyContext;
use matching::{Model, matches, validate as validate_models};

#[cfg(test)]
#[path = "payload/golden.rs"]
mod golden;
#[path = "payload/path.rs"]
mod path;
#[path = "payload/query.rs"]
mod query;
#[cfg(test)]
#[path = "payload/tests.rs"]
mod tests;

#[derive(Clone, Debug, Default)]
pub struct PayloadRules {
    stages: Vec<Vec<Rule>>,
    filters: Vec<Filter>,
    replacements: Vec<replace::Replace>,
}

#[derive(Clone, Debug)]
struct Rule {
    models: Vec<Model>,
    params: Vec<(path::Path, Value)>,
}

#[derive(Clone, Debug)]
struct Filter {
    models: Vec<Model>,
    params: Vec<path::Path>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct WireRules {
    default: Vec<WireRule>,
    default_raw: Vec<WireRule>,
    r#override: Vec<WireRule>,
    override_raw: Vec<WireRule>,
    filter: Vec<WireFilter>,
    replace: Vec<replace::WireReplace>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRule {
    models: Vec<Model>,
    params: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFilter {
    models: Vec<Model>,
    params: Vec<String>,
}

impl PayloadRules {
    /// Compile Go payload stages and request gates. Unsupported syntax fails closed.
    pub fn parse(value: &serde_yaml::Value) -> Result<Self> {
        let wire: WireRules = serde_yaml::from_value(value.clone())
            .context("unsupported or invalid payload rules")?;
        let mut stages = Vec::new();
        for (index, rules) in [
            wire.default,
            wire.default_raw,
            wire.r#override,
            wire.override_raw,
        ]
        .into_iter()
        .enumerate()
        {
            let mut stage = Vec::new();
            for rule in rules {
                validate_models(&rule.models)?;
                let mut params = Vec::new();
                for (key, mut value) in rule.params {
                    if index % 2 == 1
                        && let Value::String(fragment) = value
                    {
                        value = serde_json::from_str(&fragment).with_context(|| {
                            format!("invalid raw JSON in payload parameter {key}")
                        })?;
                    }
                    params.push((path::Path::parse(&key)?, value));
                }
                stage.push(Rule {
                    models: rule.models,
                    params,
                });
            }
            stages.push(stage);
        }
        let mut filters = Vec::new();
        for rule in wire.filter {
            validate_models(&rule.models)?;
            filters.push(Filter {
                models: rule.models,
                params: rule
                    .params
                    .iter()
                    .map(|key| path::Path::parse(key))
                    .collect::<Result<_>>()?,
            });
        }
        let replacements = wire
            .replace
            .into_iter()
            .map(|r| r.compile())
            .collect::<Result<_>>()?;
        Ok(Self {
            stages,
            filters,
            replacements,
        })
    }

    /// Apply default, default-raw, override, override-raw, replace, then filter. Failure is
    /// atomic: the caller's body remains unchanged if a path cannot be written.
    pub fn apply(&self, body: &mut Value, model: &str, protocol: &str) -> Result<()> {
        self.apply_context(
            body,
            ApplyContext {
                model,
                protocol,
                ..Default::default()
            },
        )
    }

    /// Evaluate gates against each stage's current body. Callers must provide
    /// original request headers/source protocol before translating or stripping
    /// them; `apply` deliberately leaves these unavailable rather than guessing.
    pub fn apply_context(&self, body: &mut Value, context: ApplyContext<'_>) -> Result<()> {
        if !context.root.trim().is_empty() {
            let root = path::Path::parse(context.root.trim())?;
            let mut next = body.clone();
            for resolved in root.resolve(body) {
                if let Some(mut value) = path::get(&next, &resolved).cloned() {
                    self.apply_context(
                        &mut value,
                        ApplyContext {
                            root: "",
                            original: context.original.and_then(|v| path::get(v, &resolved)),
                            ..context
                        },
                    )?;
                    path::set(&mut next, &resolved, value)?;
                }
            }
            *body = next;
            return Ok(());
        }
        if self.stages.iter().all(Vec::is_empty)
            && self.filters.is_empty()
            && self.replacements.is_empty()
        {
            return Ok(());
        }
        ensure!(body.is_object(), "payload rules require a JSON object body");
        if context.model.trim().is_empty() && context.requested_model.trim().is_empty() {
            return Ok(());
        }
        let original = context.original.unwrap_or(body).clone();
        let mut next = body.clone();
        let mut defaults = HashSet::new();
        for (index, rules) in self.stages.iter().enumerate() {
            for rule in rules {
                if !matches(&rule.models, context, &next) {
                    continue;
                }
                for (path, value) in &rule.params {
                    for resolved in path.resolve(&next) {
                        if index < 2
                            && (path::get(&original, &resolved).is_some()
                                || !defaults.insert(resolved.clone()))
                        {
                            continue;
                        }
                        path::set(&mut next, &resolved, value.clone())?;
                    }
                }
            }
        }
        for rule in &self.replacements {
            rule.apply(&mut next, context)?;
        }
        for rule in &self.filters {
            if !matches(&rule.models, context, &next) {
                continue;
            }
            for path in &rule.params {
                // Reverse traversal preserves indexes when deleting array elements.
                for resolved in path.resolve(&next).into_iter().rev() {
                    path::delete(&mut next, &resolved);
                }
            }
        }
        *body = next;
        Ok(())
    }
}
