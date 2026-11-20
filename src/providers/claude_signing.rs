//! Final serialized-body CCH signing, matching CLIProxyAPI's Claude Code hash view.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use xxhash_rust::xxh64::xxh64;

const SEED: u64 = 0x4D659218E32A3268;
const CCH_LEN: usize = 5;

/// Replace only an existing lowercase five-digit CCH in `system[0].text`.
/// Call after all business payload mutations and serialization are complete.
pub fn sign(mut body: Vec<u8>) -> Result<Vec<u8>> {
    let Some(offset) = billing_offset(&body) else {
        return Ok(body);
    };
    body[offset..offset + CCH_LEN].copy_from_slice(b"00000");
    let normalized = normalize(&body).context("normalize Claude CCH input")?;
    let signature = format!("{:05x}", xxh64(&normalized, SEED) & 0xfffff);
    body[offset..offset + CCH_LEN].copy_from_slice(signature.as_bytes());
    Ok(body)
}

#[derive(Clone, Copy, PartialEq)]
enum Path {
    Root,
    System,
    FirstSystem,
    Billing,
    Other,
}

struct Member {
    start: usize,
    end: usize,
    comma_before: Option<usize>,
    comma_after: Option<usize>,
    excluded: bool,
}

struct Scanner<'a> {
    body: &'a [u8],
    pos: usize,
    edits: Vec<(usize, usize)>,
    billing: Option<(usize, usize)>,
}

impl<'a> Scanner<'a> {
    fn new(body: &'a [u8]) -> Self {
        Self {
            body,
            pos: 0,
            edits: Vec::new(),
            billing: None,
        }
    }

    fn whitespace(&mut self) {
        while matches!(self.body.get(self.pos), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.pos += 1;
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.body.get(self.pos) == Some(&byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn string(&mut self) -> Result<(usize, usize)> {
        let start = self.pos;
        if !self.consume(b'"') {
            bail!("missing JSON string at byte {start}");
        }
        while let Some(byte) = self.body.get(self.pos) {
            match byte {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    return Ok((start, self.pos));
                }
                _ => self.pos += 1,
            }
        }
        bail!("unterminated JSON string at byte {start}")
    }

    fn edit(&mut self, start: usize, end: usize) {
        if start < end {
            self.edits.push((start, end));
        }
    }

    fn value(&mut self, collect: bool, path: Path) -> Result<()> {
        self.whitespace();
        match self.body.get(self.pos) {
            Some(b'{') => self.object(collect, path),
            Some(b'[') => self.array(collect, path),
            Some(b'"') => {
                let span = self.string()?;
                if path == Path::Billing && self.billing.is_none() {
                    self.billing = Some(span);
                }
                Ok(())
            }
            Some(_) => {
                let start = self.pos;
                while let Some(byte) = self.body.get(self.pos) {
                    if matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n') {
                        break;
                    }
                    self.pos += 1;
                }
                if start == self.pos {
                    bail!("missing JSON value at byte {start}");
                }
                Ok(())
            }
            None => bail!("missing JSON value at byte {}", self.pos),
        }
    }

    fn object(&mut self, collect: bool, path: Path) -> Result<()> {
        self.pos += 1;
        self.whitespace();
        if self.consume(b'}') {
            return Ok(());
        }
        let mut members = Vec::new();
        let mut comma_before = None;
        loop {
            self.whitespace();
            let start = self.pos;
            let (key_start, key_end) = self.string()?;
            let key = &self.body[key_start..key_end];
            // Go normalization compares raw keys, but its system path lookup decodes them.
            let child_path = match path {
                Path::Root
                    if serde_json::from_slice::<String>(key).ok().as_deref() == Some("system") =>
                {
                    Path::System
                }
                Path::FirstSystem
                    if serde_json::from_slice::<String>(key).ok().as_deref() == Some("text") =>
                {
                    Path::Billing
                }
                _ => Path::Other,
            };
            self.whitespace();
            if !self.consume(b':') {
                bail!("missing object colon at byte {}", self.pos);
            }
            self.whitespace();
            let excluded = collect
                && matches!(
                    key,
                    b"\"max_tokens\"" | b"\"fallbacks\"" | b"\"fallback_credit_token\""
                );
            if collect && key == b"\"model\"" && self.body.get(self.pos) == Some(&b'"') {
                let (start, end) = self.string()?;
                self.edit(start + 1, end - 1);
            } else {
                self.value(collect && !excluded, child_path)?;
            }
            let end = self.pos;
            self.whitespace();
            let comma_after = if self.consume(b',') {
                Some(self.pos - 1)
            } else {
                None
            };
            members.push(Member {
                start,
                end,
                comma_before,
                comma_after,
                excluded,
            });
            if comma_after.is_some() {
                comma_before = comma_after;
                continue;
            }
            if !self.consume(b'}') {
                bail!("missing object end at byte {}", self.pos);
            }
            break;
        }
        if collect {
            self.exclude_members(&members);
        }
        Ok(())
    }

    fn array(&mut self, collect: bool, path: Path) -> Result<()> {
        self.pos += 1;
        self.whitespace();
        if self.consume(b']') {
            return Ok(());
        }
        let mut index = 0;
        loop {
            let child_path = if path == Path::System && index == 0 {
                Path::FirstSystem
            } else {
                Path::Other
            };
            self.value(collect, child_path)?;
            index += 1;
            self.whitespace();
            if self.consume(b',') {
                continue;
            }
            if !self.consume(b']') {
                bail!("missing array end at byte {}", self.pos);
            }
            return Ok(());
        }
    }

    fn exclude_members(&mut self, members: &[Member]) {
        let mut start = 0;
        while start < members.len() {
            if !members[start].excluded {
                start += 1;
                continue;
            }
            let mut end = start;
            while end + 1 < members.len() && members[end + 1].excluded {
                end += 1;
            }
            if end + 1 < members.len() {
                self.edit(members[start].start, members[end].comma_after.unwrap() + 1);
            } else if start > 0 && end == start {
                self.edit(members[start].comma_before.unwrap(), members[end].end);
            } else {
                // Native keeps the preceding comma for a trailing run of 2+ members.
                self.edit(members[start].start, members[end].end);
            }
            start = end + 1;
        }
    }
}

fn billing_offset(body: &[u8]) -> Option<usize> {
    let mut scanner = Scanner::new(body);
    // Like Go's path lookup, eligibility may be found before invalid trailing data.
    let _ = scanner.value(false, Path::Root);
    let (start, end) = scanner.billing?;
    let raw = &body[start..end];
    let text: String = serde_json::from_slice(raw).ok()?;
    if !text.starts_with("x-anthropic-billing-header:") {
        return None;
    }
    raw.windows(10)
        .position(|window| {
            window.starts_with(b"cch=")
                && window[9] == b';'
                && window[4..9]
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        })
        .map(|offset| start + offset + 4)
}

fn normalize(body: &[u8]) -> Result<Vec<u8>> {
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    serde::de::IgnoredAny::deserialize(&mut deserializer).context("invalid JSON body")?;
    deserializer.end().context("invalid JSON body")?;
    let mut scanner = Scanner::new(body);
    scanner.value(true, Path::Root)?;
    scanner.whitespace();
    if scanner.pos != body.len() {
        bail!("unexpected JSON data at byte {}", scanner.pos);
    }
    scanner.edits.sort_unstable_by_key(|edit| edit.0);
    let mut normalized = Vec::with_capacity(body.len());
    let mut last = 0;
    for (start, end) in scanner.edits {
        if start < last || end > body.len() {
            bail!("overlapping CCH normalization edit at byte {start}");
        }
        normalized.extend_from_slice(&body[last..start]);
        last = end;
    }
    normalized.extend_from_slice(&body[last..]);
    Ok(normalized)
}

#[cfg(test)]
#[path = "claude_signing_tests.rs"]
mod tests;
