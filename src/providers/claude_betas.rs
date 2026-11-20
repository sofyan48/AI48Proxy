//! Basic main-thread OAuth baseline from Go's claudeCodeCLIBetas.
//! This is not the full probe/subagent or advanced-tool capability engine.
use serde_json::Value;

fn canonical(model: &str) -> String {
    model
        .trim()
        .to_lowercase()
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_owned()
}

fn variant(model: &str, prefix: &str) -> bool {
    model == prefix
        || model
            .strip_prefix(prefix)
            .is_some_and(|tail| tail.starts_with('-') || tail.starts_with('['))
}

pub(super) fn is_haiku55(model: &str) -> bool {
    let model = canonical(model);
    variant(&model, "claude-haiku-5-5") || variant(&model, "claude-haiku-5.5")
}

pub(super) fn baseline(body: &Value) -> Vec<String> {
    let model = canonical(body["model"].as_str().unwrap_or(""));
    let haiku55 = is_haiku55(&model);
    // Opus deliberately does not accept a dated suffix in Go's helper.
    let opus55 = model == "claude-opus-5-5" || model.starts_with("claude-opus-5-5[");
    let sonnet55 = variant(&model, "claude-sonnet-5-5");
    let sonnet5 = !sonnet55 && variant(&model, "claude-sonnet-5");
    let fable52 = variant(&model, "claude-fable-5-2") || variant(&model, "claude-fable-5.2");
    let per_turn =
        opus55 || sonnet55 || haiku55 || fable52 || model.starts_with("claude-fable-5-1");
    let fable51 = ["fable-5-1", "fable-5.1", "mythos-5-1", "mythos-5.1"]
        .iter()
        .any(|target| {
            model.find(target).is_some_and(|index| {
                model
                    .as_bytes()
                    .get(index + target.len())
                    .is_none_or(|next| !next.is_ascii_digit())
            })
        });
    let progress = opus55 || sonnet55 || haiku55 || sonnet5 || fable52 || fable51;
    let legacy = is_legacy(&model);
    let mut betas = Vec::new();
    if !haiku55 {
        betas.push("claude-code-20250219");
    }
    betas.extend(["oauth-2025-04-20", "interleaved-thinking-2025-05-14"]);
    if !body
        .pointer("/thinking/display")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
    {
        betas.push("redact-thinking-2026-02-12");
    }
    betas.extend([
        "context-management-2025-06-27",
        "prompt-caching-scope-2026-01-05",
    ]);
    if haiku55 {
        betas.push("claude-code-20250219");
    }
    if per_turn {
        betas.push("per-turn-control-2026-07-01");
    }
    if !legacy && !sonnet5 {
        betas.push("mid-conversation-tool-changes-2026-07-01");
    }
    betas.push("thinking-token-count-2026-05-13");
    if !legacy && progress {
        betas.push("mid-conversation-system-clear-at-2026-08-21");
    }
    let thinking = body
        .pointer("/thinking/type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if (!model.contains("haiku") || haiku55) && thinking != "disabled" {
        betas.push("effort-2025-11-24");
    }
    betas.push("extended-cache-ttl-2025-04-11");
    betas.into_iter().map(str::to_owned).collect()
}

pub(super) fn uses_progress(model: &str) -> bool {
    let model = canonical(model);
    let fable51 = ["fable-5-1", "fable-5.1", "mythos-5-1", "mythos-5.1"]
        .iter()
        .any(|target| {
            model.find(target).is_some_and(|i| {
                model
                    .as_bytes()
                    .get(i + target.len())
                    .is_none_or(|b| !b.is_ascii_digit())
            })
        });
    model == "claude-opus-5-5"
        || model.starts_with("claude-opus-5-5[")
        || variant(&model, "claude-sonnet-5")
        || is_haiku55(&model)
        || variant(&model, "claude-fable-5-2")
        || variant(&model, "claude-fable-5.2")
        || fable51
}

pub(super) fn is_legacy(model: &str) -> bool {
    let model = canonical(model);
    matches!(
        model.as_str(),
        "claude-3-5-haiku-20241022"
            | "claude-3-5-haiku-latest"
            | "claude-3-7-sonnet-20250219"
            | "claude-3-7-sonnet-latest"
            | "claude-haiku-4-5"
            | "claude-haiku-4-5-20251001"
            | "claude-opus-4"
            | "claude-opus-4-20250514"
            | "claude-opus-4-1"
            | "claude-opus-4-1-20250805"
            | "claude-opus-4-5"
            | "claude-opus-4-5-20251101"
            | "claude-opus-4-6"
            | "claude-opus-4-7"
            | "claude-sonnet-4"
            | "claude-sonnet-4-20250514"
            | "claude-sonnet-4-5"
            | "claude-sonnet-4-5-20250929"
            | "claude-sonnet-4-6"
    )
}
