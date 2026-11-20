//! Go openai_speech_handlers.go routing and native xAI TTS request normalization.
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub fn prepare(body: &Value) -> Result<(String, Value, &'static str)> {
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let mut base = requested.as_str();
    for prefix in ["xai/", "x-ai/", "grok/"] {
        if let Some(value) = base.strip_prefix(prefix) {
            base = value.trim();
            break;
        }
    }
    if base.ends_with(')')
        && let Some((name, _)) = base.rsplit_once('(')
    {
        base = name.trim();
    }
    let model = match base {
        "" | "tts-1" | "tts-1-hd" | "gpt-4o-mini-tts" | "grok-tts" => "grok-tts",
        "grok-voice-tts-1.0" => "grok-voice-tts-1.0",
        _ => anyhow::bail!("unsupported speech model"),
    };
    let text = string(body, "input");
    let text = if text.is_empty() {
        string(body, "text")
    } else {
        text
    };
    ensure!(
        !text.is_empty() && text.chars().count() <= 60000,
        "input must contain 1 to 60000 characters"
    );
    let voice = string(body, "voice");
    let voice = if voice.is_empty() {
        string(body, "voice_id")
    } else {
        voice
    }
    .to_ascii_lowercase();
    let voice = match voice.as_str() {
        "" => "eve",
        "alloy" => "ara",
        "ash" => "orion",
        "ballad" => "luna",
        "coral" => "celeste",
        "echo" => "rex",
        "fable" => "sal",
        "onyx" => "leo",
        "nova" => "eve",
        "sage" => "iris",
        "shimmer" => "aurora",
        "verse" => "lumen",
        other => other,
    };
    let language = string(body, "language");
    let mut payload = json!({"text":text,"voice_id":voice,"language":if language.is_empty() {"auto"} else {language}});
    if let Some(speed) = body
        .get("speed")
        .and_then(Value::as_f64)
        .filter(|s| *s > 0.0)
    {
        payload["speed"] = speed.into();
    }
    let requested = string(body, "response_format").to_ascii_lowercase();
    let mut rate = 24000;
    let codec = if !requested.is_empty() {
        requested
    } else if let Some(output) = body.get("output_format") {
        ensure!(output.is_object(), "output_format must be an object");
        if let Some(value) = output
            .get("sample_rate")
            .and_then(Value::as_f64)
            .filter(|r| *r > 0.0)
        {
            rate = value as i64;
        }
        string(output, "codec").to_ascii_lowercase()
    } else {
        String::new()
    };
    let content_type = match codec.as_str() {
        "" | "mp3" => "audio/mpeg",
        "wav" | "pcm" => {
            payload["output_format"] =
                json!({"codec":codec,"sample_rate":if rate > 0 {rate} else {24000}});
            if codec == "wav" {
                "audio/wav"
            } else {
                "audio/pcm"
            }
        }
        _ => anyhow::bail!("response_format must be mp3, wav, or pcm"),
    };
    Ok((model.into(), payload, content_type))
}
pub fn model_unavailable(body: &Value) -> bool {
    let patterns = [
        "model_not_found",
        "model_not_supported",
        "model is not supported",
        "model is unsupported",
        "model not supported",
        "unsupported model",
        "model is not available",
        "model not available",
        "model is unavailable",
        "model unavailable",
        "not available for your plan",
        "not available for your account",
    ];
    [
        "/code",
        "/error/code",
        "/type",
        "/error/type",
        "/error",
        "/error/message",
        "/message",
        "/detail",
    ]
    .iter()
    .filter_map(|p| body.pointer(p).and_then(Value::as_str))
    .any(|s| patterns.iter().any(|p| s.to_ascii_lowercase().contains(p)))
}

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("").trim()
}
