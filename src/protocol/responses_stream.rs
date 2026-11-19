use super::{Result, finish_reason, invalid, nonempty, string, usage};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const MAX_EVENT: usize = 32 * 1024 * 1024;
#[derive(Default)]
struct Tool {
    index: usize,
    arguments: bool,
    done: bool,
}

/// Incremental SSE decoder. Completion, not transport EOF, terminates a response.
/// Tool indices are assigned by announcement order, not Responses output indices.
pub struct StreamTranslator {
    line: Vec<u8>,
    data: Vec<u8>,
    id: String,
    model: String,
    created: Value,
    tools: BTreeMap<u64, Tool>,
    text_parts: std::collections::BTreeSet<(u64, u64)>,
    include_usage: bool,
    complete: bool,
}
impl StreamTranslator {
    pub fn new(model: &str, include_usage: bool) -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            id: String::new(),
            model: model.into(),
            created: json!(0),
            tools: BTreeMap::new(),
            text_parts: std::collections::BTreeSet::new(),
            include_usage,
            complete: false,
        }
    }
    pub fn finish(&self) -> Result<()> {
        if self.complete {
            Ok(())
        } else {
            Err(invalid("Responses stream incomplete"))
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for &byte in bytes {
            if self.complete {
                break;
            }
            if self.line.len().saturating_add(self.data.len()) >= MAX_EVENT {
                return Err(invalid("Responses event too large"));
            }
            if byte != b'\n' {
                self.line.push(byte);
                continue;
            }
            if self.line.last() == Some(&b'\r') {
                self.line.pop();
            }
            let line = std::mem::take(&mut self.line);
            if line.is_empty() {
                let data = std::mem::take(&mut self.data);
                if data.is_empty() {
                    continue;
                }
                let event: Value = serde_json::from_slice(&data)
                    .map_err(|_| invalid("invalid Responses event"))?;
                self.event(&event, &mut out)?;
            } else if let Some(data) = line.strip_prefix(b"data:") {
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data
                    .extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
            }
        }
        Ok(out)
    }
    fn emit(&self, delta: Value, finish: Value, out: &mut Vec<u8>) {
        self.write(json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]}),out);
    }
    fn write(&self, value: Value, out: &mut Vec<u8>) {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(value.to_string().as_bytes());
        out.extend_from_slice(b"\n\n");
    }
    fn tool(&mut self, event: &Value, item: &Value, done: bool, out: &mut Vec<u8>) -> Result<()> {
        let key = event["output_index"]
            .as_u64()
            .ok_or_else(|| invalid("tool output index missing"))?;
        if !self.tools.contains_key(&key) {
            if self.tools.len() >= 4096 {
                return Err(invalid("too many response tool calls"));
            }
            let index = self.tools.len();
            let arguments = if done {
                string(&item["arguments"])?
            } else {
                ""
            };
            self.emit(json!({"role":"assistant","tool_calls":[{"index":index,"id":nonempty(&item["call_id"])?,
                "type":"function","function":{"name":nonempty(&item["name"])?,"arguments":arguments}}]}),Value::Null,out);
            self.tools.insert(
                key,
                Tool {
                    index,
                    arguments: !arguments.is_empty(),
                    done,
                },
            );
        }
        if done {
            self.arguments(key, string(&item["arguments"])?, true, out)?;
        }
        Ok(())
    }
    fn arguments(&mut self, key: u64, args: &str, done: bool, out: &mut Vec<u8>) -> Result<()> {
        // Go ignores unannounced arguments; output_item.done supplies the full call.
        let Some(tool) = self.tools.get_mut(&key) else {
            return Ok(());
        };
        if tool.done || (done && tool.arguments) {
            tool.done |= done;
            return Ok(());
        }
        tool.arguments |= !args.is_empty();
        tool.done = done;
        let index = tool.index;
        if !args.is_empty() {
            self.emit(
                json!({"tool_calls":[{"index":index,"function":{"arguments":args}}]}),
                Value::Null,
                out,
            );
        }
        Ok(())
    }
    fn event(&mut self, event: &Value, out: &mut Vec<u8>) -> Result<()> {
        let kind = string(&event["type"])?;
        if let Some(response) = event.get("response") {
            if let Some(id) = response["id"].as_str() {
                self.id = id.into();
            }
            if let Some(created) = response.get("created_at") {
                self.created = created.clone();
            }
        }
        match kind {
            "response.created" => {
                nonempty(&event["response"]["id"])?;
                self.emit(json!({"role":"assistant","content":""}), Value::Null, out);
            }
            "response.output_text.delta"
            | "response.refusal.delta"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta" => {
                if kind == "response.output_text.delta" {
                    self.text_parts.insert((
                        event["output_index"].as_u64().unwrap_or(0),
                        event["content_index"].as_u64().unwrap_or(0),
                    ));
                }
                let key = match kind {
                    "response.output_text.delta" => "content",
                    "response.refusal.delta" => "refusal",
                    _ => "reasoning_content",
                };
                let mut delta = json!({"role":"assistant"});
                delta[key] = json!(string(&event["delta"])?);
                self.emit(delta, Value::Null, out);
            }
            "response.output_text.done" => {
                let key = (
                    event["output_index"].as_u64().unwrap_or(0),
                    event["content_index"].as_u64().unwrap_or(0),
                );
                if self.text_parts.insert(key) {
                    self.emit(
                        json!({"role":"assistant","content":string(&event["text"])?}),
                        Value::Null,
                        out,
                    );
                }
            }
            "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                self.emit(json!({"reasoning_content":"\n\n"}), Value::Null, out)
            }
            "response.output_item.added" | "response.output_item.done"
                if event["item"]["type"] == "function_call" =>
            {
                self.tool(event, &event["item"], kind.ends_with("done"), out)?;
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                let key = event["output_index"]
                    .as_u64()
                    .ok_or_else(|| invalid("tool output index missing"))?;
                let done = kind.ends_with("done");
                self.arguments(
                    key,
                    string(&event[if done { "arguments" } else { "delta" }])?,
                    done,
                    out,
                )?;
            }
            "response.completed" | "response.done" | "response.incomplete" => {
                let response = &event["response"];
                nonempty(&response["id"])?;
                if kind != "response.incomplete" && response["status"] != "completed" {
                    return Err(invalid("Responses did not complete"));
                }
                // Some upstreams emit completed tool items only in the terminal output.
                if let Some(items) = response["output"].as_array() {
                    for (index, item) in items.iter().enumerate() {
                        if item["type"] == "message"
                            && let Some(parts) = item["content"].as_array()
                        {
                            for (content_index, part) in parts.iter().enumerate() {
                                if part["type"] == "output_text"
                                    && self.text_parts.insert((index as u64, content_index as u64))
                                {
                                    self.emit(json!({"role":"assistant","content":string(&part["text"])?}), Value::Null, out);
                                }
                            }
                        }
                        if item["type"] == "function_call" {
                            self.tool(&json!({"output_index":index}), item, true, out)?;
                        }
                    }
                }
                let finish = finish_reason(response, !self.tools.is_empty())?;
                self.emit(json!({}), json!(finish), out);
                if self.include_usage
                    && let Some(value) = response.get("usage")
                {
                    self.write(json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,
                        "choices":[],"usage":usage(value)}),out);
                }
                out.extend_from_slice(b"data: [DONE]\n\n");
                self.complete = true;
            }
            "error" | "response.failed" => return Err(invalid("Responses upstream failed")),
            _ => (),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(translator: &mut StreamTranslator, event: Value) -> Vec<Value> {
        let wire = format!("data: {event}\r\n\r\n");
        let mut output = Vec::new();
        for byte in wire.bytes() {
            output.extend(translator.feed(&[byte]).unwrap());
        }
        String::from_utf8(output)
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).unwrap())
            .collect()
    }

    #[test]
    fn unknown_arguments_wait_for_complete_tool_fallback() {
        let item = json!({"type":"function_call","call_id":"call_fixture",
            "name":"read_file","arguments":"{\"path\":\"README.md\"}"});
        for terminal_only in [false, true] {
            let mut translator = StreamTranslator::new("alias", true);
            for kind in [
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
            ] {
                assert!(
                    feed(
                        &mut translator,
                        json!({"type":kind,"output_index":0,
                    "delta":"ignored","arguments":item["arguments"]})
                    )
                    .is_empty()
                );
            }
            let mut chunks = Vec::new();
            if !terminal_only {
                chunks.extend(feed(
                    &mut translator,
                    json!({"type":"response.output_item.done",
                    "output_index":0,"item":item}),
                ));
            }
            chunks.extend(feed(
                &mut translator,
                json!({"type":"response.done","response":{
                "id":"resp_fixture","status":"completed","output":[item],
                "usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}}}),
            ));
            translator.finish().unwrap();
            let calls: Vec<_> = chunks
                .iter()
                .filter_map(|c| c.pointer("/choices/0/delta/tool_calls/0"))
                .collect();
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0],
                &json!({"index":0,"id":"call_fixture","type":"function",
                "function":{"name":"read_file","arguments":item["arguments"]}})
            );
            assert_eq!(
                chunks[chunks.len() - 2]["choices"][0]["finish_reason"],
                "tool_calls"
            );
            assert_eq!(chunks.last().unwrap()["usage"]["total_tokens"], 5);
        }
    }

    #[test]
    fn known_argument_sequences_never_duplicate_full_arguments() {
        let item = json!({"type":"function_call","call_id":"call_fixture",
            "name":"read_file","arguments":"{\"path\":\"README.md\"}"});
        for deltas in [false, true] {
            let mut translator = StreamTranslator::new("alias", false);
            let mut chunks = feed(
                &mut translator,
                json!({"type":"response.output_item.added","output_index":0,"item":item}),
            );
            if deltas {
                for delta in ["{\"path\":", "\"README.md\"}"] {
                    chunks.extend(feed(&mut translator, json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":delta})));
                }
            }
            for _ in 0..2 {
                chunks.extend(feed(&mut translator, json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":item["arguments"]})));
                chunks.extend(feed(
                    &mut translator,
                    json!({"type":"response.output_item.done","output_index":0,"item":item}),
                ));
            }
            chunks.extend(feed(
                &mut translator,
                json!({"type":"response.done","response":{
                "id":"resp_fixture","status":"completed","output":[item]}}),
            ));
            translator.finish().unwrap();
            let calls: Vec<_> = chunks
                .iter()
                .filter_map(|c| c.pointer("/choices/0/delta/tool_calls/0"))
                .collect();
            assert_eq!(calls.iter().filter(|c| c.get("id").is_some()).count(), 1);
            let arguments: String = calls
                .iter()
                .filter_map(|c| c["function"]["arguments"].as_str())
                .collect();
            assert_eq!(arguments, item["arguments"].as_str().unwrap());
        }
    }

    #[test]
    fn done_alias_requires_success_and_emits_done_once() {
        for status in ["failed", "in_progress", "incomplete", "cancelled"] {
            let mut translator = StreamTranslator::new("alias", false);
            let event = json!({"type":"response.done","response":{"id":"resp_fixture","status":status,
                "incomplete_details":{"reason":"max_output_tokens"},"output":[]}});
            assert!(
                translator
                    .feed(format!("data: {event}\n\n").as_bytes())
                    .is_err()
            );
            assert!(translator.finish().is_err());
        }
        let mut translator = StreamTranslator::new("alias", false);
        let wire = format!(
            "data: {}\n\n",
            json!({"type":"response.done","response":{
            "id":"resp_fixture","status":"completed","output":[]}})
        );
        let output = translator.feed(wire.repeat(2).as_bytes()).unwrap();
        assert_eq!(
            String::from_utf8(output)
                .unwrap()
                .matches("data: [DONE]")
                .count(),
            1
        );
        translator.finish().unwrap();
    }
}
