//! Antigravity system-instruction sensitive-word obfuscation (port of Go
//! `helps/cloak_obfuscate.go`, Antigravity variant only). Ordered regex
//! replacements run first on the original text, then literal replacements and
//! per-subword U+034F insertion run on the untouched gaps (non-cascading).
use crate::config::providers::AntigravitySensitive;
use regex::{Captures, Regex};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

/// U+034F combining grapheme joiner; the upstream still rejects U+200B.
const CGJ: char = '\u{034F}';

/// Maximal runs matching Go `unicode.IsLetter || unicode.IsDigit || U+034F || U+200B`.
static SUBWORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\p{L}\p{Nd}\x{034F}\x{200B}]+").expect("static subword regex"));

struct Rule {
    target: String,
    lower: String,
    replacement: String,
    is_replacement: bool,
}

struct Span {
    start: usize,
    end: usize,
    replacement: String,
}

pub(super) struct Matcher {
    literal: Option<Regex>,
    exact: HashMap<String, String>,
    lower: HashMap<String, String>,
    replacement_rules: Vec<Rule>,
    patterns: Vec<(Regex, String)>,
}

/// Obfuscates `request.systemInstruction` (and `request.system_instruction`)
/// of the native Antigravity envelope in place. Must run before user payload rules.
pub(super) fn obfuscate_system_instruction(body: &mut Value, rules: &AntigravitySensitive) {
    if let Some(matcher) = cached(rules) {
        apply(body, &matcher);
    }
}

fn apply(body: &mut Value, matcher: &Matcher) -> bool {
    let mut modified = false;
    let Some(request) = body.get_mut("request").and_then(Value::as_object_mut) else {
        return false;
    };
    for key in ["systemInstruction", "system_instruction"] {
        match request.get_mut(key) {
            Some(Value::String(text)) => modified |= rewrite(text, matcher),
            Some(Value::Object(instruction)) => {
                let Some(Value::Array(parts)) = instruction.get_mut("parts") else {
                    continue;
                };
                for part in parts {
                    if let Some(Value::String(text)) = part.get_mut("text") {
                        modified |= rewrite(text, matcher);
                    }
                }
            }
            _ => {}
        }
    }
    modified
}

fn rewrite(text: &mut String, matcher: &Matcher) -> bool {
    let obfuscated = matcher.obfuscate_text(text);
    if obfuscated == *text {
        return false;
    }
    *text = obfuscated;
    true
}

/// Single-entry cache keyed by the rule set, like Go `getSensitiveWordMatcher`.
fn cached(rules: &AntigravitySensitive) -> Option<Arc<Matcher>> {
    type Entry = Option<(AntigravitySensitive, Option<Arc<Matcher>>)>;
    static CACHE: Mutex<Entry> = Mutex::new(None);
    if rules.words.is_empty() && rules.replace.is_empty() && rules.patterns.is_empty() {
        return None;
    }
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((key, matcher)) = cache.as_ref()
        && key == rules
    {
        return matcher.clone();
    }
    let matcher = build(&rules.words, &rules.replace, &rules.patterns).map(Arc::new);
    *cache = Some((rules.clone(), matcher.clone()));
    matcher
}

/// Go `buildSensitiveWordMatcher` with `obfuscateAntigravityPhrase` and U+034F marker.
pub(super) fn build(
    words: &[String],
    replace: &[(String, String)],
    patterns: &[(String, String)],
) -> Option<Matcher> {
    if words.is_empty() && replace.is_empty() && patterns.is_empty() {
        return None;
    }
    let patterns: Vec<(Regex, String)> = patterns
        .iter()
        .filter(|(p, _)| !p.trim().is_empty())
        .filter_map(|(p, r)| Regex::new(p).ok().map(|re| (re, r.clone())))
        .collect();

    let mut rules = Vec::new();
    let mut replacement_rules = Vec::new();
    let mut exact = HashMap::new();
    let mut lower_map = HashMap::new();
    let mut seen = std::collections::HashSet::new();

    let mut sorted: Vec<&(String, String)> = replace.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (raw, value) in sorted {
        let target = raw.trim();
        if target.is_empty() {
            continue;
        }
        if !exact.contains_key(target) || raw == target {
            exact.insert(target.to_owned(), value.clone());
        }
        let lower = target.to_lowercase();
        lower_map
            .entry(lower.clone())
            .or_insert_with(|| value.clone());
        if !seen.insert(lower.clone()) {
            continue;
        }
        let make = || Rule {
            target: target.to_owned(),
            lower: lower.clone(),
            replacement: value.clone(),
            is_replacement: true,
        };
        rules.push(make());
        replacement_rules.push(make());
    }

    for raw in words {
        let w = raw.trim();
        if w.chars().count() < 2 || w.contains(CGJ) || obfuscate_phrase(w) == w {
            continue;
        }
        let lower = w.to_lowercase();
        if !seen.insert(lower.clone()) {
            continue;
        }
        rules.push(Rule {
            target: w.to_owned(),
            lower,
            replacement: String::new(),
            is_replacement: false,
        });
    }

    if rules.is_empty() && patterns.is_empty() {
        return None;
    }
    let mut literal = None;
    if !rules.is_empty() {
        sort_longest_first(&mut rules);
        sort_longest_first(&mut replacement_rules);
        let alternation: Vec<String> = rules.iter().map(|r| regex::escape(&r.target)).collect();
        match Regex::new(&format!("(?i:{})", alternation.join("|"))) {
            Ok(re) => literal = Some(re),
            Err(_) if patterns.is_empty() => return None,
            Err(_) => {}
        }
    }
    Some(Matcher {
        literal,
        exact,
        lower: lower_map,
        replacement_rules,
        patterns,
    })
}

fn sort_longest_first(rules: &mut [Rule]) {
    rules.sort_by(|a, b| {
        b.target
            .chars()
            .count()
            .cmp(&a.target.chars().count())
            .then(b.target.len().cmp(&a.target.len()))
            .then(b.is_replacement.cmp(&a.is_replacement))
            .then(a.lower.cmp(&b.lower))
    });
}

/// Inserts U+034F after the first char; None for single-char or U+FFFD-led
/// input (Go treats a leading `utf8.RuneError` as undecodable).
fn insert_after_first(s: &str) -> Option<String> {
    let c = s.chars().next()?;
    if c == '\u{FFFD}' || c.len_utf8() >= s.len() {
        return None;
    }
    Some(format!("{c}{CGJ}{}", &s[c.len_utf8()..]))
}

/// Go `obfuscateAntigravityPhrase`: U+034F after the first char of every
/// alphanumeric subword of two or more chars, punctuation and spacing preserved.
fn obfuscate_phrase(phrase: &str) -> String {
    if phrase.is_empty() || phrase.contains(CGJ) {
        return phrase.to_owned();
    }
    let mut out = String::with_capacity(phrase.len() + 4);
    let mut last = 0;
    let mut inserted = false;
    for m in SUBWORD.find_iter(phrase) {
        out.push_str(&phrase[last..m.start()]);
        match insert_after_first(m.as_str()) {
            Some(obfuscated) => {
                out.push_str(&obfuscated);
                inserted = true;
            }
            None => out.push_str(m.as_str()),
        }
        last = m.end();
    }
    out.push_str(&phrase[last..]);
    if inserted {
        return out;
    }
    insert_after_first(phrase).unwrap_or_else(|| phrase.to_owned())
}

fn fold_eq(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

fn overlaps(a_start: usize, a_end: usize, b_start: usize, b_end: usize) -> bool {
    if a_start == a_end {
        return b_start <= a_start && a_start <= b_end;
    }
    if b_start == b_end {
        return a_start <= b_start && b_start <= a_end;
    }
    a_start < b_end && b_start < a_end
}

impl Matcher {
    fn lookup(&self, matched: &str) -> Option<&str> {
        self.exact
            .get(matched)
            .or_else(|| self.lower.get(&matched.to_lowercase()))
            .map(String::as_str)
            .or_else(|| {
                self.replacement_rules
                    .iter()
                    .find(|r| fold_eq(matched, &r.target))
                    .map(|r| r.replacement.as_str())
            })
    }

    fn obfuscate_literals(&self, text: &str) -> String {
        match &self.literal {
            Some(re) if !text.is_empty() => re
                .replace_all(text, |caps: &Captures| {
                    let matched = &caps[0];
                    self.lookup(matched)
                        .map(str::to_owned)
                        .unwrap_or_else(|| obfuscate_phrase(matched))
                })
                .into_owned(),
            _ => text.to_owned(),
        }
    }

    /// Go `collectPatternSpans`: earlier rules win; later rules only claim
    /// non-overlapping spans, except empty deletions that enclose earlier
    /// empty deletions (which they subsume). All spans refer to the original text.
    fn pattern_spans(&self, text: &str) -> Vec<Span> {
        let mut spans: Vec<Span> = Vec::new();
        if text.is_empty() {
            return spans;
        }
        for (re, template) in &self.patterns {
            let matches: Vec<Span> = re
                .captures_iter(text)
                .filter_map(|caps| {
                    let m = caps.get(0)?;
                    let mut replacement = String::new();
                    caps.expand(template, &mut replacement);
                    (m.start() != m.end() || !replacement.is_empty()).then_some(Span {
                        start: m.start(),
                        end: m.end(),
                        replacement,
                    })
                })
                .collect();
            if matches.is_empty() {
                continue;
            }
            if spans.is_empty() {
                spans = matches;
                continue;
            }

            let mut accepted = Vec::new();
            let mut subsumed = vec![false; spans.len()];
            let mut ci = 0;
            for m in matches {
                while ci < spans.len() {
                    let c = &spans[ci];
                    if c.end < m.start || (c.end == m.start && c.start < c.end && m.start < m.end) {
                        ci += 1;
                        continue;
                    }
                    break;
                }
                let mut conflict = false;
                let mut local = Vec::new();
                for (j, c) in spans.iter().enumerate().skip(ci) {
                    if subsumed[j] {
                        continue;
                    }
                    if c.start > m.end || (c.start == m.end && c.start < c.end && m.start < m.end) {
                        break;
                    }
                    if !overlaps(m.start, m.end, c.start, c.end) {
                        continue;
                    }
                    if m.replacement.is_empty()
                        && c.replacement.is_empty()
                        && m.start <= c.start
                        && c.end <= m.end
                    {
                        local.push(j);
                        continue;
                    }
                    conflict = true;
                    break;
                }
                if conflict {
                    continue;
                }
                for j in local {
                    subsumed[j] = true;
                }
                accepted.push(m);
            }
            if accepted.is_empty() {
                continue;
            }

            let mut kept = std::mem::take(&mut spans)
                .into_iter()
                .zip(subsumed)
                .filter(|(_, s)| !s)
                .map(|(span, _)| span)
                .peekable();
            let mut added = accepted.into_iter().peekable();
            loop {
                let take_kept = match (kept.peek(), added.peek()) {
                    (Some(s), Some(a)) => {
                        s.start < a.start || (s.start == a.start && s.end <= a.end)
                    }
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (None, None) => break,
                };
                let next = if take_kept { kept.next() } else { added.next() };
                spans.extend(next);
            }
        }
        spans
    }

    /// Go `obfuscateText`.
    pub(super) fn obfuscate_text(&self, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let spans = self.pattern_spans(text);
        if spans.is_empty() {
            return self.obfuscate_literals(text);
        }
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for span in spans {
            if span.start > last {
                out.push_str(&self.obfuscate_literals(&text[last..span.start]));
            }
            out.push_str(&span.replacement);
            last = span.end;
        }
        if last < text.len() {
            out.push_str(&self.obfuscate_literals(&text[last..]));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn subwords_and_punctuation() {
        let m = build(
            &strings(&[
                "Codex",
                "GPT-5",
                "Claude Code",
                "<system-conventions>",
                "claude-code",
                "Agent X",
            ]),
            &[],
            &[],
        )
        .unwrap();
        let input = "You are Codex, an agent based on GPT-5. Follow <system-conventions> in Claude Code / claude-code with Agent X.";
        let got = m.obfuscate_text(input);
        let want = "You are C\u{034F}odex, an agent based on G\u{034F}PT-5. Follow <s\u{034F}ystem-c\u{034F}onventions> in C\u{034F}laude C\u{034F}ode / c\u{034F}laude-c\u{034F}ode with A\u{034F}gent X.";
        assert_eq!(got, want);
        assert!(!got.contains('\u{200B}'));
        assert_eq!(
            m.obfuscate_text(&got),
            got,
            "second pass must be idempotent"
        );
    }

    #[test]
    fn replacements_precedence_and_empty_deletion() {
        let m = build(
            &strings(&["Claude", "Codex", "GPT-5"]),
            &pairs(&[
                (
                    "You are Codex, an agent based on GPT-5.",
                    "You are a helpful coding assistant.",
                ),
                ("Claude Code", "Claude"),
                ("Codex", "Assistant"),
                ("RemoveMe ", ""),
            ]),
            &[],
        )
        .unwrap();
        let input = "you are codex, an agent based on gpt-5. Built for Claude Code, standalone Claude, Codex, and RemoveMe .";
        assert_eq!(
            m.obfuscate_text(input),
            "You are a helpful coding assistant. Built for Claude, standalone C\u{034F}laude, Assistant, and  ."
        );
    }

    #[test]
    fn pattern_rules_non_cascading() {
        let m = build(
            &strings(&["Claude", "omp"]),
            &pairs(&[("Codex", "Assistant")]),
            &pairs(&[
                ("[invalid", "skip"),
                (r"(?m)^You are (omp|Codex)[^\n]*\n?", "You are Claude ($1).\n"),
                (
                    r"(?s)<system[-_](conventions|directive)>.*?</system[-_](conventions|directive)>\n?",
                    "",
                ),
            ]),
        )
        .unwrap();
        let input = "You are omp's assistant.\n<system-directive>\nRFC 2119\n</system-directive>\nKeep Codex and omp.";
        assert_eq!(
            m.obfuscate_text(input),
            "You are Claude (omp).\nKeep Assistant and o\u{034F}mp."
        );
    }

    #[test]
    fn pattern_rules_preserve_original_anchors_and_nested_deletion() {
        let prefix = build(&[], &[], &pairs(&[("prefix", "safe"), ("(?m)^KEEP", "")])).unwrap();
        assert_eq!(prefix.obfuscate_text("prefixKEEP"), "safeKEEP");
        let suffix = build(&[], &[], &pairs(&[("suffix", "safe"), ("(?m)KEEP$", "")])).unwrap();
        assert_eq!(suffix.obfuscate_text("KEEPsuffix"), "KEEPsafe");

        let non_cascade = build(
            &strings(&["foobar"]),
            &pairs(&[("foobar", "LITERAL_CHANGED")]),
            &pairs(&[("DELETE", ""), ("foobar", "REGEX_CHANGED")]),
        )
        .unwrap();
        assert_eq!(non_cascade.obfuscate_text("fooDELETEbar"), "foobar");

        let nested = build(
            &strings(&["RFC 2119"]),
            &[],
            &pairs(&[
                (r"(?m)^RFC 2119[^\n]*\n?", ""),
                (
                    r"(?s)<system[-_]conventions>.*?</system[-_]conventions>\n?",
                    "",
                ),
            ]),
        )
        .unwrap();
        assert_eq!(
            nested.obfuscate_text(
                "<system-conventions>\nRFC 2119 applies.\n</system-conventions>\nKeep me."
            ),
            "Keep me."
        );
    }

    #[test]
    fn empty_or_ineffective_rules_build_nothing() {
        assert!(build(&[], &[], &[]).is_none());
        // Single-char words and words already carrying U+034F are ignored.
        assert!(build(&strings(&["a", "C\u{034F}odex", "  "]), &[], &[]).is_none());
        assert!(build(&[], &[], &pairs(&[("[invalid", "x"), ("  ", "y")])).is_none());
    }

    #[test]
    fn envelope_system_instruction_only() {
        let rules = AntigravitySensitive {
            words: strings(&["Codex"]),
            replace: pairs(&[("secret", "")]),
            patterns: pairs(&[(r"token-\d+", "[redacted]")]),
        };
        let mut body = serde_json::json!({
            "model": "m",
            "request": {
                "systemInstruction": {"parts": [
                    {"text": "You are Codex; secret token-42"},
                    {"inlineData": {"data": "Codex"}}
                ]},
                "system_instruction": "Codex",
                "contents": [{"role": "user", "parts": [{"text": "Codex secret"}]}]
            }
        });
        obfuscate_system_instruction(&mut body, &rules);
        assert_eq!(
            body["request"]["systemInstruction"]["parts"][0]["text"],
            "You are C\u{034F}odex;  [redacted]"
        );
        assert_eq!(
            body["request"]["systemInstruction"]["parts"][1]["inlineData"]["data"],
            "Codex"
        );
        assert_eq!(body["request"]["system_instruction"], "C\u{034F}odex");
        assert_eq!(
            body["request"]["contents"][0]["parts"][0]["text"],
            "Codex secret"
        );

        let mut untouched = body.clone();
        obfuscate_system_instruction(&mut untouched, &AntigravitySensitive::default());
        assert_eq!(untouched, body);
    }
}
