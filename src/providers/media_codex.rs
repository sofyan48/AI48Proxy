//! Codex Images direct routes and the Responses image-tool bridge.
use super::super::{images, multipart::Form};
use crate::{config::Config, home::client::Selection, providers};
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use http::HeaderMap;
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn base(model: &str) -> String {
    model
        .split('(')
        .next()
        .unwrap_or(model)
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .trim()
        .to_ascii_lowercase()
}
pub fn direct(original: &Value, selected: &str) -> bool {
    [
        original.get("model").and_then(Value::as_str).unwrap_or(""),
        selected,
    ]
    .iter()
    .any(|model| {
        matches!(
            base(model).as_str(),
            "gpt-image-1.5"
                | "gpt-image-2"
                | "gpt-image-2.5"
                | "gpt-image-2.5-flare"
                | "gpt-image-2.5-sunburst"
                | "gpt-image-2.6"
                | "gpt-image-2.6-flare"
                | "gpt-image-2.6-sunburst"
        )
    })
}
fn data_url(part: &super::super::multipart::Part) -> String {
    format!(
        "data:{};base64,{}",
        part.content_type,
        STANDARD.encode(&part.bytes)
    )
}
pub fn payload(
    config: &Config,
    selection: &Selection,
    original: &Value,
    form: Option<&Form>,
    edit: bool,
) -> Result<Value> {
    let mut body = original.clone();
    body["model"] = selection
        .model
        .split('(')
        .next()
        .unwrap_or(&selection.model)
        .trim()
        .into();
    body.as_object_mut().unwrap().remove("stream");
    if let Some(form) = form {
        body["images"] = form
            .images()
            .iter()
            .map(|part| json!({"image_url":data_url(part)}))
            .collect();
        if let Some(mask) = form
            .parts
            .iter()
            .find(|p| p.name == "mask" && p.filename.is_some())
        {
            body["mask"] = json!({"image_url":data_url(mask)});
        }
        for key in ["n", "output_compression", "partial_images"] {
            if let Some(number) = body
                .get(key)
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<i64>().ok())
            {
                body[key] = number.into();
            }
        }
    }
    if direct(original, &selection.model) {
        return Ok(body);
    }
    let mut tool = json!({"type":"image_generation","action":if edit {"edit"} else {"generate"},
        "model":original.get("model").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).unwrap_or(&selection.model)});
    for key in [
        "size",
        "quality",
        "background",
        "output_format",
        "moderation",
        "output_compression",
        "partial_images",
    ] {
        if let Some(value) = body.get(key) {
            tool[key] = value.clone();
        }
    }
    if edit {
        if let Some(value) = body.get("input_fidelity") {
            tool["input_fidelity"] = value.clone();
        }
        if let Some(mask) = body.pointer("/mask/image_url") {
            tool["input_image_mask"] = json!({"image_url":mask});
        }
    }
    let mut content = vec![
        json!({"type":"input_text","text":body.get("prompt").and_then(Value::as_str).unwrap_or("").trim()}),
    ];
    if edit {
        for image in body
            .get("images")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(url) = image
                .get("image_url")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                content.push(json!({"type":"input_image","image_url":url.trim()}));
            }
        }
    }
    Ok(
        json!({"model":config.gpt_image_2_base_model,"instructions":"","stream":true,"store":false,
        "reasoning":{"effort":"medium","summary":"auto"},"parallel_tool_calls":true,
        "include":["reasoning.encrypted_content"],"tool_choice":{"type":"image_generation"},"tools":[tool],
        "input":[{"type":"message","role":"user","content":content}]}),
    )
}

pub fn headers(
    config: &Config,
    selection: &Selection,
    incoming: &HeaderMap,
    body: &Value,
    direct: bool,
) -> Result<providers::Credentials> {
    let mut creds = providers::resolve(selection, "codex")?;
    // Direct Images calls deliberately do not inherit a browser/client User-Agent.
    let mut incoming = incoming.clone();
    if direct {
        incoming.remove("user-agent");
    }
    providers::codex::request_headers(
        &mut creds.headers,
        &incoming,
        config,
        &selection.auth,
        creds.oauth,
    )?;
    providers::codex::prompt_cache_header(&mut creds.headers, body)?;
    Ok(creds)
}

/// Completion is mandatory. Empty final output uses output_item.done in output-index order.
pub fn response(bytes: &[u8], format: &str) -> Result<Value> {
    let text = std::str::from_utf8(bytes)?;
    let mut indexed = BTreeMap::new();
    let mut fallback = Vec::new();
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("error" | "response.failed" | "response.incomplete") => {
                anyhow::bail!("Codex image response failed")
            }
            Some("response.output_item.done") => {
                if let Some(item) = event.get("item") {
                    if let Some(index) = event.get("output_index").and_then(Value::as_i64) {
                        indexed.insert(index, item.clone());
                    } else {
                        fallback.push(item.clone());
                    }
                }
            }
            Some("response.completed") => {
                let response = &event["response"];
                let output = response
                    .get("output")
                    .and_then(Value::as_array)
                    .filter(|items| !items.is_empty())
                    .cloned()
                    .unwrap_or_else(|| {
                        indexed
                            .values()
                            .cloned()
                            .chain(fallback.iter().cloned())
                            .collect()
                    });
                let mut data = Vec::new();
                let mut metadata = json!({});
                for item in output {
                    if item["type"] != "image_generation_call" {
                        continue;
                    }
                    let Some(result) = item
                        .get("result")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                    else {
                        continue;
                    };
                    if data.is_empty() {
                        for key in ["size", "quality", "background", "output_format"] {
                            if let Some(value) = item.get(key) {
                                metadata[key] = value.clone();
                            }
                        }
                    }
                    let mut image = json!({"b64_json":result.trim()});
                    for key in ["revised_prompt", "output_format"] {
                        if let Some(value) = item.get(key) {
                            image[key] = value.clone();
                        }
                    }
                    data.push(image);
                }
                ensure!(!data.is_empty(), "missing image output");
                let mut raw = json!({"created":response["created_at"],"data":data});
                if let Some(usage) = response.pointer("/tool_usage/image_gen") {
                    raw["usage"] = usage.clone();
                }
                let mut result = images::response(raw, format)?;
                for (key, value) in metadata.as_object().unwrap() {
                    result[key] = value.clone();
                }
                return Ok(result);
            }
            _ => {}
        }
    }
    anyhow::bail!("Codex image stream ended before completion")
}
