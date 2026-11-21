//! spawn_agent "Available model overrides" list for Codex multi-agent v2.
//! Mirrors CPA internal/client/codex/optimize-multi-agent-v2 (Home mode):
//! codexHomeAvailableModels, decodeCodexHomeAvailableModels,
//! codexSpawnAgentModelsFromTemplates, formatCodexSpawnAgentModels and
//! replaceCodexSpawnAgentModels.
//!
//! `codex_models_catalog.json` is a byte-identical copy of CPA
//! internal/registry/models/codex_client_models.json (CLIProxyAPI commit
//! a5e8f69e56309e1ef6d6872e667da81fdfbce2ce, sha256
//! 5088dd5ed900cf7a420ba4651af7f747130d6380a7fe23365fd1b02f0ac2ca8c).
//! Refresh it by copying that file; never hand-edit it.
use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::sync::OnceLock;

use serde_json::{Map, Value};

#[cfg(test)]
const CATALOG_JSON: &str = include_str!("codex_models_catalog.json");
const DESCRIPTION_MARKER: &str = "Spawns an agent";
pub(crate) const MODELS_HEADING: &str =
    "Available model overrides (optional; inherited parent model is preferred):";
const DEFAULT_TEMPLATE_SLUG: &str = "gpt-5.5";

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SpawnAgentModel {
    pub id: String,
    pub description: String,
    pub reasoning_efforts: Vec<String>,
    pub default_reasoning_effort: String,
    pub service_tiers: Vec<String>,
    pub priority: i64,
    pub display_name: String,
}

/// One Home-provided model (CPA decodeCodexHomeAvailableModels entry).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct AvailableModel {
    pub id: String,
    pub display_name: String,
    pub description: String,
}

/// Registry metadata used for models without a catalog template
/// (the subset of CPA registry.ModelInfo the rewrite reads).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ModelInfo {
    pub description: String,
    pub thinking_levels: Vec<String>,
}

pub(crate) struct Templates {
    by_slug: HashMap<String, SpawnAgentModel>,
    default: SpawnAgentModel,
}

fn map_string(values: &Map<String, Value>, key: &str) -> String {
    values
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned()
}

fn map_int(values: &Map<String, Value>, key: &str) -> i64 {
    match values.get(key) {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        _ => 0,
    }
}

/// CPA codexClientModelsCatalog parsing plus template/default selection.
pub(crate) fn parse_templates(catalog: &str) -> Option<Templates> {
    let value: Value = serde_json::from_str(catalog).ok()?;
    let models = value.get("models")?.as_array()?;
    // Go unmarshals into []map[string]any: null entries are allowed, scalars are not.
    if models
        .iter()
        .any(|model| !model.is_null() && !model.is_object())
    {
        return None;
    }
    let mut by_slug = HashMap::with_capacity(models.len());
    let mut default = None;
    for model in models.iter().filter_map(Value::as_object) {
        let slug = map_string(model, "slug");
        if slug.is_empty() {
            continue;
        }
        let profile = model_from_metadata(&slug, model);
        if default.is_none() || slug == DEFAULT_TEMPLATE_SLUG {
            default = Some(profile.clone());
        }
        by_slug.insert(slug, profile);
    }
    Some(Templates {
        by_slug,
        default: default?,
    })
}

/// The embedded Codex client catalog, parsed once.
#[cfg(test)]
pub(crate) fn embedded_templates() -> Option<&'static Templates> {
    static TEMPLATES: OnceLock<Option<Templates>> = OnceLock::new();
    TEMPLATES
        .get_or_init(|| parse_templates(CATALOG_JSON))
        .as_ref()
}

fn model_from_metadata(id: &str, metadata: &Map<String, Value>) -> SpawnAgentModel {
    let (reasoning_efforts, default_reasoning_effort) = reasoning_metadata(metadata);
    SpawnAgentModel {
        id: id.to_owned(),
        description: map_string(metadata, "description"),
        display_name: map_string(metadata, "display_name"),
        priority: map_int(metadata, "priority"),
        reasoning_efforts,
        default_reasoning_effort,
        service_tiers: service_tier_ids(metadata),
    }
}

fn normalize_effort(effort: &str) -> String {
    let effort = effort.trim().to_lowercase();
    match effort.as_str() {
        "none" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra" => effort,
        _ => String::new(),
    }
}

fn reasoning_metadata(metadata: &Map<String, Value>) -> (Vec<String>, String) {
    let levels = metadata
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let efforts: Vec<String> = levels
        .iter()
        .map(|level| {
            level
                .as_object()
                .map(|level| normalize_effort(&map_string(level, "effort")))
                .unwrap_or_default()
        })
        .filter(|effort| !effort.is_empty())
        .collect();
    if efforts.is_empty() {
        return (Vec::new(), String::new());
    }
    let mut default = normalize_effort(&map_string(metadata, "default_reasoning_level"));
    if !efforts.contains(&default) {
        default = efforts[0].clone();
    }
    (efforts, default)
}

fn service_tier_ids(metadata: &Map<String, Value>) -> Vec<String> {
    let mut seen = HashSet::new();
    metadata
        .get("service_tiers")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|tier| tier.as_object().map(|tier| map_string(tier, "id")))
        .filter(|id| !id.is_empty() && seen.insert(id.clone()))
        .collect()
}

fn apply_thinking(profile: &mut SpawnAgentModel, levels: &[String]) {
    let mut efforts = Vec::with_capacity(levels.len());
    let mut default = String::new();
    let mut first = String::new();
    for raw in levels {
        let effort = normalize_effort(raw);
        if effort.is_empty() {
            continue;
        }
        if first.is_empty() {
            first = effort.clone();
        }
        if (default.is_empty() && effort != "none") || effort == "medium" {
            default = effort.clone();
        }
        efforts.push(effort);
    }
    if efforts.is_empty() {
        return;
    }
    if default.is_empty() {
        default = first;
    }
    profile.reasoning_efforts = efforts;
    profile.default_reasoning_effort = default;
}

fn home_model_id(model: &Map<String, Value>) -> String {
    let id = map_string(model, "id");
    if !id.is_empty() {
        return id;
    }
    let name = map_string(model, "name");
    name.strip_prefix("models/").unwrap_or(&name).to_owned()
}

/// Home's models payload is `{bucket: [model, ...]}`; anything else (including
/// the `{"error":...}` envelope) yields no models, like Go's typed unmarshal.
fn home_sections(raw: &Value) -> Option<Vec<&Map<String, Value>>> {
    let sections = raw.as_object().filter(|s| !s.is_empty())?;
    let mut models = Vec::new();
    for entries in sections.values() {
        match entries {
            Value::Null => {}
            Value::Array(entries) => {
                for entry in entries {
                    match entry {
                        Value::Null => {}
                        Value::Object(model) => models.push(model),
                        _ => return None,
                    }
                }
            }
            _ => return None,
        }
    }
    Some(models)
}

/// CPA decodeCodexHomeAvailableModels.
pub(crate) fn decode_home_available_models(raw: &Value) -> Vec<AvailableModel> {
    let Some(sections) = home_sections(raw) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for model in sections {
        let id = home_model_id(model);
        if id.is_empty() || !seen.insert(id.clone()) {
            continue;
        }
        let mut display_name = map_string(model, "display_name");
        if display_name.is_empty() {
            display_name = map_string(model, "displayName");
        }
        models.push(AvailableModel {
            id,
            description: display_name.clone(),
            display_name,
        });
    }
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models
}

/// Registry metadata carried by Home's model entries (first entry per ID wins).
/// CPA reads the same fields from its local registry via registry.LookupModelInfo.
pub(crate) fn home_model_infos(raw: &Value) -> HashMap<String, ModelInfo> {
    let mut infos = HashMap::new();
    for model in home_sections(raw).unwrap_or_default() {
        let id = home_model_id(model);
        if id.is_empty() || infos.contains_key(&id) {
            continue;
        }
        let thinking_levels = model
            .get("thinking")
            .and_then(|t| t.get("levels"))
            .and_then(Value::as_array)
            .map(|levels| {
                levels
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        infos.insert(
            id,
            ModelInfo {
                description: map_string(model, "description"),
                thinking_levels,
            },
        );
    }
    infos
}

/// CPA codexSpawnAgentModelsFromTemplates.
pub(crate) fn models_from_templates(
    available: &[AvailableModel],
    templates: &Templates,
    lookup: &dyn Fn(&str) -> Option<ModelInfo>,
) -> Vec<SpawnAgentModel> {
    let mut seen = HashSet::new();
    let mut template_models = Vec::new();
    let mut synthesized = Vec::new();
    for model in available {
        let id = model.id.trim();
        if id.is_empty() || !seen.insert(id.to_owned()) {
            continue;
        }
        if let Some(template) = templates.by_slug.get(id) {
            template_models.push(template.clone());
            continue;
        }
        let mut profile = templates.default.clone();
        profile.id = id.to_owned();
        profile.description = model.description.trim().to_owned();
        profile.display_name = model.display_name.trim().to_owned();
        if profile.display_name.is_empty() {
            profile.display_name = id.to_owned();
        }
        if let Some(info) = lookup(id) {
            if !info.description.trim().is_empty() {
                profile.description = info.description.trim().to_owned();
            }
            apply_thinking(&mut profile, &info.thinking_levels);
        }
        if profile.description.is_empty() {
            profile.description = id.to_owned();
        }
        profile.service_tiers.clear();
        synthesized.push(profile);
    }
    template_models.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.id.cmp(&b.id)));
    synthesized.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    template_models.append(&mut synthesized);
    template_models
}

/// CPA codexSpawnAgentModelsFromSources (catalog JSON + available models).
#[cfg(test)]
pub(crate) fn models_from_sources(
    available: &[AvailableModel],
    catalog: &str,
    lookup: &dyn Fn(&str) -> Option<ModelInfo>,
) -> Vec<SpawnAgentModel> {
    parse_templates(catalog)
        .map(|templates| models_from_templates(available, &templates, lookup))
        .unwrap_or_default()
}

/// Offline helper retained for deterministic embedded-catalog tests.
#[cfg(test)]
pub(crate) fn models_from_home(raw: &Value) -> Vec<SpawnAgentModel> {
    embedded_templates()
        .map(|templates| models_from_home_templates(raw, templates))
        .unwrap_or_default()
}

pub(crate) fn models_from_home_templates(
    raw: &Value,
    templates: &Templates,
) -> Vec<SpawnAgentModel> {
    let available = decode_home_available_models(raw);
    let infos = home_model_infos(raw);
    models_from_templates(&available, templates, &|id| infos.get(id).cloned())
}

fn fields(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn markdown_code(value: &str) -> String {
    if value.contains('`') {
        format!("`` {value} ``")
    } else {
        format!("`{value}`")
    }
}

/// CPA formatCodexSpawnAgentModels.
pub(crate) fn format_models(models: &[SpawnAgentModel]) -> String {
    let mut out = String::new();
    for model in models {
        let id = fields(&model.id);
        if id.is_empty() {
            continue;
        }
        out.push_str("- ");
        out.push_str(&markdown_code(&id));
        out.push_str(": ");
        let mut has_details = false;
        let description = fields(&model.description);
        if !description.is_empty() {
            out.push_str(&description);
            if !description.ends_with(['.', '!', '?']) {
                out.push('.');
            }
            has_details = true;
        }
        if !model.reasoning_efforts.is_empty() {
            if has_details {
                out.push(' ');
            }
            out.push_str("Reasoning efforts: ");
            for (index, effort) in model.reasoning_efforts.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                out.push_str(effort);
                if *effort == model.default_reasoning_effort {
                    out.push_str(" (default)");
                }
            }
            out.push('.');
            has_details = true;
        }
        if !model.service_tiers.is_empty() {
            if has_details {
                out.push(' ');
            }
            out.push_str("Service tiers: ");
            out.push_str(&model.service_tiers.join(", "));
            out.push('.');
        }
        out.push('\n');
    }
    out.strip_suffix('\n').map(str::to_owned).unwrap_or(out)
}

fn remove_model_sections(description: &str) -> (String, String) {
    if !description.contains(MODELS_HEADING) {
        return (description.to_owned(), String::new());
    }
    let lines: Vec<&str> = description.split_inclusive('\n').collect();
    let mut cleaned = String::with_capacity(description.len());
    let mut indent = String::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.trim() != MODELS_HEADING {
            cleaned.push_str(line);
            index += 1;
            continue;
        }
        if indent.is_empty()
            && let Some(at) = line.find(MODELS_HEADING).filter(|at| *at > 0)
        {
            indent = line[..at].to_owned();
        }
        index += 1;
        while index < lines.len() && lines[index].trim().starts_with("- ") {
            index += 1;
        }
    }
    (cleaned, indent)
}

/// CPA replaceCodexSpawnAgentModels.
pub(crate) fn replace_models(description: &str, model_list: &str) -> String {
    if model_list.is_empty() {
        return description.to_owned();
    }
    let (cleaned, indent) = remove_model_sections(description);
    let section = format!("{indent}{MODELS_HEADING}\n{model_list}\n");
    if let Some(marker) = cleaned.find(DESCRIPTION_MARKER) {
        let line_start = cleaned[..marker].rfind('\n').map_or(0, |i| i + 1);
        return format!(
            "{}{section}{}",
            &cleaned[..line_start],
            &cleaned[line_start..]
        );
    }
    let separator = if !cleaned.is_empty() && !cleaned.ends_with('\n') {
        "\n\n"
    } else {
        ""
    };
    format!(
        "{cleaned}{separator}{}",
        section.strip_suffix('\n').unwrap_or(&section)
    )
}
