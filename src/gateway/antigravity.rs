//! Bounded SSE framing for Cloud Code envelopes; usage is a snapshot, not a delta.
use anyhow::{Result, ensure};
use serde_json::Value;

use super::{diagnostics, usage::Usage};

const MAX_EVENT: usize = 1024 * 1024;

#[derive(Default)]
pub(super) struct Events {
    line: Vec<u8>,
    data: Vec<u8>,
    seen: bool,
    terminal: bool,
    done: bool,
}

impl Events {
    pub fn feed(&mut self, bytes: &[u8], usage: &mut Usage) -> Result<Vec<u8>> {
        let result = self.feed_inner(bytes, usage);
        if result.is_err() {
            diagnostics::stage("antigravity_sse_parse");
        }
        result
    }
    fn feed_inner(&mut self, bytes: &[u8], usage: &mut Usage) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        for &byte in bytes {
            if byte != b'\n' {
                ensure!(
                    self.line.len().saturating_add(self.data.len()) < MAX_EVENT,
                    "Antigravity event exceeds limit"
                );
                self.line.push(byte);
                continue;
            }
            if self.line.last() == Some(&b'\r') {
                self.line.pop();
            }
            if self.line.is_empty() {
                if !self.data.is_empty() {
                    ensure!(!self.done, "Antigravity data after DONE");
                    if self.data == b"[DONE]\n" {
                        ensure!(self.terminal, "Antigravity DONE before completion");
                        self.done = true;
                    } else {
                        let envelope: Value = serde_json::from_slice(&self.data)?;
                        if let Some(failure) = diagnostics::event_failure(&envelope, "antigravity")
                        {
                            return Err(failure.into());
                        }
                        let response = crate::providers::antigravity::unwrap_response(&envelope)?;
                        self.terminal |= response
                            .get("candidates")
                            .and_then(Value::as_array)
                            .is_some_and(|candidates| {
                                !candidates.is_empty()
                                    && candidates.iter().all(|candidate| {
                                        candidate
                                            .get("finishReason")
                                            .and_then(Value::as_str)
                                            .is_some_and(|reason| !reason.is_empty())
                                    })
                            })
                            || response
                                .get("promptFeedback")
                                .and_then(|v| v.get("blockReason"))
                                .is_some();
                        usage.json(&response);
                        self.seen = true;
                        output.extend_from_slice(b"data: ");
                        serde_json::to_writer(&mut output, &response)?;
                        output.extend_from_slice(b"\n\n");
                    }
                    self.data.clear();
                }
            } else if let Some(data) = self.line.strip_prefix(b"data:") {
                let data = data.strip_prefix(b" ").unwrap_or(data);
                ensure!(
                    self.data.len().saturating_add(data.len() + 1) <= MAX_EVENT,
                    "Antigravity event exceeds limit"
                );
                self.data.extend_from_slice(data);
                self.data.push(b'\n');
            }
            self.line.clear();
        }
        Ok(output)
    }

    pub fn complete(&self) -> bool {
        self.seen && self.terminal && self.line.is_empty() && self.data.is_empty()
    }
}
