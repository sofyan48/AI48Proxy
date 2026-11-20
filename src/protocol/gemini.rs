//! OpenAI Chat -> native Gemini requests and native Gemini -> Chat responses.
//! Text/function-tools only; transport wrappers (Vertex/Antigravity) belong to callers.
//! Mappings follow CLIProxyAPI/internal/translator/gemini/openai/chat-completions.
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};

#[path = "gemini_request.rs"]
mod request;
#[path = "gemini_schema.rs"]
pub(crate) mod schema;
const BYPASS: &str = "skip_thought_signature_validator";
fn is_bypass(signature: &str) -> bool {
    matches!(
        signature,
        "skip_thought_signature_validator" | "context_engineering_is_the_way_to_go"
    )
}
#[path = "gemini_stream.rs"]
mod stream;
pub use request::{antigravity_chat_request, chat_request, openai_request};
pub use stream::StreamTranslator;
#[cfg(test)]
#[path = "gemini_tests.rs"]
mod tests;

fn object(value: &Value) -> Result<()> {
    ensure!(value.is_object(), "expected JSON object");
    Ok(())
}
fn array(value: &Value) -> Result<&Vec<Value>> {
    value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("expected array"))
}
fn text(value: &Value) -> Result<&str> {
    value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("expected string"))
}
fn name(value: &Value) -> Result<&str> {
    let value = text(value)?;
    ensure!(!value.is_empty(), "empty function name");
    // No request context is available to reverse a lossy name sanitization.
    ensure!(
        value.len() <= 64
            && value.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':')),
        "unsupported function name"
    );
    Ok(value)
}
fn upstream_error(body: &Value) -> Result<()> {
    object(body)?;
    if body.get("error").is_some_and(|v| !v.is_null()) {
        // Do not copy arbitrary upstream bodies into errors (they can contain secrets).
        bail!("Gemini upstream error");
    }
    if body
        .pointer("/promptFeedback/blockReason")
        .is_some_and(|v| {
            v.as_str()
                .is_some_and(|s| !s.is_empty() && s != "BLOCK_REASON_UNSPECIFIED")
        })
    {
        bail!("Gemini prompt blocked");
    }
    Ok(())
}
fn finish_reason(reason: &str, tools: bool) -> Result<&'static str> {
    // A finishReason is required, rather than inventing success on truncated EOF.
    let reason = reason.to_ascii_uppercase();
    let mapped = match reason.as_str() {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY"
        | "RECITATION"
        | "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "SPII"
        | "IMAGE_SAFETY"
        | "MALFORMED_RESPONSE"
        | "MALFORMED_FUNCTION_CALL" => "content_filter",
        _ => bail!("unsupported Gemini finishReason: {reason}"),
    };
    // Preserve unsuccessful terminals even if tools were seen earlier.
    Ok(if tools && mapped == "stop" {
        "tool_calls"
    } else {
        mapped
    })
}
fn usage(value: &Value) -> Result<Value> {
    object(value)?;
    let count = |key: &str| -> Result<u64> {
        value
            .get(key)
            .map(|v| {
                v.as_u64()
                    .ok_or_else(|| anyhow::anyhow!("invalid Gemini token count"))
            })
            .unwrap_or(Ok(0))
    };
    let prompt = count("promptTokenCount")?;
    let thoughts = count("thoughtsTokenCount")?;
    let completion = count("candidatesTokenCount")?
        .checked_add(thoughts)
        .ok_or_else(|| anyhow::anyhow!("token count overflow"))?;
    let total = if value.get("totalTokenCount").is_some() {
        count("totalTokenCount")?
    } else {
        prompt
            .checked_add(completion)
            .ok_or_else(|| anyhow::anyhow!("token count overflow"))?
    };
    let mut out =
        json!({"prompt_tokens":prompt,"completion_tokens":completion,"total_tokens":total});
    if value.get("thoughtsTokenCount").is_some() {
        out["completion_tokens_details"] = json!({"reasoning_tokens":thoughts});
    }
    if value.get("cachedContentTokenCount").is_some() {
        out["prompt_tokens_details"] = json!({"cached_tokens":count("cachedContentTokenCount")?});
    }
    Ok(out)
}

fn parts(candidate: &Value, index: u64, next_tool: &mut u64) -> Result<Value> {
    object(candidate)?;
    let mut message = json!({});
    let Some(content) = candidate.get("content") else {
        return Ok(message);
    };
    object(content)?;
    let mut calls = Vec::new();
    for part in array(&content["parts"])? {
        object(part)?;
        for key in part.as_object().unwrap().keys() {
            ensure!(
                [
                    "text",
                    "thought",
                    "thoughtSignature",
                    "thought_signature",
                    "functionCall"
                ]
                .contains(&key.as_str()),
                "unsupported Gemini response part: {key}"
            );
        }
        ensure!(
            part.get("text").is_some() || part.get("functionCall").is_some(),
            "unsupported Gemini signature-only or empty response part"
        );
        if let Some(thought) = part.get("thought") {
            ensure!(thought.is_boolean(), "invalid thought flag");
        }
        ensure!(
            !(part.get("text").is_some() && part.get("functionCall").is_some()),
            "ambiguous Gemini part"
        );
        if let Some(value) = part.get("text") {
            let key = if part["thought"] == true {
                "reasoning_content"
            } else {
                "content"
            };
            let mut joined = message[key].as_str().unwrap_or_default().to_owned();
            joined.push_str(text(value)?);
            message[key] = json!(joined);
        }
        if let Some(call) = part.get("functionCall") {
            object(call)?;
            ensure!(
                call.get("partialArgs").is_none() && call.get("willContinue").is_none(),
                "unsupported incremental Gemini function arguments"
            );
            let function_name = name(&call["name"])?;
            let args = call.get("args").cloned().unwrap_or(json!({}));
            object(&args)?;
            let tool_index = *next_tool;
            *next_tool += 1;
            let mut tool = json!({"index":tool_index,"id":call.get("id").map(text).transpose()?.map(str::to_owned).unwrap_or_else(|| format!("call_gemini_{index}_{tool_index}")),
                "type":"function","function":{"name":function_name,"arguments":serde_json::to_string(&args)?}});
            if let Some(signature) = part
                .get("thoughtSignature")
                .or_else(|| part.get("thought_signature"))
            {
                let signature = text(signature)?;
                if signature != "skip_thought_signature_validator" {
                    tool["extra_content"] = json!({"google":{"thought_signature":signature}});
                }
            }
            calls.push(tool);
        }
    }
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    Ok(message)
}
fn metadata(body: &Value, out: &mut Value) -> Result<()> {
    if let Some(id) = body.get("responseId") {
        out["id"] = json!(text(id)?);
    }
    if let Some(model) = body.get("modelVersion") {
        out["model"] = json!(text(model)?);
    }
    if let Some(time) = body.get("createTime") {
        let parsed = chrono::DateTime::parse_from_rfc3339(text(time)?)?;
        out["created"] = json!(parsed.timestamp());
    }
    Ok(())
}
fn envelope(model: &str, streaming: bool) -> Value {
    json!({"id":"chatcmpl-gemini","object":if streaming {"chat.completion.chunk"} else {"chat.completion"},"created":0,"model":model,"choices":[]})
}

/// Convert a native Gemini generateContent response. Metadata may be absent.
pub fn chat_response(body: &Value, model: &str) -> Result<Value> {
    upstream_error(body)?;
    let mut out = envelope(model, false);
    metadata(body, &mut out)?;
    let mut choices = Vec::new();
    for (position, candidate) in array(&body["candidates"])?.iter().enumerate() {
        let index = candidate
            .get("index")
            .map(|v| {
                v.as_u64()
                    .ok_or_else(|| anyhow::anyhow!("invalid candidate index"))
            })
            .transpose()?
            .unwrap_or(position as u64);
        ensure!(
            !choices.iter().any(|c: &Value| c["index"] == index),
            "duplicate candidate index"
        );
        let mut message = parts(candidate, index, &mut 0)?;
        let tools = message.get("tool_calls").is_some();
        if let Some(calls) = message["tool_calls"].as_array_mut() {
            for call in calls {
                call.as_object_mut().unwrap().remove("index");
            }
        }
        message["role"] = json!("assistant");
        if message.get("content").is_none() {
            message["content"] = if tools { Value::Null } else { json!("") };
        }
        let native = text(&candidate["finishReason"])?;
        choices.push(json!({"index":index,"message":message,"finish_reason":finish_reason(native, tools)?,"native_finish_reason":native.to_ascii_lowercase()}));
    }
    ensure!(!choices.is_empty(), "Gemini response has no candidates");
    out["choices"] = json!(choices);
    if let Some(value) = body.get("usageMetadata") {
        out["usage"] = usage(value)?;
    }
    Ok(out)
}
