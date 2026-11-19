//! Strict OpenAI Chat <-> Claude Messages translation for text and function tools.
//! No network, credentials, routing, clock reads or payload rules live here.
#[path = "protocol/gemini.rs"]
pub mod gemini;
#[path = "protocol/messages.rs"]
mod messages;
#[path = "protocol/response.rs"]
mod response;
#[path = "protocol/responses.rs"]
pub mod responses;
#[path = "protocol/stream.rs"]
mod stream;
#[cfg(test)]
#[path = "protocol/stream_tests.rs"]
mod stream_tests;
#[cfg(test)]
#[path = "protocol/tests.rs"]
mod tests;
#[path = "protocol/usage.rs"]
mod usage;
use messages::*;
pub use response::{ResponseOptions, translate_response};
use serde_json::{Map, Value, json};
use std::{collections::HashSet, error::Error, fmt};
pub use stream::StreamTranslator;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    OpenAiChat,
    ClaudeMessages,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslationError {
    Invalid(String),
    Unsupported(String),
}
impl fmt::Display for TranslationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(s) => write!(f, "invalid protocol payload: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported protocol feature: {s}"),
        }
    }
}
impl Error for TranslationError {}
type Result<T> = std::result::Result<T, TranslationError>;
fn invalid(s: &str) -> TranslationError {
    TranslationError::Invalid(s.into())
}
fn unsupported(s: &str) -> TranslationError {
    TranslationError::Unsupported(s.into())
}
fn object(v: &Value) -> Result<&Map<String, Value>> {
    v.as_object().ok_or_else(|| invalid("expected object"))
}
fn array(v: &Value) -> Result<&Vec<Value>> {
    v.as_array().ok_or_else(|| invalid("expected array"))
}
fn string(v: &Value) -> Result<&str> {
    v.as_str().ok_or_else(|| invalid("expected string"))
}
fn nonempty(v: &Value) -> Result<&str> {
    let s = string(v)?;
    if s.is_empty() {
        return Err(invalid("empty identifier"));
    }
    Ok(s)
}
fn fields(v: &Value, allowed: &[&str]) -> Result<()> {
    for key in object(v)?.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(TranslationError::Unsupported(format!("field {key:?}")));
        }
    }
    Ok(())
}
fn uint(v: &Value) -> Result<u64> {
    v.as_u64()
        .ok_or_else(|| invalid("expected nonnegative integer"))
}
fn no_stream(v: &Value) -> Result<()> {
    if let Some(stream) = v.get("stream") {
        match stream.as_bool() {
            Some(false) => (),
            Some(true) => return Err(unsupported("streaming")),
            None => return Err(invalid("stream must be boolean")),
        }
    }
    if v.get("stream_options").is_some() {
        return Err(unsupported("stream_options"));
    }
    Ok(())
}

/// Target model must be supplied explicitly (no cross-provider model guessing).
/// Claude requires max_tokens: if OpenAI omitted it, supply a positive default.
/// Used in reverse response translation because Claude has no `created` field.
#[derive(Debug, Clone, Copy)]
pub struct RequestOptions<'a> {
    pub target_model: &'a str,
    pub default_max_tokens: Option<u64>,
}

/// Translate a complete request. Only opposite formats are accepted: this API
/// must not be mistaken for a same-format pass-through or a streaming converter.
/// Unknown request knobs are rejected instead of silently losing semantics.
pub fn translate_request(
    from: Format,
    to: Format,
    body: &Value,
    options: RequestOptions<'_>,
) -> Result<Value> {
    if from == to {
        return Err(unsupported(
            "same-format translation; use the gateway directly",
        ));
    }
    if options.target_model.is_empty() {
        return Err(invalid("target_model is required"));
    }
    let streaming = body
        .get("stream")
        .map(|v| v.as_bool().ok_or_else(|| invalid("stream must be boolean")))
        .transpose()?;
    if let Some(options) = body.get("stream_options") {
        fields(options, &["include_usage"])?;
        if options
            .get("include_usage")
            .is_some_and(|v| !v.is_boolean())
        {
            return Err(invalid("include_usage must be boolean"));
        }
    }
    nonempty(&body["model"])?;
    let openai = from == Format::OpenAiChat;
    if openai {
        fields(
            body,
            &[
                "model",
                "messages",
                "max_tokens",
                "max_completion_tokens",
                "temperature",
                "top_p",
                "stop",
                "stream",
                "stream_options",
                "tools",
                "tool_choice",
                "n",
            ],
        )?;
        if body.get("n").is_some_and(|v| v.as_u64() != Some(1)) {
            return Err(unsupported("multiple choices"));
        }
        if body.get("max_tokens").is_some() && body.get("max_completion_tokens").is_some() {
            return Err(invalid("conflicting token limits"));
        }
    } else {
        fields(
            body,
            &[
                "model",
                "messages",
                "system",
                "max_tokens",
                "temperature",
                "top_p",
                "stop_sequences",
                "stream",
                "tools",
                "tool_choice",
            ],
        )?;
    }
    let token = body
        .get("max_tokens")
        .or_else(|| body.get("max_completion_tokens"));
    let max = token.map(uint).transpose()?.or(if openai {
        options.default_max_tokens
    } else {
        None
    });
    if max == Some(0) {
        return Err(invalid("max_tokens must be positive"));
    }
    if max.is_none() {
        return Err(invalid(
            "Claude max_tokens required; supply explicit default for OpenAI requests",
        ));
    }
    let mut result = json!({"model":options.target_model,"max_tokens":max.unwrap()});
    for key in ["temperature", "top_p"] {
        if let Some(v) = body.get(key) {
            let n = v
                .as_f64()
                .ok_or_else(|| invalid("sampling parameter must be numeric"))?;
            let upper = if key == "temperature" && !openai {
                2.0
            } else {
                1.0
            };
            // OpenAI temperature >1 cannot be represented faithfully by Claude.
            if !(0.0..=upper).contains(&n) {
                return Err(unsupported("sampling parameter outside target range"));
            }
            result[key] = v.clone();
        }
    }
    if let Some(v) = body.get(if openai { "stop" } else { "stop_sequences" }) {
        let stops = if openai && v.is_string() {
            vec![v.clone()]
        } else {
            array(v)?.clone()
        };
        for stop in &stops {
            nonempty(stop)?;
        }
        result[if openai { "stop_sequences" } else { "stop" }] = json!(stops);
    }
    let (messages, system) = if openai {
        openai_messages(&body["messages"])?
    } else {
        claude_messages(&body["messages"], body.get("system"))?
    };
    result["messages"] = json!(messages);
    if openai && !system.is_empty() {
        result["system"] = json!(system);
    }
    if let Some(tools) = body.get("tools") {
        result["tools"] = translate_tools(tools, openai)?;
    }
    if let Some(choice) = body.get("tool_choice") {
        let mapped = tool_choice(choice, openai)?;
        if !mapped.is_null() {
            result["tool_choice"] = mapped;
        } else {
            result.as_object_mut().unwrap().remove("tools");
        }
    }
    if let Some(streaming) = streaming {
        result["stream"] = json!(streaming);
        if streaming && !openai {
            result["stream_options"] = json!({"include_usage":true});
        }
    }
    Ok(result)
}
