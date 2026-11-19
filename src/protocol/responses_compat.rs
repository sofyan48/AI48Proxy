//! Responses -> Chat and Chat -> Responses mappings from CPA's
//! openai/openai/responses translators. Stateful continuation is deliberately rejected.
use super::{Result, array, invalid, nonempty, object, string, unsupported};
use serde_json::{Value, json};

#[path = "responses_compat_stream.rs"]
mod stream;
pub use stream::ChatStreamTranslator;
#[cfg(test)]
#[path = "responses_compat_tests.rs"]
mod tests;

pub fn responses_request(body: &Value, model: &str) -> Result<Value> {
    object(body)?;
    for field in ["previous_response_id", "conversation", "context_management"] {
        if body
            .get(field)
            .is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
        {
            return Err(unsupported(
                "compat continuation/compaction requires full input history",
            ));
        }
    }
    let mut messages: Vec<Value> = Vec::new();
    if let Some(instructions) = body.get("instructions").filter(|v| !v.is_null()) {
        messages.push(json!({"role":"system","content":string(instructions)?}));
    }
    let mut reasoning = String::new();
    let items = match &body["input"] {
        Value::String(s) => vec![json!({"role":"user","content":s})],
        Value::Array(items) => items.clone(),
        _ => return Err(invalid("Responses input must be text or array")),
    };
    for item in &items {
        match item["type"].as_str().unwrap_or("message") {
            "message" => {
                let role = string(&item["role"])?;
                if !matches!(role, "system" | "developer" | "user" | "assistant") {
                    return Err(unsupported("Responses message role"));
                }
                let content = if item["content"].is_string() {
                    item["content"].clone()
                } else {
                    let mut parts = Vec::new();
                    for part in array(&item["content"])? {
                        parts.push(match string(&part["type"])? {
                            "input_text" | "output_text" | "text" => json!({"type":"text","text":string(&part["text"])?}),
                            "input_image" if role == "user" => {
                                let mut image = json!({"type":"image_url","image_url":{"url":nonempty(&part["image_url"])?}});
                                if let Some(detail) = part.get("detail") { image["image_url"]["detail"] = detail.clone(); }
                                image
                            }
                            "input_file" if role == "user" => {
                                let mut file = json!({});
                                for key in ["file_id","file_data","filename"] { if let Some(v) = part.get(key) {file[key] = v.clone();} }
                                if file.get("file_id").is_none() && file.get("file_data").is_none() {return Err(invalid("input file missing data"));}
                                json!({"type":"file","file":file})
                            }
                            _ => return Err(unsupported("compat Responses content part")),
                        });
                    }
                    json!(parts)
                };
                let mut message = json!({"role":role,"content":content});
                if role == "assistant" && !reasoning.is_empty() {
                    message["reasoning_content"] = json!(std::mem::take(&mut reasoning));
                }
                messages.push(message);
            }
            "reasoning" => {
                if let Some(parts) = item.get("summary").or_else(|| item.get("content")) {
                    for part in array(parts)? {
                        if let Some(text) = part["text"].as_str() {
                            reasoning.push_str(text);
                        }
                    }
                }
            }
            "function_call" => {
                let call = json!({"id":nonempty(&item["call_id"])?,"type":"function","function":{
                    "name":nonempty(&item["name"])?,"arguments":string(&item["arguments"])?}});
                if messages.last().is_none_or(|m| m["role"] != "assistant") {
                    messages.push(json!({"role":"assistant","content":null}));
                }
                let message = messages.last_mut().unwrap();
                if message.get("tool_calls").is_none() {
                    message["tool_calls"] = json!([]);
                }
                message["tool_calls"].as_array_mut().unwrap().push(call);
                if !reasoning.is_empty() {
                    message["reasoning_content"] = json!(std::mem::take(&mut reasoning));
                }
            }
            "function_call_output" => {
                let output = &item["output"];
                let content = if output.is_string() {
                    output.clone()
                } else {
                    let mut parts = Vec::new();
                    for part in array(output)? {
                        if !matches!(
                            part["type"].as_str(),
                            Some("input_text" | "output_text" | "text")
                        ) {
                            return Err(unsupported("multimodal tool output"));
                        }
                        parts.push(string(&part["text"])?);
                    }
                    json!(parts.join("\n\n"))
                };
                messages.push(json!({"role":"tool","tool_call_id":nonempty(&item["call_id"])?,"content":content}));
            }
            _ => return Err(unsupported("compat Responses input item")),
        }
    }
    if !reasoning.is_empty() {
        return Err(unsupported("unattached reasoning history"));
    }
    let mut out = json!({"model":model,"messages":messages,"stream":body.get("stream").cloned().unwrap_or(json!(false))});
    for key in [
        "temperature",
        "top_p",
        "parallel_tool_calls",
        "service_tier",
        "user",
        "metadata",
        "prompt_cache_key",
        "frequency_penalty",
        "presence_penalty",
    ] {
        if let Some(v) = body.get(key) {
            out[key] = v.clone();
        }
    }
    if let Some(v) = body.get("max_output_tokens") {
        out["max_tokens"] = v.clone();
    }
    if let Some(v) = body.pointer("/reasoning/effort") {
        out["reasoning_effort"] = v.clone();
    }
    if let Some(v) = body.pointer("/text/verbosity") {
        out["verbosity"] = v.clone();
    }
    if let Some(format) = body.pointer("/text/format") {
        out["response_format"] = match string(&format["type"])? {
            "json_schema" => {
                let mut schema = format.clone();
                schema.as_object_mut().unwrap().remove("type");
                json!({"type":"json_schema","json_schema":schema})
            }
            "text" | "json_object" => format.clone(),
            _ => return Err(unsupported("Responses text format")),
        };
    }
    if let Some(tools) = body.get("tools") {
        let mut converted = Vec::new();
        for tool in array(tools)? {
            if tool["type"] != "function" {
                return Err(unsupported("compat hosted/custom tool"));
            }
            let mut function = Value::Object(object(tool)?.clone());
            nonempty(&function["name"])?;
            function.as_object_mut().unwrap().remove("type");
            converted.push(json!({"type":"function","function":function}));
        }
        out["tools"] = json!(converted);
    }
    if let Some(choice) = body.get("tool_choice") {
        out["tool_choice"] = if choice.is_string() {
            choice.clone()
        } else {
            if choice["type"] != "function" {
                return Err(unsupported("compat tool choice"));
            }
            json!({"type":"function","function":{"name":nonempty(&choice["name"])?}})
        };
    }
    Ok(out)
}

pub fn responses_usage(value: &Value) -> Value {
    let mut out = json!({});
    for (from, to) in [
        ("prompt_tokens", "input_tokens"),
        ("completion_tokens", "output_tokens"),
        ("total_tokens", "total_tokens"),
        ("prompt_tokens_details", "input_tokens_details"),
        ("completion_tokens_details", "output_tokens_details"),
    ] {
        if let Some(v) = value.get(from) {
            out[to] = v.clone();
        }
    }
    out
}

pub fn response_status(reason: &str) -> Result<(&'static str, Value)> {
    Ok(match reason {
        "stop" | "tool_calls" => ("completed", Value::Null),
        "length" | "max_tokens" => ("incomplete", json!({"reason":"max_output_tokens"})),
        "content_filter" => ("incomplete", json!({"reason":"content_filter"})),
        _ => return Err(unsupported("Chat finish reason")),
    })
}

pub fn responses_response(chat: &Value, model: &str) -> Result<Value> {
    let choices = array(&chat["choices"])?;
    if choices.len() != 1 {
        return Err(unsupported("Responses requires one Chat choice"));
    }
    let choice = &choices[0];
    let (status, incomplete) = response_status(string(&choice["finish_reason"])?)?;
    let message = &choice["message"];
    object(message)?;
    let id = nonempty(&chat["id"])?;
    let mut output = Vec::new();
    if let Some(reasoning) = message["reasoning_content"]
        .as_str()
        .filter(|s| !s.is_empty())
    {
        output.push(json!({"id":format!("rs_{id}"),"type":"reasoning","summary":[{"type":"summary_text","text":reasoning}]}));
    }
    let mut content = Vec::new();
    if let Some(text) = message.get("content").filter(|v| !v.is_null()) {
        content.push(json!({"type":"output_text","text":string(text)?,"annotations":[]}));
    }
    if let Some(refusal) = message["refusal"].as_str() {
        content.push(json!({"type":"refusal","refusal":refusal}));
    }
    if !content.is_empty() {
        output.push(json!({"id":format!("msg_{id}"),"type":"message","status":status,"role":"assistant","content":content}));
    }
    if let Some(calls) = message.get("tool_calls") {
        for call in array(calls)? {
            if call["type"] != "function" {
                return Err(unsupported("Chat custom tool output"));
            }
            let call_id = nonempty(&call["id"])?;
            output.push(json!({"id":format!("fc_{call_id}"),"type":"function_call","status":status,"call_id":call_id,
                "name":nonempty(&call["function"]["name"])?,"arguments":string(&call["function"]["arguments"])?}));
        }
    }
    let mut response = json!({"id":id,"object":"response","created_at":chat.get("created").cloned().unwrap_or(json!(0)),
        "model":model,"status":status,"error":null,"incomplete_details":incomplete,"output":output});
    if let Some(usage) = chat.get("usage") {
        response["usage"] = responses_usage(usage);
    }
    if let Some(tier) = chat.get("service_tier") {
        response["service_tier"] = tier.clone();
    }
    Ok(response)
}
