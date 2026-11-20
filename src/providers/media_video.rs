//! Go xAI video model aliases and OpenAI video facade.
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub const DEFAULT_MODEL: &str = "grok-imagine-video";
pub fn model(requested: &str, openai: bool) -> Result<(String, String)> {
    let requested = requested.trim().to_ascii_lowercase();
    let requested = requested.split('(').next().unwrap_or("").trim();
    let (prefix, base) = requested.rsplit_once('/').unwrap_or(("", requested));
    if openai && (base == "sora-2" || base.starts_with("sora-2-")) {
        return Ok((DEFAULT_MODEL.into(), DEFAULT_MODEL.into()));
    }
    ensure!(
        matches!(prefix, "" | "xai" | "x-ai" | "grok"),
        "unsupported video prefix"
    );
    let base = if base.is_empty() { DEFAULT_MODEL } else { base };
    ensure!(
        matches!(
            base,
            "grok-imagine-video"
                | "grok-imagine-video-1.5"
                | "grok-imagine-video-1.5-lite"
                | "grok-imagine-video-1.5-preview"
        ),
        "unsupported video model"
    );
    Ok((
        base.into(),
        if base.ends_with("-preview") {
            "grok-imagine-video-1.5"
        } else {
            base
        }
        .into(),
    ))
}
fn text<'a>(body: &'a Value, key: &str) -> &'a str {
    body.get(key).and_then(Value::as_str).unwrap_or("").trim()
}
fn image(value: &Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| value.get("url")?.as_str())
        .or_else(|| value.get("image_url")?.as_str())
        .or_else(|| value.pointer("/image_url/url")?.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}
pub fn normalize_refs(body: &mut Value) {
    match body {
        Value::Object(object) => {
            if object.contains_key("image_url")
                && let Some(url) = image(&Value::Object(object.clone())).map(str::to_owned)
            {
                object.insert("url".into(), url.into());
                object.remove("image_url");
            } else if let Some(Value::String(url)) = object.get_mut("url") {
                *url = url.trim().to_owned();
            }
            for value in object.values_mut() {
                normalize_refs(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                normalize_refs(value);
            }
        }
        _ => {}
    }
}
pub fn create(original: &Value, openai: bool) -> Result<(String, Value)> {
    ensure!(original.is_object(), "expected object");
    ensure!(
        original.get("stream").is_none_or(|v| v == false),
        "video streaming unsupported"
    );
    let (routing, canonical) = model(text(original, "model"), openai)?;
    if !openai {
        let mut body = original.clone();
        body["model"] = canonical.into();
        normalize_refs(&mut body);
        return Ok((routing, body));
    }
    let prompt = text(original, "prompt");
    ensure!(!prompt.is_empty(), "prompt required");
    let seconds = match original.get("seconds") {
        None => 4,
        Some(Value::String(s)) => s.trim().parse::<i64>()?,
        Some(v) => v
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("invalid seconds"))?,
    }
    .clamp(1, 15);
    let size = match text(original, "size") {
        "" => "720x1280",
        size => size,
    };
    let mut aspect = match size {
        "720x1280" | "1024x1792" => "9:16",
        "1280x720" | "1792x1024" => "16:9",
        _ => anyhow::bail!("unsupported size"),
    };
    aspect = match text(original, "aspect_ratio") {
        "square" | "1:1" => "1:1",
        "landscape" | "16:9" => "16:9",
        "portrait" | "9:16" => "9:16",
        "4:3" => "4:3",
        "3:4" => "3:4",
        "3:2" => "3:2",
        "2:3" => "2:3",
        _ => aspect,
    };
    let resolution = match text(original, "resolution") {
        "480p" => "480p",
        "1080p" => "1080p",
        _ => "720p",
    };
    ensure!(
        original
            .pointer("/input_reference/file_id")
            .is_none_or(|v| v.as_str().is_none_or(str::is_empty)),
        "file references unsupported"
    );
    let input = original
        .pointer("/input_reference/image_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| original.get("image").and_then(image))
        .or_else(|| original.get("image_url").and_then(Value::as_str));
    let refs: Vec<_> = ["reference_images", "reference_image_urls"]
        .iter()
        .filter_map(|key| original.get(key)?.as_array())
        .flatten()
        .filter_map(image)
        .collect();
    ensure!(
        refs.len() <= 7 && (input.is_none() || refs.is_empty()),
        "invalid image references"
    );
    let mut body = json!({"model":canonical,"prompt":prompt,"duration":seconds,"aspect_ratio":aspect,"resolution":resolution});
    if let Some(input) = input {
        body["image"] = json!({"url":input});
    }
    if !refs.is_empty() {
        body["reference_images"] = refs.iter().map(|url| json!({"url":url})).collect();
    }
    Ok((routing, body))
}
/// Match Go ParseXAIVideoUsage: async creation alone is not billable video output.
pub fn completed_output(value: &Value) -> bool {
    ["usage", "video", "data", "url"]
        .iter()
        .any(|key| value.get(key).is_some())
        || text(value, "status").eq_ignore_ascii_case("completed")
}

pub fn duration_seconds(request: &Value, response: &Value) -> Option<f64> {
    if !completed_output(response) {
        return None;
    }
    // Go chooses the first existing response field, then falls back to the request
    // if it is not a positive number; it never coerces strings or estimates duration.
    let duration = response
        .get("duration")
        .or_else(|| response.pointer("/video/duration"));
    duration
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .or_else(|| {
            request
                .get("duration")?
                .as_f64()
                .filter(|n| n.is_finite() && *n > 0.0)
        })
}

pub fn id(value: &Value) -> Option<&str> {
    ["request_id", "id"].iter().find_map(|key| {
        value
            .get(key)?
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    })
}
pub fn facade(value: &Value, id: &str, model: &str, created: Option<&Value>) -> Value {
    let status = match text(value, "status").to_ascii_lowercase().as_str() {
        "queued" | "pending" => "queued",
        "in_progress" | "processing" | "running" => "in_progress",
        "completed" | "done" | "succeeded" | "success" => "completed",
        "failed" | "error" | "expired" | "cancelled" | "canceled" => "failed",
        _ => "",
    };
    let mut out = json!({"object":"video","id":id,"model":value.get("model").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(model)});
    if !status.is_empty() {
        out["status"] = status.into();
    }
    for key in [
        "created_at",
        "completed_at",
        "expires_at",
        "prompt",
        "remixed_from_video_id",
        "size",
        "progress",
        "seconds",
    ] {
        if let Some(v) = value.get(key) {
            out[key] = v.clone();
        }
    }
    if out.get("seconds").is_none()
        && let Some(duration) = value.pointer("/video/duration")
    {
        out["seconds"] = duration
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| duration.to_string())
            .into();
    }
    if let Some(url) = value.pointer("/video/url") {
        out["video_url"] = url.clone();
    }
    if let Some(request) = created {
        out["status"] = if status.is_empty() { "queued" } else { status }.into();
        if out.get("progress").is_none() {
            out["progress"] = 0.into();
        }
        out["prompt"] = request["prompt"].clone();
        out["seconds"] = request["duration"].to_string().into();
        out["created_at"] = chrono::Utc::now().timestamp().into();
        out["size"] = "720x1280".into();
    }
    if value.get("error").is_some() || value.get("code").is_some() {
        if out.get("status").is_none() {
            out["status"] = "failed".into();
        }
        if out.get("progress").is_none() {
            out["progress"] = 0.into();
        }
        // Never reflect arbitrary upstream error text or embedded credentials.
        out["error"] =
            json!({"code":"video_generation_failed","message":"Video generation failed"});
    }
    out
}
