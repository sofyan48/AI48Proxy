//! Incremental bounded SSE decoder and cross-format text/function-tool state.
use super::*;
use std::collections::BTreeMap;

pub struct StreamTranslator {
    from: Format,
    model: String,
    id: String,
    created: u64,
    pending: Vec<u8>,
    data: Vec<u8>,
    started: bool,
    pub complete: bool,
    blocks: BTreeMap<u64, u64>,
    next_block: u64,
    text_block: Option<u64>,
    input: Option<Value>,
    output: Option<Value>,
    usage: Value,
    finished_reason: bool,
    terminal: bool,
    closed: std::collections::BTreeSet<u64>,
}
impl StreamTranslator {
    pub fn new(from: Format, model: &str, created: u64) -> Self {
        Self {
            from,
            model: model.into(),
            id: String::new(),
            created,
            pending: Vec::new(),
            data: Vec::new(),
            started: false,
            complete: false,
            blocks: BTreeMap::new(),
            next_block: 0,
            text_block: None,
            input: None,
            output: None,
            usage: json!({}),
            finished_reason: false,
            terminal: false,
            closed: std::collections::BTreeSet::new(),
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for &b in bytes {
            if b != b'\n' {
                if self.pending.len() >= 256 * 1024 {
                    return Err(invalid("SSE line exceeds limit"));
                }
                self.pending.push(b);
                continue;
            }
            let mut line = std::mem::take(&mut self.pending);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                let data = std::mem::take(&mut self.data);
                if data.is_empty() {
                    continue;
                }
                if data.strip_suffix(b"\n").unwrap_or(&data) == b"[DONE]" {
                    if self.from == Format::OpenAiChat {
                        if !self.complete {
                            return Err(invalid("DONE before finish reason"));
                        }
                        self.claude_end(&mut out);
                        self.terminal = true;
                    }
                    continue;
                }
                if self.terminal {
                    return Err(invalid("event after terminal event"));
                }
                let value: Value =
                    serde_json::from_slice(&data).map_err(|_| invalid("invalid SSE JSON"))?;
                if self.from == Format::OpenAiChat {
                    self.openai(&value, &mut out)?;
                } else {
                    self.claude(&value, &mut out)?;
                }
            } else if let Some(data) = line.strip_prefix(b"data:") {
                let data = data.strip_prefix(b" ").unwrap_or(data);
                if self.data.len() + data.len() > 256 * 1024 {
                    return Err(invalid("SSE event exceeds limit"));
                }
                self.data.extend_from_slice(data);
                self.data.push(b'\n');
            }
        }
        Ok(out)
    }
    pub fn finish(&self) -> Result<()> {
        if !self.complete || !self.terminal || !self.pending.is_empty() || !self.data.is_empty() {
            return Err(invalid("incomplete SSE stream"));
        }
        Ok(())
    }
    fn event(out: &mut Vec<u8>, name: &str, value: Value) {
        if !name.is_empty() {
            out.extend_from_slice(format!("event: {name}\n").as_bytes());
        }
        out.extend_from_slice(format!("data: {value}\n\n").as_bytes());
    }
    fn chunk(&self, out: &mut Vec<u8>, delta: Value, finish: Value) {
        Self::event(
            out,
            "",
            json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]}),
        );
    }
    fn claude(&mut self, v: &Value, out: &mut Vec<u8>) -> Result<()> {
        match string(&v["type"])? {
            "ping" => (),
            "message_start" if !self.started => {
                self.id = nonempty(&v["message"]["id"])?.into();
                self.input = v.pointer("/message/usage/input_tokens").cloned();
                if let Some(u) = v.pointer("/message/usage").and_then(Value::as_object) {
                    self.usage.as_object_mut().unwrap().extend(u.clone());
                }
                self.started = true;
                self.chunk(out, json!({"role":"assistant","content":""}), Value::Null);
            }
            "content_block_start" if self.started => {
                let index = uint(&v["index"])?;
                let block = &v["content_block"];
                match string(&block["type"])? {
                    "text" => {
                        if let Some(text) = block["text"].as_str().filter(|s| !s.is_empty()) {
                            self.chunk(out, json!({"content":text}), Value::Null);
                        }
                    }
                    "tool_use" => {
                        let tool = self.blocks.len() as u64;
                        self.blocks.insert(index, tool);
                        self.chunk(out,json!({"tool_calls":[{"index":tool,"id":nonempty(&block["id"])?,"type":"function","function":{"name":nonempty(&block["name"])?,"arguments":""}}]}),Value::Null);
                    }
                    _ => return Err(unsupported("Claude stream block")),
                }
            }
            "content_block_delta" if self.started => {
                let delta = &v["delta"];
                match string(&delta["type"])? {
                    "text_delta" => {
                        self.chunk(out, json!({"content":string(&delta["text"])?}), Value::Null)
                    }
                    "input_json_delta" => {
                        let index = self
                            .blocks
                            .get(&uint(&v["index"])?)
                            .ok_or_else(|| invalid("unknown tool block"))?;
                        self.chunk(out,json!({"tool_calls":[{"index":index,"function":{"arguments":string(&delta["partial_json"])?}}]}),Value::Null);
                    }
                    _ => return Err(unsupported("Claude stream delta")),
                }
            }
            "content_block_stop" => (),
            "message_delta" if self.started => {
                if let Some(u) = v.get("usage").and_then(Value::as_object) {
                    self.usage.as_object_mut().unwrap().extend(u.clone());
                }
                self.output = v
                    .pointer("/usage/output_tokens")
                    .cloned()
                    .or(self.output.take());
                if let Some(reason) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    let reason = match reason {
                        "end_turn" | "stop_sequence" => "stop",
                        "max_tokens" => "length",
                        "tool_use" => "tool_calls",
                        _ => return Err(unsupported("Claude stream stop reason")),
                    };
                    self.finished_reason = true;
                    self.chunk(out, json!({}), json!(reason));
                }
            }
            "message_stop" if self.started && self.finished_reason => {
                let usage = super::usage::mapped_usage(self.from, &self.usage)?;
                self.terminal = true;
                Self::event(
                    out,
                    "",
                    json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"choices":[],"usage":usage}),
                );
                out.extend_from_slice(b"data: [DONE]\n\n");
                self.complete = true;
            }
            _ => return Err(unsupported("Claude stream event or event ordering")),
        }
        Ok(())
    }
    fn start_block(&mut self, out: &mut Vec<u8>, block: Value) -> u64 {
        let index = self.next_block;
        self.next_block += 1;
        Self::event(
            out,
            "content_block_start",
            json!({"type":"content_block_start","index":index,"content_block":block}),
        );
        index
    }
    fn openai(&mut self, v: &Value, out: &mut Vec<u8>) -> Result<()> {
        if v.get("error").is_some() {
            return Err(invalid("upstream stream error"));
        }
        if !self.started {
            self.id = nonempty(&v["id"])?.into();
            self.started = true;
            Self::event(
                out,
                "message_start",
                json!({"type":"message_start","message":{"id":self.id,"type":"message","role":"assistant","model":self.model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{}}}),
            );
        }
        if let Some(usage) = v.get("usage").filter(|u| u.is_object()) {
            let mapped = super::usage::mapped_usage(self.from, usage)?;
            Self::event(
                out,
                "message_delta",
                json!({"type":"message_delta","delta":{},"usage":mapped}),
            );
        }
        let choices = array(&v["choices"])?;
        if choices.len() > 1 {
            return Err(unsupported("multiple stream choices"));
        }
        let Some(choice) = choices.first() else {
            return Ok(());
        };
        if choice["index"] != 0 {
            return Err(unsupported("nonzero stream choice"));
        }
        let delta = &choice["delta"];
        fields(delta, &["role", "content", "tool_calls"])?;
        if let Some(text) = delta.get("content").filter(|v| !v.is_null()) {
            let text = string(text)?;
            let index = match self.text_block {
                Some(i) => i,
                None => {
                    let i = self.start_block(out, json!({"type":"text","text":""}));
                    self.text_block = Some(i);
                    i
                }
            };
            Self::event(
                out,
                "content_block_delta",
                json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}),
            );
        }
        if let Some(tools) = delta.get("tool_calls") {
            for tool in array(tools)? {
                let key = uint(&tool["index"])?;
                if self.blocks.len() >= 1024 && !self.blocks.contains_key(&key) {
                    return Err(unsupported("too many tool blocks"));
                }
                if let Some(index) = self.text_block.take() {
                    Self::event(
                        out,
                        "content_block_stop",
                        json!({"type":"content_block_stop","index":index}),
                    );
                    self.closed.insert(index);
                }
                let index = if let Some(index) = self.blocks.get(&key) {
                    *index
                } else {
                    let index = self.start_block(out,json!({"type":"tool_use","id":nonempty(&tool["id"])?,"name":nonempty(&tool["function"]["name"])?,"input":{}}));
                    self.blocks.insert(key, index);
                    index
                };
                if let Some(args) = tool.pointer("/function/arguments") {
                    Self::event(
                        out,
                        "content_block_delta",
                        json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":string(args)?}}),
                    );
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            let reason = match reason {
                "stop" => "end_turn",
                "length" => "max_tokens",
                "tool_calls" => "tool_use",
                _ => return Err(unsupported("OpenAI stream finish reason")),
            };
            for index in 0..self.next_block {
                if self.closed.contains(&index) {
                    continue;
                }
                Self::event(
                    out,
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":index}),
                );
            }
            Self::event(
                out,
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":reason,"stop_sequence":null},"usage":{}}),
            );
            self.complete = true;
        }
        Ok(())
    }
    fn claude_end(&self, out: &mut Vec<u8>) {
        Self::event(out, "message_stop", json!({"type":"message_stop"}));
    }
}
