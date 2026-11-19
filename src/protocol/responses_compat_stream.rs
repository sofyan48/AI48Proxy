use super::{Result, invalid, object, response_status, responses_usage, string, unsupported};
use serde_json::{Value, json};
use std::collections::BTreeMap;
const LIMIT: usize = 32 * 1024 * 1024;

struct Item {
    value: Value,
    announced: bool,
}
/// Chat SSE -> Responses lifecycle. Finish waits for [DONE] so a trailing
/// usage-only Chat chunk is included in the authoritative terminal response.
pub struct ChatStreamTranslator {
    line: Vec<u8>,
    data: Vec<u8>,
    id: String,
    model: String,
    created: Value,
    sequence: u64,
    started: bool,
    complete: bool,
    finish_reason: Option<String>,
    items: Vec<Item>,
    tools: BTreeMap<u64, usize>,
    message: Option<usize>,
    reasoning: Option<usize>,
    usage: Option<Value>,
    retained: usize,
}
impl ChatStreamTranslator {
    pub fn new(model: &str) -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            id: String::new(),
            model: model.into(),
            created: json!(0),
            sequence: 0,
            started: false,
            complete: false,
            finish_reason: None,
            items: Vec::new(),
            tools: BTreeMap::new(),
            message: None,
            reasoning: None,
            usage: None,
            retained: 0,
        }
    }
    pub fn complete(&self) -> bool {
        self.complete
    }
    pub fn finish(&self) -> Result<()> {
        if self.complete {
            Ok(())
        } else {
            Err(invalid("Chat SSE ended before DONE"))
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for &byte in bytes {
            if self.complete {
                break;
            }
            if self.line.len().saturating_add(self.data.len()) >= LIMIT {
                return Err(invalid("Chat SSE event too large"));
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
                if data == b"[DONE]" {
                    self.terminal(&mut out)?;
                } else {
                    let event: Value = serde_json::from_slice(&data)
                        .map_err(|_| invalid("invalid Chat SSE JSON"))?;
                    self.chunk(&event, &mut out)?;
                }
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
    fn emit(&mut self, kind: &str, mut value: Value, out: &mut Vec<u8>) {
        value["type"] = json!(kind);
        value["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        out.extend_from_slice(format!("event: {kind}\ndata: {value}\n\n").as_bytes());
    }
    fn envelope(&self, status: &str, output: Vec<Value>, incomplete: Value) -> Value {
        let mut response = json!({"id":self.id,"object":"response","created_at":self.created,"model":self.model,
            "status":status,"error":null,"incomplete_details":incomplete,"output":output});
        if let Some(usage) = &self.usage {
            response["usage"] = usage.clone();
        }
        response
    }
    fn add(&mut self, value: Value, announced: bool, out: &mut Vec<u8>) -> Result<usize> {
        if self.items.len() >= 4096 {
            return Err(invalid("too many response output items"));
        }
        let index = self.items.len();
        if announced {
            self.emit(
                "response.output_item.added",
                json!({"output_index":index,"item":value}),
                out,
            );
        }
        self.items.push(Item { value, announced });
        Ok(index)
    }
    fn text(&mut self, text: &str, reasoning: bool, out: &mut Vec<u8>) -> Result<()> {
        let current = if reasoning {
            self.reasoning
        } else {
            self.message
        };
        let index = if let Some(index) = current {
            index
        } else {
            let index = if reasoning {
                self.add(
                    json!({"id":format!("rs_{}",self.id),"type":"reasoning","summary":[]}),
                    true,
                    out,
                )?
            } else {
                self.add(json!({"id":format!("msg_{}",self.id),"type":"message","status":"in_progress","role":"assistant","content":[]}),true,out)?
            };
            let id = self.items[index].value["id"].clone();
            if reasoning {
                self.reasoning = Some(index);
                self.emit("response.reasoning_summary_part.added",json!({"item_id":id,"output_index":index,"summary_index":0,"part":{"type":"summary_text","text":""}}),out);
                self.items[index].value["summary"] = json!([{"type":"summary_text","text":""}]);
            } else {
                self.message = Some(index);
                self.emit("response.content_part.added",json!({"item_id":id,"output_index":index,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),out);
                self.items[index].value["content"] =
                    json!([{"type":"output_text","text":"","annotations":[]}]);
            }
            index
        };
        let key = if reasoning { "summary" } else { "content" };
        let previous = self.items[index].value[key][0]["text"]
            .as_str()
            .unwrap_or("");
        self.items[index].value[key][0]["text"] = json!(format!("{previous}{text}"));
        let id = self.items[index].value["id"].clone();
        let mut event = json!({"item_id":id,"output_index":index,"delta":text});
        event[if reasoning {
            "summary_index"
        } else {
            "content_index"
        }] = json!(0);
        self.emit(
            if reasoning {
                "response.reasoning_summary_text.delta"
            } else {
                "response.output_text.delta"
            },
            event,
            out,
        );
        Ok(())
    }
    fn chunk(&mut self, chunk: &Value, out: &mut Vec<u8>) -> Result<()> {
        object(chunk)?;
        if chunk.get("error").is_some() {
            return Err(invalid("Chat upstream error"));
        }
        if !self.started {
            let id = chunk["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid("Chat chunk id missing"))?;
            self.id = id.into();
            self.created = chunk.get("created").cloned().unwrap_or(json!(0));
            self.started = true;
            for kind in ["response.created", "response.in_progress"] {
                self.emit(
                    kind,
                    json!({"response":self.envelope("in_progress",Vec::new(),Value::Null)}),
                    out,
                );
            }
        }
        self.retained = self.retained.saturating_add(chunk.to_string().len());
        if self.retained > LIMIT {
            return Err(invalid("Chat response exceeds aggregation limit"));
        }
        if let Some(usage) = chunk.get("usage").filter(|v| !v.is_null()) {
            self.usage = Some(responses_usage(usage));
        }
        let choices = chunk["choices"]
            .as_array()
            .ok_or_else(|| invalid("Chat choices missing"))?;
        for choice in choices {
            if choice["index"].as_u64() != Some(0) {
                return Err(unsupported("multiple Chat choices"));
            }
            let delta = &choice["delta"];
            for (key, reasoning) in [("reasoning_content", true), ("content", false)] {
                if let Some(text) = delta.get(key).filter(|v| !v.is_null()) {
                    let text = string(text)?;
                    if !text.is_empty() {
                        self.text(text, reasoning, out)?;
                    }
                }
            }
            if delta.get("refusal").is_some_and(|v| !v.is_null()) {
                return Err(unsupported("streamed Chat refusal"));
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                for call in calls {
                    let key = call["index"]
                        .as_u64()
                        .ok_or_else(|| invalid("tool index missing"))?;
                    let index = if let Some(index) = self.tools.get(&key) {
                        *index
                    } else {
                        let index = self.add(json!({"id":format!("fc_{}_{}",self.id,key),"type":"function_call","status":"in_progress","call_id":"","name":"","arguments":""}),false,out)?;
                        self.tools.insert(key, index);
                        index
                    };
                    if let Some(id) = call["id"].as_str() {
                        let old = self.items[index].value["call_id"].as_str().unwrap_or("");
                        if !old.is_empty() && old != id {
                            return Err(invalid("conflicting tool identity"));
                        }
                        self.items[index].value["call_id"] = json!(id);
                    }
                    for field in ["name", "arguments"] {
                        if let Some(fragment) = call["function"][field].as_str() {
                            let previous = self.items[index].value[field].as_str().unwrap_or("");
                            self.items[index].value[field] = json!(format!("{previous}{fragment}"));
                        }
                    }
                    if !self.items[index].announced
                        && self.items[index].value["call_id"]
                            .as_str()
                            .is_some_and(|s| !s.is_empty())
                        && self.items[index].value["name"]
                            .as_str()
                            .is_some_and(|s| !s.is_empty())
                    {
                        let mut item = self.items[index].value.clone();
                        item["arguments"] = json!("");
                        self.emit(
                            "response.output_item.added",
                            json!({"output_index":index,"item":item}),
                            out,
                        );
                        self.items[index].announced = true;
                        let args = self.items[index].value["arguments"].clone();
                        if args != "" {
                            self.emit("response.function_call_arguments.delta",json!({"item_id":self.items[index].value["id"],"output_index":index,"delta":args}),out);
                        }
                    } else if self.items[index].announced
                        && let Some(args) = call["function"]["arguments"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                    {
                        self.emit("response.function_call_arguments.delta",json!({"item_id":self.items[index].value["id"],"output_index":index,"delta":args}),out);
                    }
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                response_status(reason)?;
                self.finish_reason = Some(reason.into());
            }
        }
        Ok(())
    }
    fn terminal(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let reason = self
            .finish_reason
            .as_deref()
            .ok_or_else(|| invalid("Chat DONE without finish reason"))?;
        let (status, incomplete) = response_status(reason)?;
        for index in 0..self.items.len() {
            if !self.items[index].announced {
                return Err(invalid("tool identity incomplete"));
            }
            let item = &mut self.items[index].value;
            if item["type"] != "reasoning" {
                item["status"] = json!(status);
            }
            let item = item.clone();
            let common = json!({"item_id":item["id"],"output_index":index});
            match item["type"].as_str() {
                Some("message") => {
                    let mut event = common.clone();
                    event["content_index"] = json!(0);
                    event["text"] = item["content"][0]["text"].clone();
                    self.emit("response.output_text.done", event, out);
                    let mut event = common.clone();
                    event["content_index"] = json!(0);
                    event["part"] = item["content"][0].clone();
                    self.emit("response.content_part.done", event, out);
                }
                Some("reasoning") => {
                    let mut event = common.clone();
                    event["summary_index"] = json!(0);
                    event["text"] = item["summary"][0]["text"].clone();
                    self.emit("response.reasoning_summary_text.done", event, out);
                    let mut event = common.clone();
                    event["summary_index"] = json!(0);
                    event["part"] = item["summary"][0].clone();
                    self.emit("response.reasoning_summary_part.done", event, out);
                }
                Some("function_call") => {
                    let mut event = common;
                    event["arguments"] = item["arguments"].clone();
                    self.emit("response.function_call_arguments.done", event, out);
                }
                _ => (),
            }
            self.emit(
                "response.output_item.done",
                json!({"output_index":index,"item":item}),
                out,
            );
        }
        let output = self.items.iter().map(|item| item.value.clone()).collect();
        self.emit(
            if status == "completed" {
                "response.completed"
            } else {
                "response.incomplete"
            },
            json!({"response":self.envelope(status,output,incomplete)}),
            out,
        );
        self.complete = true;
        Ok(())
    }
}
