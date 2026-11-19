//! Vertex execution using Home-dispatched tokens/keys, never service-account signing.
use super::*;

pub fn uses_openapi(selection: &Selection, requested_model: &str) -> Result<bool> {
    let auth = &selection.auth;
    if selection.model.to_ascii_lowercase().contains("imagen") {
        return Ok(false);
    }
    let raw = text(auth, "/attributes/vertex_model_openapi");
    if !raw.is_empty() {
        let rules: Vec<Value> = serde_json::from_str(raw)
            .map_err(|_| anyhow::anyhow!("invalid Home Vertex routing rules"))?;
        // Alias-specific choices must not leak to a different alias of the same model.
        let model = selection.model.as_str();
        let target = if requested_model.is_empty() {
            model
        } else {
            requested_model
        };
        for base in [false, true] {
            for rule in &rules {
                let alias = rule.get("alias").and_then(Value::as_str).unwrap_or("");
                let name = rule.get("name").and_then(Value::as_str).unwrap_or("");
                if model_match(target, alias, base)
                    && (name.is_empty()
                        || model_match(model, alias, true)
                        || model_match(model, name, true))
                    && let Some(setting) = rule.get("openapi").and_then(Value::as_bool)
                {
                    return Ok(setting);
                }
            }
        }
        for base in [false, true] {
            for rule in &rules {
                let alias = rule.get("alias").and_then(Value::as_str).unwrap_or("");
                let name = rule.get("name").and_then(Value::as_str).unwrap_or("");
                let can_match = alias.is_empty()
                    || alias.eq_ignore_ascii_case(name)
                    || requested_model.is_empty()
                    || model_match(requested_model, alias, true)
                    || model_match(requested_model, name, true);
                if can_match
                    && (model_match(model, name, base) || model_match(requested_model, name, base))
                    && let Some(setting) = rule.get("openapi").and_then(Value::as_bool)
                {
                    return Ok(setting);
                }
            }
        }
    }
    for path in ["/attributes/openapi", "/metadata/openapi"] {
        if let Some(value) = auth.pointer(path) {
            let setting =
                value
                    .as_bool()
                    .or_else(|| match value.as_str()?.to_ascii_lowercase().as_str() {
                        "true" | "1" => Some(true),
                        "false" | "0" => Some(false),
                        _ => None,
                    });
            if let Some(setting) = setting {
                return Ok(setting);
            }
        }
    }
    let model = normalize_model(&selection.model);
    Ok(model
        .split_once('/')
        .is_some_and(|(publisher, _)| publisher != "google"))
}

pub(super) fn credentials(
    selection: &Selection,
    protocol: &str,
    headers: &mut HeaderMap,
) -> Result<(String, bool)> {
    ensure!(
        matches!(protocol, "gemini" | "openai"),
        "Vertex requires Gemini or OpenAI Chat input"
    );
    let auth = &selection.auth;
    let key = text(auth, "/attributes/api_key");
    let oauth = key.is_empty();
    if oauth {
        bearer(headers, access_token(auth))?;
    } else if protocol == "openai" {
        bearer(headers, key)?;
    } else {
        insert(headers, "x-goog-api-key", key)?;
    }
    let base = field(auth, "base_url");
    let location = if protocol == "openai" {
        openapi_location(auth)
    } else {
        location(auth)
    };
    segment(location)?;
    let base = if !base.is_empty() {
        base.to_owned()
    } else if location.eq_ignore_ascii_case("global") {
        "https://aiplatform.googleapis.com".into()
    } else if matches!(location, "us" | "eu") {
        format!("https://aiplatform.{location}.rep.googleapis.com")
    } else {
        format!("https://{location}-aiplatform.googleapis.com")
    };
    validate_base(&base)?;
    if oauth && !Url::parse(&base)?.path().contains("/projects/") {
        segment(project(auth))?;
    }
    if !oauth && protocol == "openai" {
        let host = Url::parse(&base)?.host_str().unwrap_or_default().to_owned();
        ensure!(
            !(host == "aiplatform.googleapis.com"
                || host.ends_with("-aiplatform.googleapis.com")
                || host.ends_with(".rep.googleapis.com")),
            "Vertex endpoints/openapi on Google requires a Home access token"
        );
    }
    Ok((base, oauth))
}

pub(super) fn url(
    creds: &Credentials,
    selection: &Selection,
    openapi: bool,
    stream: bool,
) -> Result<Url> {
    let auth = &selection.auth;
    let mut url = Url::parse(&creds.base_url)?;
    let mut root = url.path().trim_end_matches('/').to_owned();
    let location = if openapi {
        openapi_location(auth)
    } else {
        location(auth)
    };
    let project = project(auth);
    if openapi && root.contains("/locations/") {
        let explicit = explicit_location(auth);
        if !explicit.is_empty() {
            segment(explicit)?;
            let start = root.find("/locations/").unwrap() + "/locations/".len();
            let end = root[start..]
                .find('/')
                .map(|index| start + index)
                .unwrap_or(root.len());
            root.replace_range(start..end, explicit);
        }
    }
    if openapi {
        if let Some(index) = root.find("/publishers/") {
            root.truncate(index);
        }
        if !root.ends_with("/endpoints/openapi") {
            if !root.contains("/projects/") {
                version(&mut root);
            }
            if !root.contains("/projects/") && (!project.is_empty() || creds.oauth) {
                segment(project)?;
                root.push_str(&format!("/projects/{project}/locations/{location}"));
            }
            root.push_str("/endpoints/openapi");
        }
        root.push_str("/chat/completions");
    } else {
        segment(&selection.model)?;
        ensure!(
            !selection.model.to_ascii_lowercase().contains("imagen"),
            "Vertex Imagen request conversion unsupported"
        );
        if !root.contains("/publishers/") {
            if !root.contains("/projects/") {
                version(&mut root);
            }
            if creds.oauth && !root.contains("/projects/") {
                segment(project)?;
                root.push_str(&format!("/projects/{project}/locations/{location}"));
            }
            root.push_str("/publishers/google/models");
        }
        root.push_str(&format!(
            "/{}:{}",
            selection.model,
            if stream {
                "streamGenerateContent"
            } else {
                "generateContent"
            }
        ));
    }
    url.set_path(&root);
    if stream && !openapi {
        url.query_pairs_mut().append_pair("alt", "sse");
    }
    Ok(url)
}

pub(super) fn normalize(body: &mut Value, model: &str, openapi: bool) -> Result<()> {
    if openapi {
        ensure!(
            body.get("messages").is_some_and(Value::is_array),
            "Vertex OpenAPI requires Chat messages"
        );
        body["model"] = normalize_model(model).into();
    } else {
        ensure!(
            body.get("contents").is_some_and(Value::is_array),
            "Vertex requires native Gemini contents"
        );
        body["model"] = model.into();
        body.as_object_mut().unwrap().remove("session_id");
    }
    Ok(())
}

fn normalize_model(model: &str) -> String {
    if let Some(rest) = model.strip_prefix("publishers/")
        && let Some((publisher, id)) = rest.split_once("/models/")
    {
        return format!("{publisher}/{id}");
    }
    if let Some(id) = model.strip_prefix("models/") {
        return format!("google/{id}");
    }
    if model.contains('/') {
        model.into()
    } else {
        format!("google/{model}")
    }
}
fn model_match(value: &str, rule: &str, base: bool) -> bool {
    let value = value.trim();
    let rule = rule.trim();
    if value.is_empty() || rule.is_empty() {
        return false;
    }
    if value.eq_ignore_ascii_case(rule) {
        return true;
    }
    base && !rule.ends_with(')')
        && value
            .split('(')
            .next()
            .unwrap_or(value)
            .eq_ignore_ascii_case(rule)
}
fn version(root: &mut String) {
    let last = root.rsplit('/').next().unwrap_or_default();
    if !(last.starts_with('v') && last.bytes().any(|c| c.is_ascii_digit())) {
        root.push_str("/v1");
    }
}
fn project(auth: &Value) -> &str {
    [field(auth, "project_id"), field(auth, "project")]
        .into_iter()
        .find(|v| !v.is_empty())
        .unwrap_or("")
}
fn explicit_location(auth: &Value) -> &str {
    [
        field(auth, "openapi_location"),
        field(auth, "openapi-location"),
    ]
    .into_iter()
    .find(|v| !v.is_empty())
    .unwrap_or("")
}
fn openapi_location(auth: &Value) -> &str {
    let explicit = explicit_location(auth);
    if explicit.is_empty() {
        location(auth)
    } else {
        explicit
    }
}
fn location(auth: &Value) -> &str {
    let value = field(auth, "location");
    if value.is_empty() { "global" } else { value }
}
fn field<'a>(auth: &'a Value, key: &str) -> &'a str {
    [format!("/attributes/{key}"), format!("/metadata/{key}")]
        .iter()
        .map(|path| text(auth, path))
        .find(|v| !v.is_empty())
        .unwrap_or("")
}
fn segment(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            && !matches!(value, "." | ".."),
        "invalid Vertex resource segment"
    );
    Ok(())
}
