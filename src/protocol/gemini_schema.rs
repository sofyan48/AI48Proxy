//! Focused Go schema-cleaning subset. Unsupported composition fails closed rather
//! than dropping branches. Only call this on declaration schemas, never history.
use anyhow::{Result, ensure};
use serde_json::{Value, json};

fn display(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}
fn hint(schema: &mut serde_json::Map<String, Value>, value: String) {
    let existing = schema
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    if existing == value
        || existing.starts_with(&format!("{value} ("))
        || existing.contains(&format!("({value})"))
    {
        return;
    }
    schema.insert(
        "description".into(),
        json!(if existing.is_empty() {
            value
        } else {
            format!("{existing} ({value})")
        }),
    );
}

/// Gemini parametersJsonSchema keeps constraints; Antigravity tools turn ignored
/// constraints/enums into advisory descriptions, without changing argument types.
pub(crate) fn clean(schema: &Value, antigravity: bool, placeholder: bool) -> Result<Value> {
    clean_node(schema, antigravity, placeholder, 0)
}
fn clean_node(schema: &Value, antigravity: bool, placeholder: bool, depth: usize) -> Result<Value> {
    ensure!(depth < 64, "tool schema nesting too deep");
    let mut out = schema
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("expected object tool schema"))?;
    // The complete Go resolver/conditional merger is deliberately not approximated.
    for key in [
        "$ref",
        "$dynamicRef",
        "allOf",
        "if",
        "then",
        "else",
        "dependencies",
        "dependentSchemas",
        "dependentRequired",
        "patternProperties",
        "propertyNames",
        "not",
    ] {
        ensure!(
            !out.contains_key(key),
            "unsupported tool schema composition: {key}"
        );
    }
    if let Some(value) = out.get("description") {
        ensure!(value.is_string(), "invalid schema description");
    }
    if let Some(value) = out.remove("const") {
        out.entry("enum").or_insert(json!([value]));
    }
    if let Some(value) = out.remove("enum") {
        let values = value
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("invalid schema enum"))?;
        ensure!(
            values.iter().all(|v| !v.is_object() && !v.is_array()),
            "unsupported compound enum"
        );
        let strings: Vec<String> = values.iter().map(display).collect();
        if !strings.is_empty() && strings.len() <= 10 {
            hint(&mut out, format!("Allowed: {}", strings.join(", ")));
        }
        if !antigravity {
            out.insert("enum".into(), json!(strings));
            out.insert("type".into(), json!("string"));
        }
    }
    if let Some(types) = out.get("type").and_then(Value::as_array).cloned() {
        let non_null: Vec<_> = types.iter().filter(|v| **v != "null").cloned().collect();
        ensure!(
            non_null.len() == 1 && types.len() <= 2,
            "unsupported schema type union"
        );
        out.insert("type".into(), non_null[0].clone());
        if antigravity && types.len() == 2 {
            out.insert("nullable".into(), json!(true));
        }
    }
    if let Some(kind) = out.get("type") {
        ensure!(
            kind.as_str().is_some_and(|v| [
                "object", "array", "string", "number", "integer", "boolean", "null"
            ]
            .contains(&v)),
            "unsupported schema type"
        );
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(branches) = out.get(key).cloned() {
            let branches = branches
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("invalid schema union"))?;
            ensure!(
                !branches.is_empty() && !placeholder,
                "tool schema union requires unimplemented model-specific normalization"
            );
            let cleaned = branches
                .iter()
                .map(|v| clean_node(v, antigravity, placeholder, depth + 1))
                .collect::<Result<Vec<_>>>()?;
            out.insert(key.into(), json!(cleaned));
        }
    }
    if let Some(properties) = out.get_mut("properties") {
        let properties = properties
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("invalid schema properties"))?;
        for value in properties.values_mut() {
            *value = clean_node(value, antigravity, placeholder, depth + 1)?;
        }
        if !out.contains_key("type") {
            out.insert("type".into(), json!("object"));
        }
    }
    if let Some(items) = out.get("items").cloned() {
        if !out.contains_key("type") {
            out.insert("type".into(), json!("array"));
        }
        if out["type"] == "array" {
            out.insert(
                "items".into(),
                clean_node(&items, antigravity, placeholder, depth + 1)?,
            );
        } else {
            out.remove("items");
        }
    } else if out.get("type").is_some_and(|v| v == "array") {
        out.insert("items".into(), json!({"type":"string"}));
    }
    if let Some(required) = out.get("required").cloned() {
        let required = required
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("invalid schema required"))?;
        ensure!(
            required.iter().all(Value::is_string),
            "invalid required property name"
        );
        let properties = out.get("properties").and_then(Value::as_object);
        let mut names = Vec::new();
        for value in required {
            if properties.is_some_and(|p| p.contains_key(value.as_str().unwrap()))
                && !names.contains(value)
            {
                names.push(value.clone());
            }
        }
        if names.is_empty() {
            out.remove("required");
        } else {
            out.insert("required".into(), json!(names));
        }
    }
    if antigravity {
        for key in [
            "minLength",
            "maxLength",
            "exclusiveMinimum",
            "exclusiveMaximum",
            "pattern",
            "minItems",
            "maxItems",
            "uniqueItems",
            "contains",
            "format",
            "default",
            "examples",
            "minimum",
            "maximum",
            "multipleOf",
        ] {
            if let Some(value) = out.remove(key) {
                hint(&mut out, format!("{key}: {}", display(&value)));
            }
        }
        if out.remove("additionalProperties") == Some(json!(false)) {
            hint(&mut out, "No extra properties allowed".into());
        }
    } else if let Some(value) = out.get_mut("additionalProperties") {
        if value.is_object() {
            *value = clean_node(value, false, false, depth + 1)?;
        } else {
            ensure!(value.is_boolean(), "invalid additionalProperties");
        }
    }
    for key in [
        "$schema",
        "$defs",
        "definitions",
        "$id",
        "id",
        "$anchor",
        "$vocabulary",
        "$dynamicAnchor",
        "$comment",
        "enumDescriptions",
        "enumTitles",
        "prefill",
        "deprecated",
        "encrypted",
        "discriminator",
        "readOnly",
        "writeOnly",
        "additionalItems",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contentSchema",
        "contentMediaType",
        "contentEncoding",
    ] {
        out.remove(key);
    }
    out.retain(|key, _| !key.starts_with("x-"));
    if !antigravity || !placeholder {
        out.remove("title");
    }
    if !antigravity {
        out.remove("nullable");
    }
    if out.get("type").is_some_and(|v| v == "object")
        && !out.contains_key("anyOf")
        && !out.contains_key("oneOf")
    {
        out.entry("properties").or_insert(json!({}));
        if placeholder {
            let empty = out["properties"].as_object().is_some_and(|v| v.is_empty());
            if empty {
                out.insert("properties".into(), json!({"reason":{"type":"string","description":"Brief explanation of why you are calling this tool"}}));
                out.insert("required".into(), json!(["reason"]));
            } else if !out.contains_key("required") {
                // Go cleans each declaration inside a wrapper: every object is nested.
                out["properties"]
                    .as_object_mut()
                    .unwrap()
                    .entry("_")
                    .or_insert(json!({"type":"boolean"}));
                out.insert("required".into(), json!(["_"]));
            }
        }
    }
    Ok(Value::Object(out))
}
