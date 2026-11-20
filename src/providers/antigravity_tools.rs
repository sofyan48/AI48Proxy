//! Declaration-only schema conversion and conservative synthetic tool replay.
use super::*;
use std::collections::HashMap;

const BYPASS: &str = "skip_thought_signature_validator";
fn signature(value: &str) -> bool {
    matches!(
        value,
        "skip_thought_signature_validator" | "context_engineering_is_the_way_to_go"
    )
}

pub(super) fn normalize(body: &mut Value, model: &str) -> Result<()> {
    let placeholder = model.contains("claude")
        || model.contains("gemini-3-pro")
        || model.contains("gemini-3.1-pro");
    if let Some(system) = body.get("systemInstruction") {
        let parts = system
            .get("parts")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("invalid systemInstruction"))?;
        ensure!(
            parts.iter().all(|p| p
                .as_object()
                .is_some_and(|p| p.len() == 1 && p.get("text").is_some_and(Value::is_string))),
            "unsupported Antigravity system replay"
        );
    }
    let mut names = Vec::new();
    if let Some(tools) = body.get_mut("tools") {
        let tools = tools
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("invalid Antigravity tools"))?;
        ensure!(!tools.is_empty(), "empty Antigravity tools unsupported");
        for tool in tools {
            let tool = tool
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("invalid tool"))?;
            ensure!(
                tool.len() == 1 && tool.contains_key("functionDeclarations"),
                "unsupported Antigravity built-in tool"
            );
            let declarations = tool["functionDeclarations"]
                .as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("invalid function declarations"))?;
            ensure!(!declarations.is_empty(), "empty function declarations");
            for declaration in declarations {
                let declaration = declaration
                    .as_object_mut()
                    .ok_or_else(|| anyhow::anyhow!("invalid function declaration"))?;
                ensure!(
                    declaration.keys().all(|k| [
                        "name",
                        "description",
                        "parameters",
                        "parametersJsonSchema"
                    ]
                    .contains(&k.as_str())),
                    "unsupported function declaration field"
                );
                let name = declaration
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("missing function name"))?
                    .to_owned();
                ensure!(!names.contains(&name), "duplicate function declaration");
                names.push(name);
                ensure!(
                    !(declaration.contains_key("parameters")
                        && declaration.contains_key("parametersJsonSchema")),
                    "conflicting parameter schemas"
                );
                let schema = declaration
                    .remove("parametersJsonSchema")
                    .or_else(|| declaration.remove("parameters"))
                    .unwrap_or_else(|| serde_json::json!({"type":"object","properties":{}}));
                declaration.insert(
                    "parameters".into(),
                    crate::protocol::gemini::schema::clean(&schema, true, placeholder)?,
                );
            }
        }
    }
    if let Some(config) = body.get("toolConfig") {
        let config = config
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("invalid toolConfig"))?;
        ensure!(config.len() == 1, "unsupported toolConfig field");
        let calling = config
            .get("functionCallingConfig")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow::anyhow!("invalid functionCallingConfig"))?;
        ensure!(
            calling
                .keys()
                .all(|k| ["mode", "allowedFunctionNames"].contains(&k.as_str())),
            "unsupported functionCallingConfig field"
        );
        let mode = calling
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("AUTO");
        ensure!(
            ["AUTO", "NONE", "ANY", "VALIDATED"].contains(&mode),
            "unsupported tool calling mode"
        );
        ensure!(
            !matches!(mode, "ANY" | "VALIDATED") || !names.is_empty(),
            "tool calling mode requires declarations"
        );
        // Go forces VALIDATED for Claude. Do not silently turn NONE/ANY/named into AUTO-like behavior.
        ensure!(
            !model.contains("claude") || mode == "VALIDATED",
            "Antigravity Claude tool_choice override unsupported (requires VALIDATED)"
        );
        if let Some(allowed) = calling.get("allowedFunctionNames") {
            let allowed = allowed
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("invalid allowedFunctionNames"))?;
            ensure!(
                mode == "ANY" && !allowed.is_empty(),
                "allowedFunctionNames requires ANY"
            );
            ensure!(
                allowed
                    .iter()
                    .all(|n| n.as_str().is_some_and(|s| names.iter().any(|v| v == s))),
                "undeclared allowed function"
            );
        }
    }
    let mut pending = HashMap::<String, String>::new();
    for content in body["contents"].as_array_mut().unwrap() {
        let role = content
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        ensure!(
            pending.is_empty() || role == "user",
            "tool results must follow model tool calls"
        );
        let parts = content
            .get_mut("parts")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| anyhow::anyhow!("invalid Antigravity content parts"))?;
        let mut first = true;
        for part in parts {
            let part = part
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("invalid Antigravity part"))?;
            ensure!(
                part.keys().all(|k| [
                    "text",
                    "thought",
                    "thoughtSignature",
                    "thought_signature",
                    "functionCall",
                    "functionResponse"
                ]
                .contains(&k.as_str())),
                "unsupported Antigravity replay part"
            );
            ensure!(
                part.get("thought").is_none_or(|v| v == false),
                "Antigravity reasoning replay unsupported"
            );
            ensure!(
                !(part.contains_key("thoughtSignature") && part.contains_key("thought_signature")),
                "ambiguous thought signature"
            );
            let sig = part
                .remove("thoughtSignature")
                .or_else(|| part.remove("thought_signature"));
            let call = part.get("functionCall");
            let response = part.get("functionResponse");
            ensure!(
                call.is_none() || response.is_none(),
                "interleaved function call and result"
            );
            if let Some(call) = call {
                ensure!(
                    role == "model" && !part.contains_key("text"),
                    "invalid function call role/part"
                );
                let call = call
                    .as_object()
                    .ok_or_else(|| anyhow::anyhow!("invalid functionCall"))?;
                ensure!(
                    call.keys()
                        .all(|k| ["id", "name", "args"].contains(&k.as_str())),
                    "unsupported functionCall field"
                );
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("missing functionCall name"))?;
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("Antigravity replay requires functionCall id")
                    })?;
                ensure!(
                    call.get("args").is_some_and(Value::is_object),
                    "invalid functionCall args"
                );
                ensure!(!pending.contains_key(id), "duplicate functionCall id");
                if let Some(sig) = &sig {
                    ensure!(
                        !model.contains("claude")
                            && sig.as_str().is_some_and(
                                |s| signature(s) || super::signature::function_signature(s)
                            )
                            || sig.as_str().is_some_and(signature),
                        "Antigravity provider-issued signature replay unsupported"
                    );
                }
                pending.insert(id.to_owned(), name.to_owned());
                if let Some(sig) = sig
                    .as_ref()
                    .and_then(Value::as_str)
                    .filter(|s| !signature(s))
                {
                    part.insert(
                        "thoughtSignature".into(),
                        sig.strip_prefix("google#").unwrap_or(sig).into(),
                    );
                } else if first {
                    part.insert("thoughtSignature".into(), BYPASS.into());
                }
                first = false;
            } else if let Some(response) = response {
                ensure!(
                    role == "user" && first && sig.is_none() && !part.contains_key("text"),
                    "invalid functionResponse replay"
                );
                let response = response
                    .as_object()
                    .ok_or_else(|| anyhow::anyhow!("invalid functionResponse"))?;
                ensure!(
                    response
                        .keys()
                        .all(|k| ["id", "name", "response"].contains(&k.as_str())),
                    "unsupported functionResponse field"
                );
                let id = response
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing response id"))?;
                let name = pending
                    .remove(id)
                    .ok_or_else(|| anyhow::anyhow!("unknown functionResponse id"))?;
                ensure!(
                    response.get("name").and_then(Value::as_str) == Some(name.as_str())
                        && response.get("response").is_some_and(Value::is_object),
                    "invalid functionResponse pairing"
                );
            } else {
                ensure!(sig.is_none(), "Antigravity signed text replay unsupported");
                ensure!(pending.is_empty(), "tool results must follow tool calls");
                ensure!(
                    part.get("text").is_some_and(Value::is_string),
                    "unsupported empty replay part"
                );
                first = true;
            }
        }
    }
    ensure!(pending.is_empty(), "unanswered Antigravity tool calls");
    Ok(())
}
