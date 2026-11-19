use anyhow::{Result, bail, ensure};
use serde_json::{Map, Value};

#[derive(Clone, Debug)]
pub(super) struct Path(Vec<Part>);
#[derive(Clone, Debug)]
struct Part {
    text: String,
    wildcard: bool,
    array: bool,
    query: Option<(super::query::Query, bool)>,
}

impl Path {
    pub(super) fn parse(input: &str) -> Result<Self> {
        ensure!(!input.is_empty(), "empty payload path");
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut wildcard = false;
        let mut escaped = false;
        for component in super::query::split(input, ".") {
            if component == "@this" {
                continue;
            }
            if component.starts_with("#(") {
                let (query, all) = if let Some(q) = component.strip_suffix(")#") {
                    (q, true)
                } else if let Some(q) = component.strip_suffix(')') {
                    (q, false)
                } else {
                    bail!("unclosed payload query");
                };
                parts.push(Part {
                    text: String::new(),
                    wildcard: false,
                    array: true,
                    query: Some((super::query::Query::parse(&query[2..])?, all)),
                });
                continue;
            }
            ensure!(
                component.chars().rev().take_while(|c| *c == '\\').count() % 2 == 0,
                "trailing escape in payload path {input}"
            );
            for c in component.chars().chain(std::iter::once('.')) {
                if escaped {
                    // Preserve wildcard escapes for the glob matcher.
                    if matches!(c, '*' | '?' | '\\') {
                        text.push('\\');
                    }
                    text.push(c);
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '.' {
                    ensure!(!text.is_empty(), "empty component in payload path {input}");
                    let array = text == "#";
                    ensure!(
                        array || !text.contains('#'),
                        "unsupported payload predicate/projection in {input}"
                    );
                    parts.push(Part {
                        text: std::mem::take(&mut text),
                        wildcard,
                        array,
                        query: None,
                    });
                    wildcard = false;
                } else {
                    ensure!(
                        !matches!(c, '@' | '!' | '|' | '[' | ']' | '{' | '}' | '(' | ')'),
                        "unsupported payload path syntax in {input}"
                    );
                    wildcard |= matches!(c, '*' | '?');
                    text.push(c);
                }
            }
        }
        ensure!(
            !escaped && text.is_empty(),
            "trailing escape in payload path {input}"
        );
        Ok(Self(parts))
    }

    pub(super) fn resolve(&self, body: &Value) -> Vec<Vec<String>> {
        self.resolve_mode(body, false)
    }

    /// Go replacement object-key wildcards select one source child, whereas
    /// array projections/queries can explicitly select multiple source nodes.
    pub(super) fn resolve_replace(&self, body: &Value) -> Vec<Vec<String>> {
        self.resolve_mode(body, true)
    }

    fn resolve_mode(&self, body: &Value, first_wildcard: bool) -> Vec<Vec<String>> {
        fn visit(
            parts: &[Part],
            node: Option<&Value>,
            prefix: &mut Vec<String>,
            out: &mut Vec<Vec<String>>,
            first_wildcard: bool,
        ) {
            let Some((part, rest)) = parts.split_first() else {
                out.push(prefix.clone());
                return;
            };
            if part.array {
                if let Some(Value::Array(array)) = node {
                    for (i, value) in array.iter().enumerate() {
                        if part.query.as_ref().is_some_and(|(q, _)| !q.matches(value)) {
                            continue;
                        }
                        prefix.push(i.to_string());
                        visit(rest, Some(value), prefix, out, first_wildcard);
                        prefix.pop();
                        if part.query.as_ref().is_some_and(|(_, all)| !all) {
                            break;
                        }
                    }
                }
            } else if part.wildcard {
                if let Some(Value::Object(map)) = node {
                    for (key, value) in map {
                        if glob(&part.text, key) {
                            prefix.push(key.clone());
                            visit(rest, Some(value), prefix, out, first_wildcard);
                            prefix.pop();
                            if first_wildcard {
                                break;
                            }
                        }
                    }
                }
            } else {
                let key = unescape(&part.text);
                let next = node.and_then(|v| child(v, &key));
                prefix.push(key);
                visit(rest, next, prefix, out, first_wildcard);
                prefix.pop();
            }
        }
        let mut out = Vec::new();
        visit(
            &self.0,
            Some(body),
            &mut Vec::new(),
            &mut out,
            first_wildcard,
        );
        out
    }
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn child<'a>(node: &'a Value, key: &str) -> Option<&'a Value> {
    match node {
        Value::Object(map) => map.get(key),
        Value::Array(array) => key.parse::<usize>().ok().and_then(|i| array.get(i)),
        _ => None,
    }
}
pub(super) fn get<'a>(node: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(node, |v, key| child(v, key))
}

pub(super) fn set(node: &mut Value, path: &[String], value: Value) -> Result<()> {
    let Some((key, rest)) = path.split_first() else {
        *node = value;
        return Ok(());
    };
    if node.is_null() {
        *node = if key.parse::<usize>().is_ok() || key == "-1" {
            Value::Array(Vec::new())
        } else {
            Value::Object(Map::new())
        };
    }
    match node {
        Value::Object(map) => set(map.entry(key.clone()).or_insert(Value::Null), rest, value),
        Value::Array(array) => {
            let index = if key == "-1" {
                array.len()
            } else {
                key.parse::<usize>()
                    .map_err(|_| anyhow::anyhow!("non-numeric payload array index {key}"))?
            };
            // Bound sparse creation to avoid a tiny config allocating arbitrary memory.
            ensure!(
                index <= array.len().saturating_add(65_536),
                "payload array index exceeds sparse allocation bound"
            );
            if index >= array.len() {
                array.resize(index + 1, Value::Null);
            }
            set(&mut array[index], rest, value)
        }
        _ => bail!("payload path traverses a scalar"),
    }
}

pub(super) fn delete(node: &mut Value, path: &[String]) {
    let Some((key, rest)) = path.split_first() else {
        return;
    };
    match node {
        Value::Object(map) => {
            if rest.is_empty() {
                map.remove(key);
            } else if let Some(v) = map.get_mut(key) {
                delete(v, rest);
            }
        }
        Value::Array(array) => {
            if let Ok(index) = key.parse::<usize>()
                && index < array.len()
            {
                if rest.is_empty() {
                    array.remove(index);
                } else {
                    delete(&mut array[index], rest);
                }
            }
        }
        _ => {}
    }
}

// Linear-space glob matching, with '*' and '?' and escaped literals.
pub(super) fn glob(pattern: &str, text: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let mut previous = vec![false; text.len() + 1];
    previous[0] = true;
    let mut chars = pattern.chars();
    while let Some(mut c) = chars.next() {
        let escaped = c == '\\';
        if escaped {
            c = chars.next().unwrap_or('\\');
        }
        let mut next = vec![false; text.len() + 1];
        if c == '*' && !escaped {
            next[0] = previous[0];
            for i in 1..=text.len() {
                next[i] = previous[i] || next[i - 1];
            }
        } else {
            for i in 1..=text.len() {
                next[i] = previous[i - 1] && ((c == '?' && !escaped) || c == text[i - 1]);
            }
        }
        previous = next;
    }
    previous[text.len()]
}
