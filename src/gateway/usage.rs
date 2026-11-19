//! Conservative accounting: retain authoritative totals, never invent token buckets.
use super::upstream::Format;
use crate::reporting::TokenBreakdown;
use serde_json::Value;

const MAX_EVENT: usize = 256 * 1024;

pub(super) struct Usage {
    format: Format,
    line: Vec<u8>,
    data: Vec<u8>,
    oversized: bool,
    input: Option<i64>,
    output: Option<i64>,
    cache_read: i64,
    cache_write: i64,
    total: Option<i64>,
    reasoning: i64,
    tool_input: i64,
    pub failed: bool,
}

impl Usage {
    pub fn new(format: Format) -> Self {
        Self {
            format,
            line: Vec::new(),
            data: Vec::new(),
            oversized: false,
            input: None,
            output: None,
            cache_read: 0,
            cache_write: 0,
            total: None,
            reasoning: 0,
            tool_input: 0,
            failed: false,
        }
    }

    pub fn json(&mut self, value: &Value) {
        self.failed |= value.get("error").is_some()
            || value
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| matches!(t, "error" | "response.failed"));
        match self.format {
            Format::OpenAi => {
                let usage = value
                    .get("usage")
                    .or_else(|| value.pointer("/response/usage"));
                if let Some(usage) = usage {
                    self.input = count(usage, "prompt_tokens")
                        .or_else(|| count(usage, "input_tokens"))
                        .or(self.input);
                    self.output = count(usage, "completion_tokens")
                        .or_else(|| count(usage, "output_tokens"))
                        .or(self.output);
                    self.total = count(usage, "total_tokens").or(self.total);
                    self.cache_read = nested(
                        usage,
                        &[
                            "/prompt_tokens_details/cached_tokens",
                            "/input_tokens_details/cached_tokens",
                        ],
                    )
                    .unwrap_or(self.cache_read);
                    self.cache_write = nested(
                        usage,
                        &[
                            "/input_tokens_details/cache_creation_tokens",
                            "/prompt_tokens_details/cache_creation_tokens",
                        ],
                    )
                    .unwrap_or(self.cache_write);
                    self.reasoning = nested(
                        usage,
                        &[
                            "/completion_tokens_details/reasoning_tokens",
                            "/output_tokens_details/reasoning_tokens",
                            "/reasoning_tokens",
                        ],
                    )
                    .unwrap_or(self.reasoning);
                }
            }
            Format::Gemini => {
                if let Some(usage) = value.get("usageMetadata") {
                    self.input = count(usage, "promptTokenCount")
                        .or_else(|| count(usage, "prompt_token_count"))
                        .or(self.input);
                    self.output = count(usage, "candidatesTokenCount")
                        .or_else(|| count(usage, "candidates_token_count"))
                        .or(self.output);
                    self.reasoning = count(usage, "thoughtsTokenCount")
                        .or_else(|| count(usage, "thoughts_token_count"))
                        .unwrap_or(self.reasoning);
                    self.tool_input = count(usage, "toolUsePromptTokenCount")
                        .or_else(|| count(usage, "tool_use_prompt_token_count"))
                        .unwrap_or(self.tool_input);
                    self.cache_read = count(usage, "cachedContentTokenCount")
                        .or_else(|| count(usage, "cached_content_token_count"))
                        .unwrap_or(self.cache_read);
                    self.total = count(usage, "totalTokenCount")
                        .or_else(|| count(usage, "total_token_count"))
                        .or(self.total);
                }
            }
            Format::Claude => {
                if let Some(usage) = value
                    .get("usage")
                    .or_else(|| value.pointer("/message/usage"))
                {
                    self.reasoning = nested(
                        usage,
                        &[
                            "/output_tokens_details/thinking_tokens",
                            "/output_tokens_details/reasoning_tokens",
                            "/thinking_tokens",
                        ],
                    )
                    .unwrap_or(self.reasoning);
                    if let Some(input) = count(usage, "input_tokens") {
                        self.input = Some(input);
                    }
                    if let Some(output) = count(usage, "output_tokens") {
                        self.output = Some(output);
                    }
                    if let Some(read) = count(usage, "cache_read_input_tokens") {
                        self.cache_read = read;
                    }
                    if let Some(write) = count(usage, "cache_creation_input_tokens") {
                        self.cache_write = write;
                    }
                }
            }
        }
    }

    pub fn breakdown(&self) -> TokenBreakdown {
        let result = (|| {
            let mut input = self.input.unwrap_or(0);
            let mut output = self.output.unwrap_or(0);
            if self.format == Format::Claude {
                input = input
                    .checked_add(self.cache_read)?
                    .checked_add(self.cache_write)?;
            } else if self.format == Format::Gemini {
                input = input.checked_add(self.tool_input)?;
                output = output.checked_add(self.reasoning)?;
            }
            let expected = input.checked_add(output)?;
            // An explicitly supplied zero is authoritative, not an absent total.
            let total = self.total.unwrap_or(expected);
            let read = if self.input.is_some() {
                self.cache_read
            } else {
                0
            };
            let write = if self.input.is_some() {
                self.cache_write
            } else {
                0
            };
            let reasoning = if self.output.is_some() {
                self.reasoning
            } else {
                0
            };
            let cache = read.checked_add(write)?;
            let complete = self.input.is_some() && self.output.is_some();
            if cache > input
                || reasoning > output
                || total < expected
                || (complete && total != expected)
            {
                return None;
            }
            let mut b = TokenBreakdown::subset(input, read, write, output, reasoning, expected);
            b.total_tokens = total;
            b.unclassified_tokens = total - expected;
            if !complete || b.unclassified_tokens > 0 {
                b.quality = "unclassified".into();
            }
            Some(b)
        })();
        result.unwrap_or_else(|| {
            let mut b = TokenBreakdown::unclassified(self.total.unwrap_or(0));
            b.quality = "inconsistent".into();
            b
        })
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if byte == b'\n' {
                if self.line.last() == Some(&b'\r') {
                    self.line.pop();
                }
                if self.line.is_empty() {
                    self.event();
                } else if !self.oversized && self.line.starts_with(b"data:") {
                    let raw = &self.line[5..];
                    let raw = raw.strip_prefix(b" ").unwrap_or(raw);
                    if self.data.len().saturating_add(raw.len() + 1) > MAX_EVENT {
                        self.oversized = true;
                        self.data.clear();
                    } else {
                        self.data.extend_from_slice(raw);
                        self.data.push(b'\n');
                    }
                }
                self.line.clear();
            } else if self.line.len() < MAX_EVENT {
                self.line.push(byte);
            } else {
                self.oversized = true;
            }
        }
    }

    fn event(&mut self) {
        if !self.oversized
            && let Ok(value) = serde_json::from_slice::<Value>(&self.data)
        {
            self.json(&value);
        }
        self.data.clear();
        self.oversized = false;
    }
}

fn nested(value: &Value, paths: &[&str]) -> Option<i64> {
    paths
        .iter()
        .find_map(|p| value.pointer(p).and_then(Value::as_i64).filter(|n| *n >= 0))
}

fn count(value: &Value, name: &str) -> Option<i64> {
    value.get(name)?.as_i64().filter(|n| *n >= 0)
}
