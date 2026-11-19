use super::{array, envelope, finish_reason, metadata, parts, text, upstream_error, usage};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const LIMIT: usize = 32 * 1024 * 1024;
#[derive(Default)]
struct Candidate {
    next_tool: u64,
    terminal: bool,
    announced: bool,
}

/// Incremental Gemini SSE -> OpenAI Chat SSE. Call finish at transport EOF;
/// Gemini does not normally send [DONE]. Errors poison the translator.
pub struct StreamTranslator {
    line: Vec<u8>,
    data: Vec<u8>,
    base: Value,
    candidates: BTreeMap<u64, Candidate>,
    include_usage: bool,
    usage: Option<Value>,
    done: bool,
    failed: bool,
}
impl StreamTranslator {
    pub fn new(model: &str, include_usage: bool) -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            base: envelope(model, true),
            candidates: BTreeMap::new(),
            include_usage,
            usage: None,
            done: false,
            failed: false,
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        ensure!(!self.failed, "Gemini stream previously failed");
        let result = self.feed_inner(bytes);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn feed_inner(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        if self.done {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for &byte in bytes {
            ensure!(
                self.line.len().saturating_add(self.data.len()) < LIMIT,
                "Gemini SSE event too large"
            );
            if byte == b'\n' {
                self.line(&mut out)?;
                if self.done {
                    break;
                }
            } else {
                self.line.push(byte);
            }
        }
        Ok(out)
    }
    fn line(&mut self, out: &mut Vec<u8>) -> Result<()> {
        if self.line.last() == Some(&b'\r') {
            self.line.pop();
        }
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            self.event(out)?;
        } else if let Some(data) = line.strip_prefix(b"data:") {
            if !self.data.is_empty() {
                self.data.push(b'\n');
            }
            self.data
                .extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
        }
        Ok(())
    }
    fn emit(&self, choices: Value, usage: Option<&Value>, out: &mut Vec<u8>) {
        let mut chunk = self.base.clone();
        chunk["choices"] = choices;
        if let Some(usage) = usage {
            chunk["usage"] = usage.clone();
        } else if self.include_usage {
            chunk["usage"] = Value::Null;
        }
        out.extend_from_slice(format!("data: {chunk}\n\n").as_bytes());
    }
    fn event(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let data = std::mem::take(&mut self.data);
        if data.is_empty() {
            return Ok(());
        }
        if data == b"[DONE]" {
            return self.terminal(out);
        }
        let event: Value = serde_json::from_slice(&data)?;
        upstream_error(&event)?;
        metadata(&event, &mut self.base)?;
        if let Some(value) = event
            .get("usageMetadata")
            .or_else(|| event.get("cpaUsageMetadata"))
        {
            self.usage = Some(usage(value)?);
        }
        let Some(candidates) = event.get("candidates") else {
            ensure!(
                event.get("usageMetadata").is_some() || event.get("cpaUsageMetadata").is_some(),
                "Gemini SSE has no candidates or usage"
            );
            return Ok(());
        };
        let mut seen = std::collections::HashSet::new();
        for (position, value) in array(candidates)?.iter().enumerate() {
            let index = value
                .get("index")
                .map(|v| {
                    v.as_u64()
                        .ok_or_else(|| anyhow::anyhow!("invalid candidate index"))
                })
                .transpose()?
                .unwrap_or(position as u64);
            ensure!(seen.insert(index), "duplicate Gemini candidate index");
            ensure!(
                self.candidates.contains_key(&index) || self.candidates.len() < 4096,
                "too many Gemini candidates"
            );
            let candidate = self.candidates.entry(index).or_default();
            ensure!(
                !candidate.terminal,
                "Gemini candidate continued after finishReason"
            );
            let mut delta = parts(value, index, &mut candidate.next_tool)?;
            let reason = value.get("finishReason").map(text).transpose()?;
            let finish = reason
                .map(|reason| finish_reason(reason, candidate.next_tool > 0))
                .transpose()?;
            if !candidate.announced {
                delta["role"] = json!("assistant");
                candidate.announced = true;
            }
            candidate.terminal = finish.is_some();
            let mut choice = json!({"index":index,"delta":delta,"finish_reason":finish});
            if let Some(reason) = reason {
                choice["native_finish_reason"] = json!(reason.to_ascii_lowercase());
            }
            self.emit(json!([choice]), None, out);
        }
        Ok(())
    }
    fn terminal(&mut self, out: &mut Vec<u8>) -> Result<()> {
        ensure!(
            !self.candidates.is_empty() && self.candidates.values().all(|c| c.terminal),
            "Gemini stream ended before terminal finishReason"
        );
        if self.include_usage
            && let Some(usage) = &self.usage
        {
            self.emit(json!([]), Some(usage), out);
        }
        out.extend_from_slice(b"data: [DONE]\n\n");
        self.done = true;
        Ok(())
    }
    /// Flush an unterminated final SSE event, validate terminals, and emit usage
    /// (when requested) and exactly one [DONE]. Idempotent after success.
    pub fn finish(&mut self) -> Result<Vec<u8>> {
        ensure!(!self.failed, "Gemini stream previously failed");
        if self.done {
            return Ok(Vec::new());
        }
        let result = (|| {
            let mut out = Vec::new();
            if !self.line.is_empty() {
                self.line(&mut out)?;
            }
            if !self.done {
                self.event(&mut out)?;
            }
            if !self.done {
                self.terminal(&mut out)?;
            }
            Ok(out)
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}
