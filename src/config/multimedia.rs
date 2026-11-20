//! Go multimedia configuration and pre-payload image-tool policy.
use super::wire::field;
use anyhow::{Context, Result, bail};
use serde_json::Value as Json;
use serde_yaml::Value;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageGenerationMode {
    #[default]
    Off,
    All,
    Chat,
    Passthrough,
}

impl ImageGenerationMode {
    pub fn parse(value: Option<&Value>) -> Result<Self> {
        match value {
            None | Some(Value::Null) | Some(Value::Bool(false)) => Ok(Self::Off),
            Some(Value::Bool(true)) => Ok(Self::All),
            Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "" | "false" | "0" | "off" | "no" => Ok(Self::Off),
                "true" | "1" | "on" | "yes" => Ok(Self::All),
                "chat" => Ok(Self::Chat),
                "passthrough" => Ok(Self::Passthrough),
                _ => bail!("invalid disable-image-generation mode"),
            },
            // Go YAML decodes numeric scalars into strings before parsing.
            Some(Value::Number(n)) if n.as_i64() == Some(0) => Ok(Self::Off),
            Some(Value::Number(n)) if n.as_i64() == Some(1) => Ok(Self::All),
            _ => bail!("invalid disable-image-generation value"),
        }
    }

    /// Go Codex hosted-tool defaults, applied before user payload rules.
    pub fn codex_tools(self, body: &mut Json, model: &str, auth: &Json, headers: &http::HeaderMap) {
        if self != Self::Off {
            return;
        }
        let lite_header = auth
            .pointer("/attributes/header:X-OpenAI-Internal-Codex-Responses-Lite")
            .and_then(Json::as_str)
            .or_else(|| {
                headers
                    .get("x-openai-internal-codex-responses-lite")
                    .and_then(|v| v.to_str().ok())
            })
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("true"));
        let lite_body = body
            .pointer("/client_metadata/ws_request_header_x_openai_internal_codex_responses_lite")
            .is_some_and(|v| {
                v == true || v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("true"))
            });
        let reasoning = body
            .pointer("/reasoning/effort")
            .is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
            || model.contains('(')
            || body
                .get("input")
                .and_then(Json::as_array)
                .is_some_and(|items| {
                    items.iter().any(|item| {
                        item["type"] == "configuration_update"
                            && item
                                .pointer("/reasoning/effort")
                                .is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
                    })
                });
        let existing =
            body.get("tools")
                .and_then(Json::as_array)
                .is_some_and(|tools| {
                    tools.iter().any(|tool| {
                        tool["type"] == "image_generation"
                            || (tool["type"] == "function" && tool["name"] == "image_gen.imagegen")
                            || (tool["type"] == "namespace"
                                && tool["name"] == "image_gen"
                                && tool.get("tools").and_then(Json::as_array).is_some_and(
                                    |tools| {
                                        tools.iter().any(|t| {
                                            t["type"] == "function" && t["name"] == "imagegen"
                                        })
                                    },
                                ))
                    })
                });
        let free = auth
            .pointer("/attributes/plan_type")
            .and_then(Json::as_str)
            .is_some_and(|s| s.eq_ignore_ascii_case("free"));
        if !existing
            && !free
            && !lite_header
            && !lite_body
            && !(lite_model(model) && reasoning)
            && !model.to_ascii_lowercase().ends_with("spark")
        {
            if body.get("tools").is_none() {
                body["tools"] = serde_json::json!([]);
            }
            if let Some(tools) = body.get_mut("tools").and_then(Json::as_array_mut) {
                tools.push(serde_json::json!({"type":"image_generation","output_format":"png"}));
            }
        }
        if auth
            .pointer("/attributes/api_key")
            .and_then(Json::as_str)
            .is_none_or(str::is_empty)
            && let Some(tools) = body.get_mut("tools").and_then(Json::as_array_mut)
        {
            for tool in tools {
                if tool["type"] == "image_generation"
                    && let Some(object) = tool.as_object_mut()
                {
                    object.remove("complexity");
                }
            }
        }
    }

    /// Filtering precedes user payload rules, which may explicitly restore tools.
    pub fn filter(self, body: &mut Json) {
        if !matches!(self, Self::All | Self::Chat) {
            return;
        }

        if let Some(tools) = body.get_mut("tools").and_then(Json::as_array_mut) {
            tools
                .retain(|tool| tool.get("type").and_then(Json::as_str) != Some("image_generation"));
        }
        let selected = body.get("tool_choice").is_some_and(|choice| {
            choice
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("image_generation"))
                || choice
                    .get("type")
                    .and_then(Json::as_str)
                    .is_some_and(|s| s.eq_ignore_ascii_case("image_generation"))
                || (choice.get("type").and_then(Json::as_str) == Some("tool")
                    && choice.get("name").and_then(Json::as_str) == Some("image_generation"))
        });
        if selected && let Some(object) = body.as_object_mut() {
            object.remove("tool_choice");
        }
    }
}

pub(super) fn base_model(raw: &Value) -> Result<String> {
    let model = field(raw, "gpt-image-2-base-model")
        .filter(|v| !v.is_null())
        .map(|v| {
            v.as_str()
                .context("gpt-image-2-base-model must be a string")
        })
        .transpose()?
        .unwrap_or("")
        .trim();
    Ok(
        if model.to_ascii_lowercase().starts_with("gpt-") && !lite_model(model) {
            model.to_owned()
        } else {
            "gpt-5.4-mini".into()
        },
    )
}

pub fn lite_model(model: &str) -> bool {
    model
        .split('(')
        .next()
        .unwrap_or(model)
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .trim()
        .to_ascii_lowercase()
        .starts_with("gpt-6")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use serde_json::json;

    #[test]
    fn go_modes_and_layouts() {
        for (value, expected) in [
            ("false", ImageGenerationMode::Off),
            ("true", ImageGenerationMode::All),
            ("null", ImageGenerationMode::Off),
            ("0", ImageGenerationMode::Off),
            ("1", ImageGenerationMode::All),
            ("' OFF '", ImageGenerationMode::Off),
            ("'no'", ImageGenerationMode::Off),
            ("'false'", ImageGenerationMode::Off),
            ("'0'", ImageGenerationMode::Off),
            ("''", ImageGenerationMode::Off),
            ("' YES '", ImageGenerationMode::All),
            ("'on'", ImageGenerationMode::All),
            ("'true'", ImageGenerationMode::All),
            ("'1'", ImageGenerationMode::All),
            ("' CHAT '", ImageGenerationMode::Chat),
            ("passthrough", ImageGenerationMode::Passthrough),
        ] {
            for prefix in ["", "multimedia:\n  "] {
                let yaml = format!(
                    "credential-concurrency:\n  lifecycle-config-revision: 1\n{prefix}disable-image-generation: {value}\n"
                );
                let config = Config::parse(yaml.as_bytes()).unwrap();
                assert_eq!(config.disable_image_generation, expected);
                assert!(config.unsupported_features().is_empty());
            }
        }
        for value in ["invalid", "[]", "{}", "2"] {
            assert!(
                ImageGenerationMode::parse(Some(&serde_yaml::from_str(value).unwrap())).is_err()
            );
        }
    }

    #[test]
    fn filter_preserves_other_tools_and_passthrough() {
        let original = json!({"tools":[{"type":"image_generation"},{"type":"function","name":"f"}],"tool_choice":{"type":"image_generation"}});
        for mode in [ImageGenerationMode::All, ImageGenerationMode::Chat] {
            let mut body = original.clone();
            mode.filter(&mut body);
            assert_eq!(body["tools"], json!([{"type":"function","name":"f"}]));
            assert!(body.get("tool_choice").is_none());
        }
        for mode in [ImageGenerationMode::Off, ImageGenerationMode::Passthrough] {
            let mut body = original.clone();
            mode.filter(&mut body);
            assert_eq!(body, original);
        }
    }

    #[test]
    fn remaining_multimedia_settings_stay_gated() {
        let config = Config::parse(b"credential-concurrency:\n  lifecycle-config-revision: 1\nmultimedia:\n  gpt-image-2-base-model: gpt-5.5\n  video-result-auth-cache-ttl: 1h\n  unknown-control: true\n").unwrap();
        assert_eq!(config.gpt_image_2_base_model, "gpt-5.5");
        assert_eq!(
            config.unsupported_features(),
            vec![
                "multimedia.unknown-control",
                "multimedia.video-result-auth-cache-ttl"
            ]
        );
        for model in ["", "other", "gpt-6-sol"] {
            let raw = serde_yaml::from_str(&format!("gpt-image-2-base-model: '{model}'")).unwrap();
            assert_eq!(base_model(&raw).unwrap(), "gpt-5.4-mini");
        }
    }
}
