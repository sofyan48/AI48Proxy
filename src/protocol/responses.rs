//! Chat/Responses bridge derived from CPA's codex/openai/chat-completions translators.
//! Pure protocol conversion; provider normalization and payload rules run afterwards.
use super::{TranslationError, array, invalid, nonempty, object, string, unsupported};
use serde_json::{Value, json};

#[path = "responses_stream.rs"]
mod stream;
pub use stream::StreamTranslator;
#[path = "responses_compat.rs"]
mod compat;
pub use compat::{ChatStreamTranslator, responses_request, responses_response};
#[cfg(test)]
#[path = "responses_tests.rs"]
mod tests;

type Result<T> = std::result::Result<T, TranslationError>;

pub fn chat_request(body: &Value, model: &str) -> Result<Value> {
    object(body)?;
    if let Some(stream) = body.get("stream")
        && !stream.is_boolean()
    {
        return Err(invalid("stream must be boolean"));
    }
    if let Some(options) = body.get("stream_options") {
        object(options)?;
        if options
            .get("include_usage")
            .is_some_and(|v| !v.is_boolean())
        {
            return Err(invalid("include_usage must be boolean"));
        }
    }
    if let Some(text) = body.get("text") {
        object(text)?;
    }
    if body.get("n").is_some_and(|n| n.as_u64() != Some(1)) {
        return Err(unsupported("Responses supports one choice"));
    }
    let mut input = Vec::new();
    for message in array(&body["messages"])? {
        let role = string(&message["role"])?;
        if role == "tool" {
            let output = match &message["content"] {
                Value::String(s) => s.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .map(|p| string(&p["text"]))
                    .collect::<Result<Vec<_>>>()?
                    .join("\n"),
                _ => return Err(invalid("tool output must be text")),
            };
            input.push(json!({"type":"function_call_output","call_id":nonempty(&message["tool_call_id"])?,"output":output}));
            continue;
        }
        if !matches!(role, "system" | "developer" | "user" | "assistant") {
            return Err(unsupported("message role"));
        }
        let text_type = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        let mut content = Vec::new();
        match &message["content"] {
            Value::String(text) if !text.is_empty() => {
                content.push(json!({"type":text_type,"text":text}))
            }
            Value::Array(parts) => {
                for part in parts {
                    content.push(match string(&part["type"])? {
                    "text" => json!({"type":text_type,"text":string(&part["text"])?}),
                    "image_url" if role == "user" => {
                        let mut image = json!({"type":"input_image","image_url":nonempty(&part["image_url"]["url"])?});
                        if let Some(detail) = part["image_url"].get("detail") { image["detail"] = detail.clone(); }
                        image
                    }
                    "file" if role == "user" => {
                        let file = object(&part["file"])?;
                        let mut value = json!({"type":"input_file"});
                        for key in ["file_id", "file_data", "filename"] {
                            if let Some(v) = file.get(key) { value[key] = v.clone(); }
                        }
                        if value.get("file_id").is_none() && value.get("file_data").is_none() { return Err(invalid("file data missing")); }
                        value
                    }
                    _ => return Err(unsupported("Responses content part")),
                });
                }
            }
            Value::Null | Value::String(_) => (),
            _ => return Err(invalid("message content")),
        }
        if !content.is_empty() {
            input.push(json!({"type":"message","role":if role == "system" { "developer" } else {role},"content":content}));
        }
        if let Some(calls) = message.get("tool_calls") {
            for call in array(calls)? {
                if call["type"] != "function" {
                    return Err(unsupported("non-function chat tool"));
                }
                input.push(json!({"type":"function_call","call_id":nonempty(&call["id"])?,
                    "name":nonempty(&call["function"]["name"])?,"arguments":string(&call["function"]["arguments"])?}));
            }
        }
    }
    let mut out = json!({"model":model,"input":input,"instructions":"","stream":body.get("stream").cloned().unwrap_or(json!(false))});
    for key in [
        "temperature",
        "top_p",
        "parallel_tool_calls",
        "service_tier",
        "store",
        "metadata",
        "user",
        "text",
    ] {
        if let Some(v) = body.get(key) {
            out[key] = v.clone();
        }
    }
    for key in ["max_completion_tokens", "max_tokens", "max_output_tokens"] {
        if let Some(v) = body.get(key) {
            out["max_output_tokens"] = v.clone();
            break;
        }
    }
    if let Some(v) = body.get("reasoning_effort") {
        out["reasoning"] = json!({"effort":v});
    }
    if let Some(v) = body.get("reasoning") {
        out["reasoning"] = v.clone();
    }
    if let Some(tools) = body.get("tools") {
        let mut converted = Vec::new();
        for tool in array(tools)? {
            if tool["type"] != "function" {
                return Err(unsupported("non-function chat tool"));
            }
            let mut tool = Value::Object(object(&tool["function"])?.clone());
            nonempty(&tool["name"])?;
            tool["type"] = json!("function");
            // Chat defaults to non-strict; Responses otherwise changes the schema contract.
            if tool.get("strict").is_none() {
                tool["strict"] = json!(false);
            }
            converted.push(tool);
        }
        out["tools"] = json!(converted);
    }
    if let Some(choice) = body.get("tool_choice") {
        out["tool_choice"] = if choice.is_string() {
            choice.clone()
        } else {
            if choice["type"] != "function" {
                return Err(unsupported("tool choice"));
            }
            json!({"type":"function","name":nonempty(&choice["function"]["name"])?})
        };
    }
    if let Some(format) = body.get("response_format") {
        let mapped = match string(&format["type"])? {
            "json_schema" => {
                let mut v = Value::Object(object(&format["json_schema"])?.clone());
                v["type"] = json!("json_schema");
                v
            }
            "text" | "json_object" => format.clone(),
            _ => return Err(unsupported("response format")),
        };
        if out.get("text").is_none() {
            out["text"] = json!({});
        }
        out["text"]["format"] = mapped;
    }
    Ok(out)
}

pub fn usage(value: &Value) -> Value {
    let mut out = json!({});
    for (from, to) in [
        ("input_tokens", "prompt_tokens"),
        ("output_tokens", "completion_tokens"),
        ("total_tokens", "total_tokens"),
        ("input_tokens_details", "prompt_tokens_details"),
        ("output_tokens_details", "completion_tokens_details"),
    ] {
        if let Some(v) = value.get(from) {
            out[to] = v.clone();
        }
    }
    out
}

pub fn finish_reason(response: &Value, tools: bool) -> Result<&'static str> {
    match response["status"].as_str() {
        Some("completed") => Ok(if tools { "tool_calls" } else { "stop" }),
        Some("incomplete") => Ok(
            match response
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
            {
                Some("max_output_tokens" | "max_tokens") => "length",
                Some("content_filter") => "content_filter",
                _ => return Err(unsupported("incomplete response reason")),
            },
        ),
        _ => Err(invalid("Responses did not complete")),
    }
}

pub fn chat_response(response: &Value, model: &str) -> Result<Value> {
    nonempty(&response["id"])?;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut refusal = String::new();
    let mut tools = Vec::new();
    for item in array(&response["output"])? {
        match item["type"].as_str() {
            Some("message") => for part in array(&item["content"])? {
                match part["type"].as_str() {
                    Some("output_text") => text.push_str(string(&part["text"])?),
                    Some("refusal") => refusal.push_str(string(&part["refusal"])?),
                    _ => return Err(unsupported("Responses output content")),
                }
            },
            Some("reasoning") => if let Some(parts) = item.get("summary").or_else(|| item.get("content")) {
                for part in array(parts)? { if let Some(text) = part["text"].as_str() { reasoning.push_str(text); } }
            },
            Some("function_call") => tools.push(json!({"id":nonempty(&item["call_id"])?,"type":"function",
                "function":{"name":nonempty(&item["name"])?,"arguments":string(&item["arguments"])?}})),
            // Go usage_helpers.go classifies these as hosted execution, not client tool calls;
            // its nonstream Chat translator skips them. Images/custom calls need translation.
            Some("web_search_call" | "file_search_call" | "code_interpreter_call" | "mcp_call") => (),
            _ => return Err(unsupported("Responses output item")),
        }
    }
    let mut message =
        json!({"role":"assistant","content":if text.is_empty() {Value::Null} else {json!(text)}});
    if !tools.is_empty() {
        message["tool_calls"] = json!(tools);
    }
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !refusal.is_empty() {
        message["refusal"] = json!(refusal);
    }
    let mut out = json!({"id":response["id"],"object":"chat.completion","created":response.get("created_at").cloned().unwrap_or(json!(0)),
        "model":model,"choices":[{"index":0,"message":message,"finish_reason":finish_reason(response,!tools.is_empty())?}]});
    if let Some(v) = response.get("usage") {
        out["usage"] = usage(v);
    }
    if let Some(v) = response.get("service_tier") {
        out["service_tier"] = v.clone();
    }
    Ok(out)
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    #[test]
    fn function_strict_defaults_only_when_omitted() {
        for strict in [
            None,
            Some(json!(false)),
            Some(json!(true)),
            Some(Value::Null),
        ] {
            let mut function = json!({"name":"read_file","parameters":{"type":"object"}});
            if let Some(value) = &strict {
                function["strict"] = value.clone();
            }
            let request = json!({"messages":[],"tools":[{"type":"function","function":function}]});
            let translated = chat_request(&request, "test").unwrap();
            assert_eq!(
                translated["tools"][0]["strict"],
                strict.unwrap_or(json!(false))
            );
            assert_eq!(translated["tools"][0]["parameters"], function["parameters"]);
        }
    }

    #[test]
    fn only_known_hosted_metadata_is_skipped_beside_text_and_function_calls() {
        for kind in [
            "web_search_call",
            "file_search_call",
            "code_interpreter_call",
            "mcp_call",
            "future_hosted_call",
            "image_generation_call",
            "custom_tool_call",
            "computer_call",
            "shell_call",
            "local_shell_call",
            "apply_patch_call",
        ] {
            let response = json!({"id":"resp_test","status":"completed","output":[
                {"type":kind,"status":"completed"},
                {"type":"message","content":[{"type":"output_text","text":"Answer"}]},
                {"type":"function_call","call_id":"call_test","name":"read_file","arguments":"{}"}
            ]});
            let result = chat_response(&response, "test");
            if matches!(
                kind,
                "web_search_call" | "file_search_call" | "code_interpreter_call" | "mcp_call"
            ) {
                let translated = result.unwrap();
                assert_eq!(translated["choices"][0]["message"]["content"], "Answer");
                assert_eq!(
                    translated["choices"][0]["message"]["tool_calls"][0]["id"],
                    "call_test"
                );
                assert_eq!(translated["choices"][0]["finish_reason"], "tool_calls");
            } else {
                assert!(result.is_err(), "must not silently discard {kind}");
            }
        }
    }
}
