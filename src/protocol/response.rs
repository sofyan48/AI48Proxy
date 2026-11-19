use super::*;

/// Response context is supplied by the caller, not inferred from another
/// provider's model namespace. `created` is used only for OpenAI output.
#[derive(Debug, Clone, Copy)]
pub struct ResponseOptions<'a> {
    pub target_model: &'a str,
    pub created: u64,
}

/// Translate one final response, preserving IDs, text, basic function calls,
/// finish reasons and uncached token usage. SSE/delta events are unsupported.
/// Cache/reasoning usage fields are rejected rather than mis-accounted.
pub fn translate_response(
    from: Format,
    to: Format,
    body: &Value,
    options: ResponseOptions<'_>,
) -> Result<Value> {
    if from == to {
        return Err(unsupported("same-format translation"));
    }
    no_stream(body)?;
    if options.target_model.is_empty() {
        return Err(invalid("target_model required"));
    }
    let id = nonempty(&body["id"])?;
    let empty_usage = json!({});
    let source_usage = body
        .get("usage")
        .filter(|u| !u.is_null())
        .unwrap_or(&empty_usage);
    let openai = from == Format::OpenAiChat;
    let (blocks, finish, input, output) = if openai {
        if body["object"] != "chat.completion" {
            return Err(unsupported("streaming chunk or non-chat response"));
        }
        fields(
            body,
            &[
                "id",
                "object",
                "created",
                "model",
                "choices",
                "usage",
                "system_fingerprint",
                "service_tier",
            ],
        )?;
        uint(&body["created"])?;
        let choices = array(&body["choices"])?;
        if choices.len() != 1 {
            return Err(unsupported("multiple or empty choices"));
        }
        let c = &choices[0];
        fields(c, &["index", "message", "finish_reason"])?;
        if c["index"] != 0 {
            return Err(unsupported("nonzero choice index"));
        }
        let m = &c["message"];
        fields(m, &["role", "content", "tool_calls"])?;
        if m["role"] != "assistant" {
            return Err(invalid("response must be assistant"));
        }
        let mut blocks = Vec::new();
        let s = text(&m["content"], true)?;
        if !s.is_empty() {
            blocks.push(json!({"type":"text","text":s}));
        }
        let mut ids = HashSet::new();
        if let Some(calls) = m.get("tool_calls") {
            for c in array(calls)? {
                let block = openai_call(c)?;
                if !ids.insert(string(&block["id"])?.to_owned()) {
                    return Err(invalid("duplicate response tool id"));
                }
                blocks.push(block);
            }
        }
        let finish = match string(&c["finish_reason"])? {
            "stop" => "end_turn",
            "length" => "max_tokens",
            "tool_calls" => "tool_use",
            _ => return Err(unsupported("OpenAI finish_reason")),
        };
        if (finish == "tool_use") != !ids.is_empty() {
            return Err(invalid("tool calls and finish reason disagree"));
        }
        let u = source_usage;
        fields(
            u,
            &[
                "prompt_tokens",
                "completion_tokens",
                "total_tokens",
                "prompt_tokens_details",
                "completion_tokens_details",
            ],
        )?;
        let input = u.get("prompt_tokens").map(uint).transpose()?.unwrap_or(0);
        let output = u
            .get("completion_tokens")
            .map(uint)
            .transpose()?
            .unwrap_or(0);
        if u.get("prompt_tokens").is_some()
            && u.get("completion_tokens").is_some()
            && u.get("total_tokens").is_some()
            && input.checked_add(output) != Some(uint(&u["total_tokens"])?)
        {
            return Err(invalid("usage total mismatch or overflow"));
        }
        (blocks, finish, input, output)
    } else {
        if body["type"] != "message" {
            return Err(unsupported("streaming event or non-message response"));
        }
        fields(
            body,
            &[
                "id",
                "type",
                "role",
                "model",
                "content",
                "stop_reason",
                "stop_sequence",
                "usage",
            ],
        )?;
        if body["role"] != "assistant" {
            return Err(invalid("response must be assistant"));
        }
        if body.get("stop_sequence").is_some_and(|v| !v.is_null()) {
            return Err(unsupported(
                "matched stop_sequence cannot be preserved in OpenAI response",
            ));
        }
        let finish = match string(&body["stop_reason"])? {
            "end_turn" => "stop",
            "max_tokens" => "length",
            "tool_use" => "tool_calls",
            _ => return Err(unsupported("Claude stop_reason")),
        };
        let blocks = array(&body["content"])?.clone();
        let u = source_usage;
        fields(
            u,
            &[
                "input_tokens",
                "output_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
                "output_tokens_details",
                "thinking_tokens",
                "cache_creation",
            ],
        )?;
        (
            blocks,
            finish,
            u.get("input_tokens").map(uint).transpose()?.unwrap_or(0),
            u.get("output_tokens").map(uint).transpose()?.unwrap_or(0),
        )
    };
    let mapped = super::usage::mapped_usage(from, source_usage)?;
    if openai {
        let mut response = json!({"id":id,"type":"message","role":"assistant","model":options.target_model,"content":blocks,"stop_reason":finish,"stop_sequence":null,"usage":{"input_tokens":input,"output_tokens":output}});
        response["usage"] = mapped;
        Ok(response)
    } else {
        let mut s = String::new();
        let mut calls = Vec::new();
        let mut ids = HashSet::new();
        for block in &blocks {
            match block["type"].as_str() {
                Some("text") if calls.is_empty() => {
                    fields(block, &["type", "text"])?;
                    s.push_str(string(&block["text"])?);
                }
                Some("tool_use") => {
                    let c = claude_call(block)?;
                    if !ids.insert(string(&c["id"])?.to_owned()) {
                        return Err(invalid("duplicate response tool id"));
                    }
                    calls.push(c);
                }
                _ => {
                    return Err(unsupported(
                        "response block media/thinking or text after tools",
                    ));
                }
            }
        }
        if (finish == "tool_calls") != !calls.is_empty() {
            return Err(invalid("tool calls and stop reason disagree"));
        }
        let mut m = json!({"role":"assistant","content":if s.is_empty() && !calls.is_empty() { Value::Null } else { json!(s) }});
        if !calls.is_empty() {
            m["tool_calls"] = json!(calls);
        }
        let total = input
            .checked_add(output)
            .ok_or_else(|| invalid("usage overflow"))?;
        let mut response = json!({"id":id,"object":"chat.completion","created":options.created,"model":options.target_model,"choices":[{"index":0,"message":m,"finish_reason":finish}],"usage":{"prompt_tokens":input,"completion_tokens":output,"total_tokens":total}});
        response["usage"] = mapped;
        Ok(response)
    }
}
