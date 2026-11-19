use super::*;

/// Text blocks only. Media, citations and cache-control are not silently stripped.
pub(super) fn text(v: &Value, allow_null: bool) -> Result<String> {
    if allow_null && v.is_null() {
        return Ok(String::new());
    }
    if let Some(s) = v.as_str() {
        return Ok(s.into());
    }
    let mut result = String::new();
    for block in array(v)? {
        if block["type"] != "text" {
            return Err(unsupported(
                "non-text content (media, thinking, or tool block)",
            ));
        }
        fields(block, &["type", "text"])?;
        result.push_str(string(&block["text"])?);
    }
    Ok(result)
}
pub(super) fn push_message(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut().filter(|v| v["role"] == role) {
        last["content"].as_array_mut().unwrap().extend(blocks);
    } else {
        messages.push(json!({"role":role,"content":blocks}));
    }
}
pub(super) fn parse_arguments(v: &Value) -> Result<Value> {
    let args: Value = serde_json::from_str(string(v)?)
        .map_err(|_| invalid("function arguments must be valid JSON"))?;
    object(&args)?;
    Ok(args)
}
pub(super) fn openai_call(call: &Value) -> Result<Value> {
    fields(call, &["id", "type", "function"])?;
    if call["type"] != "function" {
        return Err(unsupported("non-function tool"));
    }
    let f = &call["function"];
    fields(f, &["name", "arguments"])?;
    Ok(
        json!({"type":"tool_use","id":nonempty(&call["id"])?,"name":nonempty(&f["name"])?,"input":parse_arguments(&f["arguments"])?}),
    )
}
pub(super) fn claude_call(block: &Value) -> Result<Value> {
    fields(block, &["type", "id", "name", "input"])?;
    object(&block["input"])?;
    Ok(
        json!({"id":nonempty(&block["id"])?,"type":"function","function":{"name":nonempty(&block["name"])?,"arguments":serde_json::to_string(&block["input"]).map_err(|_| invalid("tool input serialization"))?}}),
    )
}
pub(super) fn openai_messages(v: &Value) -> Result<(Vec<Value>, String)> {
    let mut messages = Vec::new();
    let mut systems = Vec::new();
    let mut pending = HashSet::new();
    for m in array(v)? {
        fields(m, &["role", "content", "tool_calls", "tool_call_id"])?;
        let role = string(&m["role"])?;
        if role == "system" {
            if !messages.is_empty() {
                return Err(unsupported("non-leading system messages"));
            }
            fields(m, &["role", "content"])?;
            systems.push(text(&m["content"], false)?);
            continue;
        }
        if role == "tool" {
            fields(m, &["role", "content", "tool_call_id"])?;
            let id = nonempty(&m["tool_call_id"])?;
            if !pending.remove(id) {
                return Err(invalid("tool result has no matching pending call"));
            }
            push_message(
                &mut messages,
                "user",
                vec![
                    json!({"type":"tool_result","tool_use_id":id,"content":text(&m["content"], false)?}),
                ],
            );
            continue;
        }
        if !pending.is_empty() {
            return Err(invalid("missing tool results before next message"));
        }
        if role != "user" && role != "assistant" {
            return Err(unsupported("message role"));
        }
        if m.get("tool_call_id").is_some() {
            return Err(invalid("tool_call_id outside tool result"));
        }
        let mut blocks = Vec::new();
        let content = text(&m["content"], role == "assistant")?;
        if !content.is_empty() {
            blocks.push(json!({"type":"text","text":content}));
        }
        if let Some(calls) = m.get("tool_calls") {
            if role != "assistant" {
                return Err(invalid("tool_calls outside assistant message"));
            }
            for call in array(calls)? {
                let block = openai_call(call)?;
                if !pending.insert(string(&block["id"])?.to_owned()) {
                    return Err(invalid("duplicate tool call id"));
                }
                blocks.push(block);
            }
        }
        if blocks.is_empty() {
            return Err(invalid("empty message"));
        }
        push_message(&mut messages, role, blocks);
    }
    if !pending.is_empty() {
        return Err(invalid("missing tool results"));
    }
    if messages.first().is_none_or(|m| m["role"] != "user") {
        return Err(unsupported("Claude conversation must start with user"));
    }
    Ok((messages, systems.join("\n\n")))
}
pub(super) fn claude_messages(v: &Value, system: Option<&Value>) -> Result<(Vec<Value>, String)> {
    let mut messages = Vec::new();
    if let Some(s) = system {
        messages.push(json!({"role":"system","content":text(s, false)?}));
    }
    let mut pending = HashSet::new();
    let input = array(v)?;
    if input.first().is_none_or(|m| m["role"] != "user") {
        return Err(unsupported("conversation must start with user"));
    }
    for m in input {
        fields(m, &["role", "content"])?;
        let role = string(&m["role"])?;
        if role != "user" && role != "assistant" {
            return Err(unsupported("Claude message role"));
        }
        let blocks = if m["content"].is_string() {
            vec![json!({"type":"text","text":m["content"]})]
        } else {
            array(&m["content"])?.clone()
        };
        if blocks.is_empty() {
            return Err(invalid("empty message"));
        }
        let mut texts = String::new();
        let mut calls = Vec::new();
        for block in &blocks {
            match block["type"].as_str() {
                Some("text") => {
                    fields(block, &["type", "text"])?;
                    if !calls.is_empty() {
                        return Err(unsupported(
                            "text after tool calls cannot preserve block order",
                        ));
                    }
                    if !pending.is_empty() {
                        return Err(invalid("tool results required before text"));
                    }
                    texts.push_str(string(&block["text"])?);
                }
                Some("tool_use") if role == "assistant" => {
                    if !pending.is_empty() && calls.is_empty() {
                        return Err(invalid("missing previous tool results"));
                    }
                    let call = claude_call(block)?;
                    if !pending.insert(string(&call["id"])?.to_owned()) {
                        return Err(invalid("duplicate tool call id"));
                    }
                    calls.push(call);
                }
                Some("tool_result") if role == "user" => {
                    fields(block, &["type", "tool_use_id", "content", "is_error"])?;
                    if block.get("is_error").is_some_and(|v| v != false) {
                        return Err(unsupported("error tool result semantics"));
                    }
                    if !texts.is_empty() {
                        return Err(unsupported("text preceding tool results"));
                    }
                    let id = nonempty(&block["tool_use_id"])?;
                    if !pending.remove(id) {
                        return Err(invalid("unmatched tool result"));
                    }
                    messages.push(json!({"role":"tool","tool_call_id":id,"content":text(&block["content"], false)?}));
                }
                _ => {
                    return Err(unsupported(
                        "content block (media, thinking, server tool or wrong role)",
                    ));
                }
            }
        }
        if !texts.is_empty() || !calls.is_empty() {
            let mut out = json!({"role":role,"content":if texts.is_empty() { Value::Null } else { json!(texts) }});
            if !calls.is_empty() {
                out["tool_calls"] = json!(calls);
            }
            messages.push(out);
        }
    }
    if !pending.is_empty() {
        return Err(invalid("missing tool results"));
    }
    Ok((messages, String::new()))
}
pub(super) fn translate_tools(v: &Value, openai: bool) -> Result<Value> {
    let mut tools = Vec::new();
    for tool in array(v)? {
        let f = if openai {
            fields(tool, &["type", "function"])?;
            if tool["type"] != "function" {
                return Err(unsupported("non-function tool definition"));
            }
            &tool["function"]
        } else {
            tool
        };
        fields(
            f,
            if openai {
                &["name", "description", "parameters"][..]
            } else {
                &["name", "description", "input_schema"][..]
            },
        )?;
        let schema = &f[if openai { "parameters" } else { "input_schema" }];
        object(schema)?;
        let mut out = json!({"name":nonempty(&f["name"])?});
        out[if openai { "input_schema" } else { "parameters" }] = schema.clone();
        if let Some(d) = f.get("description") {
            string(d)?;
            out["description"] = d.clone();
        }
        tools.push(if openai {
            out
        } else {
            json!({"type":"function","function":out})
        });
    }
    Ok(json!(tools))
}
pub(super) fn tool_choice(v: &Value, openai: bool) -> Result<Value> {
    if openai {
        if let Some(s) = v.as_str() {
            return match s {
                "auto" => Ok(json!({"type":"auto"})),
                "required" => Ok(json!({"type":"any"})),
                // Removing definitions guarantees no tools can be invoked.
                "none" => Ok(Value::Null),
                _ => Err(unsupported("tool_choice")),
            };
        }
        fields(v, &["type", "function"])?;
        if v["type"] != "function" {
            return Err(unsupported("tool_choice type"));
        }
        fields(&v["function"], &["name"])?;
        Ok(json!({"type":"tool","name":nonempty(&v["function"]["name"])?}))
    } else {
        fields(v, &["type", "name"])?;
        match string(&v["type"])? {
            "auto" | "any" | "none" if v.get("name").is_none() => {
                Ok(json!(match string(&v["type"])? {
                    "any" => "required",
                    "none" => "none",
                    _ => "auto",
                }))
            }
            "tool" => Ok(json!({"type":"function","function":{"name":nonempty(&v["name"])?}})),
            _ => Err(unsupported("tool_choice type or fields")),
        }
    }
}
