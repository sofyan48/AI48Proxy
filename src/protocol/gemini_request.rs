use super::{array, name, object, text};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::HashMap;

fn text_parts(content: &Value) -> Result<Vec<Value>> {
    if content.is_null() {
        return Ok(Vec::new());
    }
    if let Some(value) = content.as_str() {
        return Ok(vec![json!({"text":value})]);
    }
    let mut out = Vec::new();
    for part in array(content)? {
        ensure!(
            part["type"] == "text",
            "unsupported OpenAI content part (only text is supported)"
        );
        out.push(json!({"text":text(&part["text"])?}));
    }
    Ok(out)
}

/// Convert OpenAI Chat text/function-tool requests to native generateContent JSON.
/// The caller chooses the model/URL and handles stream_options locally.
pub fn chat_request(body: &Value, model: &str) -> Result<Value> {
    chat_request_inner(body, model, false)
}

/// Preserve original declaration types for the Antigravity provider cleaner.
/// Returns unwrapped native Gemini input, not a Cloud Code request envelope.
pub fn antigravity_chat_request(body: &Value, model: &str) -> Result<Value> {
    chat_request_inner(body, model, true)
}

fn chat_request_inner(body: &Value, model: &str, antigravity: bool) -> Result<Value> {
    object(body)?;
    ensure!(!model.is_empty(), "target model is required");
    for key in body.as_object().unwrap().keys() {
        ensure!(
            [
                "model",
                "messages",
                "temperature",
                "top_p",
                "top_k",
                "max_tokens",
                "max_completion_tokens",
                "n",
                "stop",
                "stream",
                "stream_options",
                "tools",
                "tool_choice",
                "response_format",
                "reasoning_effort"
            ]
            .contains(&key.as_str()),
            "unsupported OpenAI request field: {key}"
        );
    }
    if let Some(stream) = body.get("stream") {
        ensure!(stream.is_boolean(), "stream must be boolean");
    }
    if let Some(options) = body.get("stream_options") {
        object(options)?;
        for (key, value) in options.as_object().unwrap() {
            ensure!(
                key == "include_usage" && value.is_boolean(),
                "unsupported stream_options"
            );
        }
    }
    let messages = array(&body["messages"])?;
    ensure!(!messages.is_empty(), "messages must not be empty");
    let mut contents: Vec<Value> = Vec::new();
    let mut system = Vec::new();
    let mut pending = HashMap::new();
    let mut conversation = false;
    for message in messages {
        object(message)?;
        let role = text(&message["role"])?;
        for key in message.as_object().unwrap().keys() {
            ensure!(
                [
                    "role",
                    "content",
                    "name",
                    "tool_calls",
                    "tool_call_id",
                    "reasoning_content"
                ]
                .contains(&key.as_str()),
                "unsupported message field: {key}"
            );
        }
        ensure!(
            pending.is_empty() || role == "tool",
            "tool results must follow tool calls"
        );
        let mut parts = text_parts(&message["content"])?;
        if matches!(role, "system" | "developer") && !conversation && messages.len() > 1 {
            system.extend(parts);
            continue;
        }
        conversation = true;
        let target_role = match role {
            "user" => "user",
            "system" | "developer" => {
                for part in &mut parts {
                    let value = text(&part["text"])?;
                    if !value.trim().is_empty() {
                        part["text"] =
                            json!(format!("<system-reminder>\n{value}\n</system-reminder>"));
                    }
                }
                "user"
            }
            "assistant" => {
                if let Some(reasoning) = message.get("reasoning_content") {
                    parts.insert(0, json!({"text":text(reasoning)?,"thought":true}));
                }
                if let Some(calls) = message.get("tool_calls") {
                    ensure!(
                        pending.is_empty(),
                        "unanswered tool calls before assistant turn"
                    );
                    for (call_index, call) in array(calls)?.iter().enumerate() {
                        ensure!(call["type"] == "function", "unsupported tool call type");
                        let id = text(&call["id"])?;
                        ensure!(
                            !id.is_empty() && !pending.contains_key(id),
                            "duplicate or empty tool call id"
                        );
                        let function = name(&call["function"]["name"])?;
                        let args: Value =
                            serde_json::from_str(text(&call["function"]["arguments"])?)?;
                        object(&args)?;
                        pending.insert(id.to_owned(), function.to_owned());
                        let signature = call
                            .pointer("/extra_content/google/thought_signature")
                            .map(text)
                            .transpose()?;
                        let mut part =
                            json!({"functionCall":{"id":id,"name":function,"args":args}});
                        if let Some(signature) = signature.filter(|s| !s.is_empty()) {
                            // Synthetic bypasses belong only on the first parallel call.
                            if call_index == 0 || !super::is_bypass(signature) {
                                part["thoughtSignature"] = json!(signature);
                            }
                        } else if call_index == 0 {
                            part["thoughtSignature"] = json!(super::BYPASS);
                        }
                        parts.push(part);
                    }
                }
                "model"
            }
            "tool" => {
                let id = text(&message["tool_call_id"])?;
                let function = pending
                    .remove(id)
                    .ok_or_else(|| anyhow::anyhow!("unknown tool_call_id"))?;
                let result = if message["content"].is_string() {
                    message["content"].clone()
                } else {
                    json!(
                        parts
                            .iter()
                            .map(|p| p["text"].as_str().unwrap())
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                };
                parts = vec![
                    json!({"functionResponse":{"id":id,"name":function,"response":{"result":result}}}),
                ];
                "user"
            }
            _ => bail!("unsupported message role: {role}"),
        };
        ensure!(!parts.is_empty(), "empty message");
        if role == "tool"
            && contents.last().is_some_and(|v| {
                v["role"] == "user" && v["parts"][0].get("functionResponse").is_some()
            })
        {
            contents.last_mut().unwrap()["parts"]
                .as_array_mut()
                .unwrap()
                .extend(parts);
        } else {
            contents.push(json!({"role":target_role,"parts":parts}));
        }
    }
    ensure!(pending.is_empty(), "unanswered tool calls");
    ensure!(!contents.is_empty(), "no conversation contents");
    let mut out = json!({"model":model,"contents":contents});
    if !system.is_empty() {
        out["systemInstruction"] = json!({"role":"user","parts":system});
    }
    let mut config = json!({});
    ensure!(
        body.get("max_tokens").is_none() || body.get("max_completion_tokens").is_none(),
        "conflicting token limits"
    );
    for (source, target) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("top_k", "topK"),
        ("max_tokens", "maxOutputTokens"),
        ("max_completion_tokens", "maxOutputTokens"),
        ("n", "candidateCount"),
    ] {
        if let Some(value) = body.get(source) {
            ensure!(value.is_number(), "invalid numeric generation parameter");
            if !matches!(source, "temperature" | "top_p") {
                ensure!(
                    value.as_u64().is_some_and(|n| n > 0),
                    "expected positive integer"
                );
            } else {
                ensure!(value.as_f64().is_some_and(|n| n >= 0.0 && n <= if source == "top_p" {1.0} else {2.0}), "sampling parameter out of range");
            }
            config[target] = value.clone();
        }
    }
    if let Some(stops) = body.get("stop").filter(|v| !v.is_null()) {
        let stops = if stops.is_string() {
            vec![stops.clone()]
        } else {
            array(stops)?.clone()
        };
        for stop in &stops {
            text(stop)?;
        }
        config["stopSequences"] = json!(stops);
    }
    if let Some(effort) = body.get("reasoning_effort") {
        let effort = text(effort)?;
        ensure!(
            ["auto", "none", "minimal", "low", "medium", "high"].contains(&effort),
            "unsupported reasoning_effort"
        );
        config["thinkingConfig"] = if effort == "auto" {
            json!({"thinkingBudget":-1})
        } else {
            json!({"thinkingLevel":effort})
        };
    }
    if let Some(format) = body.get("response_format") {
        match text(&format["type"])? {
            "text" => (),
            "json_object" => {
                config["responseMimeType"] = json!("application/json");
            }
            "json_schema" => {
                object(&format["json_schema"]["schema"])?;
                config["responseMimeType"] = json!("application/json");
                config["responseJsonSchema"] = format["json_schema"]["schema"].clone();
            }
            _ => bail!("unsupported response_format"),
        }
    }
    let mut declared = Vec::new();
    if let Some(tools) = body.get("tools") {
        for tool in array(tools)? {
            ensure!(tool["type"] == "function", "unsupported tool type");
            let function = &tool["function"];
            let function_name = name(&function["name"])?;
            ensure!(
                !declared.contains(&function_name.to_owned()),
                "duplicate function declaration"
            );
            declared.push(function_name.to_owned());
            let schema = function
                .get("parameters")
                .cloned()
                .unwrap_or(json!({"type":"object","properties":{}}));
            object(&schema)?;
            let schema = if antigravity {
                schema
            } else {
                super::schema::clean(&schema, false, false)?
            };
            let mut declaration = json!({"name":function_name,"parametersJsonSchema":schema});
            if let Some(description) = function.get("description") {
                declaration["description"] = json!(text(description)?);
            }

            if out.get("tools").is_none() {
                out["tools"] = json!([{"functionDeclarations":[]}]);
            }
            out["tools"][0]["functionDeclarations"]
                .as_array_mut()
                .unwrap()
                .push(declaration);
        }
    }
    if let Some(choice) = body.get("tool_choice") {
        let mut calling = match choice.as_str() {
            Some("auto") => json!({"mode":"AUTO"}),
            Some("none") => json!({"mode":"NONE"}),
            Some("required") => {
                ensure!(!declared.is_empty(), "tool_choice requires tools");
                json!({"mode":"ANY"})
            }
            Some(_) => bail!("unsupported tool_choice"),
            None => {
                ensure!(choice["type"] == "function", "unsupported tool_choice");
                let selected = name(&choice["function"]["name"])?;
                ensure!(
                    declared.iter().any(|n| n == selected),
                    "tool_choice names undeclared function"
                );
                json!({"mode":"ANY","allowedFunctionNames":[selected]})
            }
        };
        out["toolConfig"] = json!({"functionCallingConfig":calling.take()});
    }
    if !config.as_object().unwrap().is_empty() {
        out["generationConfig"] = config;
    }
    Ok(out)
}
