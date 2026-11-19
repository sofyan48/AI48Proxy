use super::{
    Value,
    matching::{self, ApplyContext, Model},
    path,
};
use anyhow::{Result, bail, ensure};
use regex::Regex;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WireReplace {
    #[serde(default)]
    models: Vec<Model>,
    target: Target,
    replacements: Vec<BTreeMap<String, String>>,
}
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Target {
    role: String,
    positions: Vec<usize>,
    all: bool,
    paths: Option<Vec<String>>,
}
#[derive(Clone, Debug)]
enum Step {
    Literal(String, String),
    Regex(Regex, String),
}
#[derive(Clone, Debug)]
pub(super) struct Replace {
    models: Vec<Model>,
    target: Target,
    paths: Vec<path::Path>,
    steps: Vec<Step>,
}
impl WireReplace {
    pub(super) fn compile(self) -> Result<Replace> {
        matching::validate(&self.models)?;
        let t = &self.target;
        let mut paths = Vec::new();
        if let Some(keys) = &t.paths {
            ensure!(!keys.is_empty(), "target.paths must not be empty");
            ensure!(
                t.role.is_empty() && !t.all && t.positions.is_empty(),
                "target.paths and role selectors are mutually exclusive"
            );
            for key in keys {
                paths.push(path::Path::parse(key.trim())?);
            }
        } else {
            ensure!(
                matches!(t.role.as_str(), "system" | "developer" | "user"),
                "invalid target.role"
            );
            ensure!(
                if t.all {
                    t.positions.is_empty()
                } else {
                    !t.positions.is_empty()
                },
                "target.all and target.positions are mutually exclusive and require an explicit selector"
            );
            ensure!(
                t.positions.iter().all(|p| *p > 0),
                "positions must be positive one-based integers"
            );
        }
        ensure!(
            !self.replacements.is_empty(),
            "replacements must not be empty"
        );
        let mut steps = Vec::new();
        for entry in self.replacements {
            if entry.len() == 1 {
                let (from, to) = entry.into_iter().next().expect("one entry");
                ensure!(!from.is_empty(), "replacement source must not be empty");
                steps.push(Step::Literal(from, to));
            } else if entry.len() == 2
                && entry.contains_key("pattern")
                && entry.contains_key("replacement")
            {
                let pattern = &entry["pattern"];
                // Go skips invalid regex entries individually, not the entire rule.
                if !pattern.is_empty() {
                    match Regex::new(pattern) {
                        Ok(re) => steps.push(Step::Regex(re, entry["replacement"].clone())),
                        Err(_) => tracing::warn!("payload replacement skipped: invalid regex"),
                    }
                }
            } else {
                bail!("replacement must be one literal mapping or pattern and replacement");
            }
        }
        Ok(Replace {
            models: self.models,
            target: self.target,
            paths,
            steps,
        })
    }
}

impl Replace {
    pub(super) fn apply(&self, body: &mut Value, ctx: ApplyContext<'_>) -> Result<()> {
        if !matching::matches(&self.models, ctx, body) {
            return Ok(());
        }
        let mut selected = Vec::new();
        if self.target.paths.is_some() {
            for p in &self.paths {
                selected.extend(p.resolve_replace(body));
            }
        } else {
            let protocol = ctx.protocol.trim().to_ascii_lowercase();
            let responses = matches!(
                protocol.as_str(),
                "codex"
                    | "responses"
                    | "response"
                    | "openai-response"
                    | "openai-responses"
                    | "meta"
            );
            let gemini = matches!(protocol.as_str(), "gemini" | "antigravity");
            let (array, system) = match protocol.as_str() {
                "openai" => ("messages", None),
                "claude" => ("messages", Some("system")),
                _ if responses => ("input", Some("instructions")),
                _ if gemini => (
                    "contents",
                    Some(if body.get("systemInstruction").is_some() {
                        "systemInstruction"
                    } else {
                        "system_instruction"
                    }),
                ),
                _ => return Ok(()),
            };
            let mut position = 0;
            let chosen = |p| self.target.all || self.target.positions.contains(&p);
            if self.target.role == "system"
                && let Some(key) = system
                && let Some(v) = body.get(key)
            {
                position += 1;
                if chosen(position) {
                    content(v, vec![key.into()], gemini, responses, &mut selected);
                }
            }
            if let Some(Value::String(_)) = body.get(array) {
                if responses && self.target.role == "user" && chosen(1) {
                    selected.push(vec![array.into()]);
                }
            } else if let Some(items) = body.get(array).and_then(Value::as_array) {
                for (i, item) in items.iter().enumerate() {
                    if item.get("role").and_then(Value::as_str) != Some(&self.target.role) {
                        continue;
                    }
                    if responses
                        && item
                            .get("type")
                            .is_some_and(|t| t.as_str() != Some("message"))
                    {
                        continue;
                    }
                    position += 1;
                    if chosen(position) {
                        if gemini {
                            content(
                                item,
                                vec![array.into(), i.to_string()],
                                true,
                                responses,
                                &mut selected,
                            );
                        } else if let Some(v) = item.get("content") {
                            content(
                                v,
                                vec![array.into(), i.to_string(), "content".into()],
                                false,
                                responses,
                                &mut selected,
                            );
                        }
                    }
                }
            }
        }
        let mut seen = HashSet::new();
        let mut changes = Vec::new();
        for p in selected {
            if !seen.insert(p.clone()) {
                continue;
            }
            if let Some(text) = path::get(body, &p).and_then(Value::as_str) {
                let mut next = text.to_owned();
                for step in &self.steps {
                    next = match step {
                        Step::Literal(from, to) => next.replace(from, to),
                        Step::Regex(re, to) => re.replace_all(&next, to.as_str()).into_owned(),
                    };
                }
                if next != text {
                    changes.push((p, Value::String(next)));
                }
            }
        }
        for (p, v) in changes {
            path::set(body, &p, v)?;
        }
        Ok(())
    }
}

fn content(
    value: &Value,
    mut prefix: Vec<String>,
    gemini: bool,
    responses: bool,
    out: &mut Vec<Vec<String>>,
) {
    if !gemini && value.is_string() {
        out.push(prefix);
        return;
    }
    let blocks = if gemini {
        prefix.push("parts".into());
        value.get("parts")
    } else {
        Some(value)
    };
    if let Some(blocks) = blocks.and_then(Value::as_array) {
        for (i, b) in blocks.iter().enumerate() {
            if b.get("thought").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let editable = if gemini {
                b.get("functionCall").is_none() && b.get("functionResponse").is_none()
            } else {
                b.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t == "text" || responses && t == "input_text")
            };
            if editable && b.get("text").is_some_and(Value::is_string) {
                let mut p = prefix.clone();
                p.extend([i.to_string(), "text".into()]);
                out.push(p);
            }
        }
    }
}
