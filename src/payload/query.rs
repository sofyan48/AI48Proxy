use super::{Value, matching::equal, path};
use anyhow::{Result, ensure};

pub(super) fn split(input: &str, delimiter: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut start, mut depth, mut quote, mut escape) = (0, 0, None, false);
    for (i, c) in input.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if c == '\\' {
            escape = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
            continue;
        }
        if c == '(' {
            depth += 1;
        }
        if c == ')' {
            depth -= 1;
        }
        if depth == 0 && i >= start && input[i..].starts_with(delimiter) {
            out.push(input[start..i].to_owned());
            start = i + delimiter.len();
        }
    }
    out.push(input[start..].to_owned());
    out
}

#[derive(Clone, Debug)]
pub(super) struct Query(Vec<Vec<Term>>);
#[derive(Clone, Debug)]
struct Term {
    path: Option<path::Path>,
    op: String,
    value: Value,
}
impl Query {
    pub(super) fn parse(s: &str) -> Result<Self> {
        let mut ors = Vec::new();
        for group in split(s, "||") {
            let mut ands = Vec::new();
            for term in split(&group, "&&") {
                let term = term.trim();
                ensure!(!term.is_empty(), "empty query term");
                let mut selected = None;
                // Operators inside quoted keys/values must not split the predicate.
                let mut quote = None;
                let mut escaped = false;
                for (i, c) in term.char_indices() {
                    if escaped {
                        escaped = false;
                        continue;
                    }
                    if c == '\\' {
                        escaped = true;
                        continue;
                    }
                    if let Some(q) = quote {
                        if c == q {
                            quote = None;
                        }
                        continue;
                    }
                    if c == '"' || c == '\'' {
                        quote = Some(c);
                        continue;
                    }
                    if let Some(op) = ["==", "!=", ">=", "<=", "!%", "%", ">", "<"]
                        .into_iter()
                        .find(|op| term[i..].starts_with(op))
                    {
                        selected = Some((i, op));
                        break;
                    }
                }
                let (key, op, value) = if let Some((i, op)) = selected {
                    let raw = term[i + op.len()..].trim();
                    ensure!(!raw.is_empty(), "query comparison requires a value");
                    let value = serde_json::from_str(raw)
                        .unwrap_or_else(|_| Value::String(raw.trim_matches('\'').to_owned()));
                    (&term[..i], op.to_owned(), value)
                } else {
                    (term, String::new(), Value::Null)
                };
                let key = key.trim();
                let path = if key.is_empty() || key == "@this" {
                    None
                } else {
                    Some(path::Path::parse(key)?)
                };
                ands.push(Term { path, op, value });
            }
            ors.push(ands);
        }
        Ok(Self(ors))
    }
    pub(super) fn matches(&self, item: &Value) -> bool {
        self.0.iter().any(|terms| {
            terms.iter().all(|t| {
                let values = if let Some(p) = &t.path {
                    p.resolve(item)
                        .iter()
                        .filter_map(|p| path::get(item, p))
                        .collect()
                } else {
                    vec![item]
                };
                values.iter().any(|actual| match t.op.as_str() {
                    "" => true,
                    "==" => equal(actual, &t.value),
                    "!=" => !equal(actual, &t.value),
                    "%" | "!%" => actual
                        .as_str()
                        .zip(t.value.as_str())
                        .is_some_and(|(a, b)| path::glob(b, a) == (t.op == "%")),
                    op => {
                        let order = if let (Some(a), Some(b)) = (actual.as_f64(), t.value.as_f64())
                        {
                            a.partial_cmp(&b)
                        } else if let (Some(a), Some(b)) = (actual.as_str(), t.value.as_str()) {
                            Some(a.cmp(b))
                        } else {
                            None
                        };
                        order.is_some_and(|o| match op {
                            ">" => o.is_gt(),
                            "<" => o.is_lt(),
                            ">=" => o.is_ge(),
                            "<=" => o.is_le(),
                            _ => false,
                        })
                    }
                })
            })
        })
    }
}
