//! Transactional local replay history. Only a successful terminal commits a turn.
use anyhow::{Result, ensure};
use serde_json::{Value, json};

#[derive(Default)]
pub(super) struct History {
    request: Option<Value>,
    output: Vec<Value>,
    response_id: String,
    valid: bool,
    pub native: bool,
}

impl History {
    pub fn hydrate(&self, mut frame: Value, limit: usize) -> Result<Value> {
        let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
        ensure!(
            matches!(kind, "response.create" | "response.append"),
            "unsupported request type"
        );
        ensure!(
            kind != "response.append" || self.request.is_some(),
            "append requires a prior response"
        );
        let previous = match frame.get("previous_response_id") {
            None | Some(Value::Null) => "",
            Some(Value::String(id)) => id.as_str(),
            _ => anyhow::bail!("invalid previous_response_id"),
        };
        let input = match frame.get("input") {
            None if self.request.is_none() => Vec::new(),
            Some(Value::Array(items)) => items.clone(),
            _ => anyhow::bail!("input must be an array"),
        };
        let explicit = !previous.is_empty();
        if explicit && !self.native {
            ensure!(
                self.valid && previous == self.response_id,
                "previous_response_id is not available in this session"
            );
        }
        let replacement = !explicit
            && input.iter().any(|item| {
                item.get("role").and_then(Value::as_str) == Some("assistant")
                    || matches!(
                        item.get("type").and_then(Value::as_str),
                        Some("function_call" | "custom_tool_call" | "compaction")
                    )
            });
        let mut merged = input.clone();
        if self.request.is_some() && !replacement && !self.native {
            ensure!(
                self.valid,
                "prior response cannot be replayed; send a full replacement transcript"
            );
            let mut history = self
                .request
                .as_ref()
                .and_then(|r| r.get("input"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            history.extend(self.output.clone());
            // A cumulative client transcript must not be appended to itself.
            if !input.starts_with(&history) {
                if input
                    .first()
                    .is_some_and(|item| history.first() == Some(item))
                {
                    anyhow::bail!("ambiguous partial cumulative input");
                }
                history.extend(input);
                merged = history;
            }
        }
        let object = frame
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("request must be an object"))?;
        object.remove("type");
        if self.native && !replacement {
            if !explicit && self.valid {
                object.insert(
                    "previous_response_id".into(),
                    self.response_id.clone().into(),
                );
            }
        } else {
            object.remove("previous_response_id");
        }
        object.remove("event_id");
        for field in ["model", "instructions"] {
            if !object.contains_key(field)
                && let Some(value) = self.request.as_ref().and_then(|r| r.get(field))
            {
                object.insert(field.into(), value.clone());
            }
        }
        ensure!(
            object
                .get("model")
                .and_then(Value::as_str)
                .is_some_and(|m| !m.trim().is_empty()),
            "model required"
        );
        object.insert("input".into(), json!(merged));
        object.insert("stream".into(), true.into());
        ensure!(
            serde_json::to_vec(&frame)?.len() <= limit,
            "session history exceeds limit"
        );
        Ok(frame)
    }

    pub fn commit(&mut self, request: Value, event: &Value, limit: usize) -> Result<()> {
        self.valid = false;
        let response = event
            .get("response")
            .ok_or_else(|| anyhow::anyhow!("terminal response missing"))?;
        let id = response
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow::anyhow!("terminal response id missing"))?;
        let output = response
            .get("output")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("terminal output missing"))?;
        ensure!(
            serde_json::to_vec(&request)?
                .len()
                .saturating_add(serde_json::to_vec(output)?.len())
                <= limit,
            "session history exceeds limit"
        );
        self.request = Some(request);
        self.output = output.clone();
        self.response_id = id.to_owned();
        self.valid = true;
        Ok(())
    }

    pub fn invalidate(&mut self) {
        self.valid = false;
    }
}

pub(super) fn control(frame: &Value, active_id: Option<&str>) -> bool {
    matches!(
        frame.get("type").and_then(Value::as_str),
        Some("response.interrupt" | "response.cancel")
    ) && frame
        .get("response_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty() && Some(id) == active_id)
}

/// Classify an upstream terminal event without letting an overlapping response
/// close the currently streamed turn. Failures may omit an id, in which case
/// the only safe attribution is the active response.
pub(super) fn terminal_response_id<'a>(
    event: &'a Value,
    active_id: Option<&'a str>,
) -> Option<&'a str> {
    let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
    let failure = matches!(kind, "error" | "response.failed");
    if !failure
        && !matches!(
            kind,
            "response.completed" | "response.done" | "response.incomplete" | "response.cancelled"
        )
    {
        return None;
    }
    let nested = event
        .pointer("/response/id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let explicit = event
        .get("response_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    if failure {
        explicit.or(nested).or(active_id)
    } else {
        nested
    }
}

pub(super) fn terminal_owns_active(event: &Value, active_id: Option<&str>) -> bool {
    terminal_response_id(event, active_id).is_some_and(|id| Some(id) == active_id)
}

#[cfg(test)]
mod native_tests {
    use super::*;

    #[test]
    fn native_chain_preserves_incremental_ids_and_unknown_first_ids() {
        let mut history = History {
            native: true,
            ..Default::default()
        };
        let first = history.hydrate(json!({"type":"response.create","model":"m","previous_response_id":"upstream-id","input":[]}), 10000).unwrap();
        assert_eq!(first["previous_response_id"], "upstream-id");
        history
            .commit(
                first,
                &json!({"response":{"id":"r1","output":[{"role":"assistant","content":"answer"}]}}),
                10000,
            )
            .unwrap();
        let next = history
            .hydrate(
                json!({"type":"response.create","input":[{"role":"user","content":"next"}]}),
                10000,
            )
            .unwrap();
        assert_eq!(next["previous_response_id"], "r1");
        assert_eq!(next["input"].as_array().unwrap().len(), 1);
        let replacement = history
            .hydrate(
                json!({"type":"response.create","input":[{"role":"assistant","content":"full"}]}),
                10000,
            )
            .unwrap();
        assert!(replacement.get("previous_response_id").is_none());
    }
}

#[cfg(test)]
#[path = "ws_state_tests.rs"]
mod tests;
