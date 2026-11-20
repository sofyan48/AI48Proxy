//! xAI image request/response adaptation; compatible providers retain native JSON.
use super::multipart::Form;
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("").trim()
}
pub fn xai_request(body: &Value, model: &str, form: Option<&Form>, edit: bool) -> Result<Value> {
    let model = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    ensure!(
        matches!(
            model.as_str(),
            "grok-imagine-image" | "grok-imagine-image-quality" | "grok-imagine-image-2.0"
        ),
        "unsupported xAI image model"
    );
    let mut out =
        json!({"model":model,"prompt":text(body,"prompt"),"response_format":response_format(body)});
    let size = text(body, "size").to_ascii_lowercase();
    let aspect = text(body, "aspect_ratio").to_ascii_lowercase();
    let aspect = match aspect.as_str() {
        "square" => "1:1",
        "landscape" => "16:9",
        "portrait" => "9:16",
        "1:1" | "16:9" | "9:16" | "9:20" | "20:9" | "4:3" | "3:4" | "3:2" | "2:3" => {
            aspect.as_str()
        }
        _ => "",
    };
    let aspect = match size.as_str() {
        "1024x1024" | "2048x2048" | "1:1" => "1:1",
        "1792x1024" | "16:9" => "16:9",
        "1024x1792" | "9:16" => "9:16",
        "9:20" => "9:20",
        "20:9" => "20:9",
        "1536x1024" | "3:2" => "3:2",
        "1024x1536" | "2:3" => "2:3",
        _ => aspect,
    };
    if !aspect.is_empty() || !edit {
        out["aspect_ratio"] = if aspect.is_empty() { "1:1" } else { aspect }.into();
    }
    let resolution = text(body, "resolution").to_ascii_lowercase();
    let resolution = match resolution.as_str() {
        "1k" | "2k" => resolution.as_str(),
        _ if size.contains("2048") => "2k",
        _ if !edit => "1k",
        _ => "",
    };
    if !resolution.is_empty() {
        out["resolution"] = resolution.into();
    }
    if !text(body, "quality").is_empty() {
        out["quality"] = text(body, "quality").into();
    }
    if let Some(n) = body
        .get("n")
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
        .filter(|n| *n > 0)
    {
        out["n"] = n.into();
    }
    if edit {
        let images: Vec<Value> = if let Some(form) = form {
            form.images().iter().map(|p| json!({"type":"image_url","url":format!("data:{};base64,{}",p.content_type,STANDARD.encode(&p.bytes))})).collect()
        } else {
            let mut images = Vec::new();
            for key in ["image", "images"] {
                if let Some(value) = body.get(key) {
                    let values = value
                        .as_array()
                        .cloned()
                        .unwrap_or_else(|| vec![value.clone()]);
                    for value in values {
                        let url = value
                            .as_str()
                            .or_else(|| value.get("url").and_then(Value::as_str))
                            .or_else(|| {
                                value.get("image_url").and_then(|v| {
                                    v.as_str().or_else(|| v.get("url").and_then(Value::as_str))
                                })
                            });
                        if let Some(url) = url.filter(|s| !s.trim().is_empty()) {
                            images.push(json!({"type":"image_url","url":url.trim()}));
                        }
                    }
                }
            }
            images
        };
        ensure!(!images.is_empty(), "image is required");
        if images.len() == 1 {
            out["image"] = images[0].clone();
        } else {
            out["images"] = images.into();
        }
    }
    Ok(out)
}
/// xAI's Go handler emits one completed event per image, without a DONE sentinel.
pub fn completed_sse(
    value: &Value,
    prefix: &str,
    mut attempt: super::lifecycle::Attempt,
) -> axum::response::Response {
    use axum::{body::Body, response::IntoResponse};
    let event = format!("{prefix}.completed");
    let chunks: Vec<_> = value["data"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|image| {
            let mut data = json!({"type":event});
            for key in ["b64_json", "url"] {
                if let Some(value) = image.get(key) {
                    data[key] = value.clone();
                }
            }
            if let Some(usage) = value.get("usage") {
                data["usage"] = usage.clone();
            }
            format!("event: {event}\ndata: {data}\n\n")
        })
        .collect();
    attempt.record.failed = true;
    attempt.record.fail_code = "request_scoped".into();
    let body = futures_util::stream::unfold(
        (chunks.into_iter(), Some(attempt)),
        |(mut chunks, mut attempt)| async move {
            let owner = attempt.as_mut()?;
            if owner.cancellation().is_cancelled() {
                owner.record.local_failure(503);
                return None;
            }
            if let Some(chunk) = chunks.next() {
                Some((Ok::<_, std::io::Error>(chunk), (chunks, attempt)))
            } else {
                owner.record.failed = false;
                owner.record.fail_code.clear();
                None
            }
        },
    );
    (
        [
            ("content-type", "text/event-stream"),
            ("cache-control", "no-cache"),
        ],
        Body::from_stream(body),
    )
        .into_response()
}

pub fn response_format(body: &Value) -> &'static str {
    if text(body, "response_format").eq_ignore_ascii_case("url") {
        "url"
    } else {
        "b64_json"
    }
}
pub fn response(value: Value, format: &str) -> Result<Value> {
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("missing image output"))?;
    let mut items = Vec::new();
    for item in data {
        let b64 = text(item, "b64_json");
        let url = text(item, "url");
        if b64.is_empty() && url.is_empty() {
            continue;
        }
        let mut out = json!({});
        if format == "url" {
            out["url"] = if !url.is_empty() {
                url.to_owned()
            } else {
                let mime = match text(item, "mime_type") {
                    "" => match text(item, "output_format") {
                        "jpeg" | "jpg" => "image/jpeg",
                        "webp" => "image/webp",
                        _ => "image/png",
                    },
                    mime => mime,
                };
                format!("data:{mime};base64,{b64}")
            }
            .into();
        } else if !b64.is_empty() {
            out["b64_json"] = b64.into();
        } else {
            out["url"] = url.into();
        }
        if !text(item, "revised_prompt").is_empty() {
            out["revised_prompt"] = text(item, "revised_prompt").into();
        }
        items.push(out);
    }
    ensure!(!items.is_empty(), "missing image output");
    let created = value
        .get("created")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    let mut out = json!({"created":created,"data":items});
    for key in [
        "size",
        "quality",
        "background",
        "output_format",
        "resolution",
        "aspect_ratio",
    ] {
        if let Some(value) = value.get(key).filter(|v| v.is_string()) {
            out[key] = value.clone();
        }
    }
    if let Some(usage) = value.get("usage").filter(|v| v.is_object()) {
        out["usage"] = usage.clone();
    }
    Ok(out)
}
