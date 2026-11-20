use anyhow::{Result, bail, ensure};
use axum::http::HeaderMap;
use serde_json::Value;

use crate::{config::Config, home::client::Selection, providers, thinking};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    OpenAi,
    Claude,
    Gemini,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResponseKind {
    Native,
    Antigravity,
    CountTokens,
    ResponsesChat,
    ResponsesChatUsage,
    ChatResponses,
    GeminiChat {
        antigravity: bool,
        include_usage: bool,
    },
}
impl ResponseKind {
    pub fn responses_chat(self) -> bool {
        matches!(self, Self::ResponsesChat | Self::ResponsesChatUsage)
    }
}

pub(crate) struct Prepared {
    pub response_kind: ResponseKind,
    pub request: reqwest::RequestBuilder,
    pub stream: bool,
    pub response_mode: providers::ResponseMode,
    pub format: Format,
    pub translation: Option<(crate::protocol::Format, crate::protocol::Format)>,
    pub response_model: String,
}

pub(crate) fn prepare(
    config: &Config,
    selection: &Selection,
    path: &str,
    headers: &HeaderMap,
    body: Value,
    requested_stream: bool,
) -> Result<Prepared> {
    let incoming = format(path)?;
    let provider = selection.provider.to_ascii_lowercase();
    if path == "/v1/messages/count_tokens" {
        return super::count_tokens::prepare(config, selection, headers, body, requested_stream);
    }

    if matches!(provider.as_str(), "xai" | "meta" | "kimi" | "kimi-ai") {
        ensure!(
            matches!(path, "/v1/chat/completions" | "/v1/responses"),
            "provider source format unsupported: only Chat and Responses are implemented; Claude/Gemini delegation requires provider replay bridges"
        );
    }
    let requested_model = if incoming == Format::Gemini {
        path.strip_prefix("/v1beta/models/")
            .and_then(|p| p.split_once(':'))
            .map(|(m, _)| m)
            .unwrap_or(&selection.model)
    } else {
        body.get("model")
            .and_then(Value::as_str)
            .unwrap_or(&selection.model)
    };
    let outgoing = match provider.as_str() {
        "claude" => Format::Claude,
        "gemini" | "antigravity" => Format::Gemini,
        "vertex" | "vertexai" => {
            if providers::vertex::uses_openapi(selection, requested_model)? {
                Format::OpenAi
            } else {
                Format::Gemini
            }
        }
        "codex" => {
            ensure!(
                matches!(
                    path,
                    "/v1/responses" | "/v1/responses/compact" | "/v1/chat/completions"
                ),
                "Codex requires Responses or Chat input"
            );
            Format::OpenAi
        }
        "gemini-cli" | "qwen" | "iflow" | "kiro" | "github-copilot" => {
            bail!("provider executor unsupported")
        }
        _ => Format::OpenAi,
    };
    let original_model = if incoming == Format::Gemini {
        path.strip_prefix("/v1beta/models/")
            .and_then(|p| p.split_once(':'))
            .map(|(m, _)| m)
            .unwrap_or(&selection.model)
    } else {
        body.get("model")
            .and_then(Value::as_str)
            .unwrap_or(&selection.model)
    };
    let response_model = original_model.to_owned();
    let original_body = body.clone();
    let native_responses = matches!(path, "/v1/responses" | "/v1/responses/compact");
    if path == "/v1/responses/compact" {
        ensure!(
            !requested_stream,
            crate::protocol::TranslationError::Invalid("compact does not support streaming".into())
        );
        ensure!(
            matches!(
                provider.as_str(),
                "codex" | "openai" | "openai-compatibility"
            ) || outgoing == Format::OpenAi,
            "compact provider unsupported"
        );
    }
    let mut thinking_body = body.clone();
    // The canonical thinking helper currently understands Chat effort, not Responses.
    // Adapt only effort for capability validation, preserving the rest of native reasoning.
    let reasoning = if native_responses {
        thinking_body
            .as_object_mut()
            .and_then(|o| o.remove("reasoning"))
    } else {
        None
    };
    if let Some(reasoning) = reasoning.as_ref() {
        ensure!(
            reasoning.is_object(),
            crate::protocol::TranslationError::Invalid("reasoning must be an object".into())
        );
    }
    if let Some(effort) = reasoning.as_ref().and_then(|r| r.get("effort")) {
        thinking_body["reasoning_effort"] = effort.clone();
    }
    let mut body = thinking::apply(
        original_model,
        match incoming {
            Format::OpenAi => thinking::Provider::OpenAiChat,
            Format::Claude => thinking::Provider::Claude,
            Format::Gemini => thinking::Provider::Gemini,
        },
        &thinking_body,
        selection.model_info.as_ref(),
    )?
    .body;
    if native_responses {
        let effort = body
            .as_object_mut()
            .and_then(|o| o.remove("reasoning_effort"));
        if let Some(reasoning) = reasoning {
            body["reasoning"] = reasoning;
        }
        if let Some(effort) = effort {
            body["reasoning"]["effort"] = effort;
        }
    }
    let gemini_chat =
        incoming == Format::OpenAi && outgoing == Format::Gemini && path == "/v1/chat/completions";
    let translation = if gemini_chat {
        body = if provider == "antigravity" {
            crate::protocol::gemini::antigravity_chat_request(&body, &selection.model)?
        } else {
            crate::protocol::gemini::chat_request(&body, &selection.model)?
        };
        None
    } else if incoming != outgoing {
        ensure!(
            matches!(path, "/v1/chat/completions" | "/v1/messages"),
            "unsupported cross-format route"
        );
        let from = protocol_format(incoming)?;
        let to = protocol_format(outgoing)?;
        body = crate::protocol::translate_request(
            from,
            to,
            &body,
            crate::protocol::RequestOptions {
                target_model: &selection.model,
                default_max_tokens: Some(4096),
            },
        )?;
        Some((to, from))
    } else {
        None
    };
    // Home's compatibility executor is Chat-native, including selections named
    // `openai`. Credential/model options remain on Selection for provider-side
    // token/modalities normalization; never infer native Responses from model names.
    let chat_responses = path == "/v1/responses"
        && outgoing == Format::OpenAi
        && !matches!(
            provider.as_str(),
            "codex" | "vertex" | "vertexai" | "xai" | "meta" | "kimi" | "kimi-ai"
        );
    if chat_responses {
        body = crate::protocol::responses::responses_request(&body, &selection.model)?;
    }
    let responses_chat =
        matches!(provider.as_str(), "codex" | "xai" | "meta") && path == "/v1/chat/completions";
    if responses_chat {
        body = crate::protocol::responses::chat_request(&body, &selection.model)?;
    }
    let gemini_path = format!(
        "/v1beta/models/{}:{}",
        selection.model,
        if requested_stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        }
    );
    let native_path = if gemini_chat {
        gemini_path.as_str()
    } else if responses_chat {
        "/v1/responses"
    } else if chat_responses {
        "/v1/chat/completions"
    } else if translation.is_some() {
        match outgoing {
            Format::Claude => "/v1/messages",
            Format::OpenAi => "/v1/chat/completions",
            _ => unreachable!(),
        }
    } else {
        path
    };
    // Named compatibility providers use OpenAI execution only with an explicit trusted base.
    let mut native: Selection = serde_json::from_value(serde_json::json!({
        "model":selection.model,"provider":selection.provider,"auth_index":selection.auth_index,
        "auth":selection.auth,"model_info":selection.model_info,"request_retry":selection.request_retry
    }))?;
    if !matches!(
        provider.as_str(),
        "openai"
            | "openai-compatibility"
            | "claude"
            | "gemini"
            | "codex"
            | "vertex"
            | "vertexai"
            | "antigravity"
            | "xai"
            | "meta"
            | "kimi"
            | "kimi-ai"
    ) {
        ensure!(
            selection
                .auth
                .pointer("/attributes/base_url")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty()),
            "compatibility provider requires base_url"
        );
        native.provider = "openai-compatibility".into();
    }
    // Go injects only in Off mode; compact never gets automatic image tools.
    if provider == "codex" && path != "/v1/responses/compact" {
        config.disable_image_generation.codex_tools(
            &mut body,
            &selection.model,
            &selection.auth,
            headers,
        );
    }
    // Go filters hosted image tools before provider payload overrides.
    config.disable_image_generation.filter(&mut body);
    // Provider normalization and credential headers precede its single final payload barrier.
    let prepared = providers::prepare_context(
        config,
        &native,
        native_path,
        headers,
        body,
        requested_stream,
        Some(crate::payload::ApplyContext {
            model: &selection.model,
            protocol: "",
            from_protocol: if native_responses {
                "openai-response"
            } else {
                match incoming {
                    Format::OpenAi => "openai",
                    Format::Claude => "claude",
                    Format::Gemini => "gemini",
                }
            },
            headers: Some(headers),
            requested_model: &response_model,
            root: "",
            original: Some(&original_body),
        }),
    )?;
    let mut request = prepared.request;
    for name in super::SESSION_HEADERS {
        for value in headers.get_all(*name) {
            request = request.header(*name, value);
        }
    }
    Ok(Prepared {
        response_kind: if gemini_chat {
            ResponseKind::GeminiChat {
                antigravity: provider == "antigravity",
                include_usage: original_body
                    .pointer("/stream_options/include_usage")
                    .and_then(Value::as_bool)
                    == Some(true),
            }
        } else if responses_chat {
            if original_body
                .pointer("/stream_options/include_usage")
                .and_then(Value::as_bool)
                == Some(true)
            {
                ResponseKind::ResponsesChatUsage
            } else {
                ResponseKind::ResponsesChat
            }
        } else if chat_responses {
            ResponseKind::ChatResponses
        } else if provider == "antigravity" {
            ResponseKind::Antigravity
        } else {
            ResponseKind::Native
        },
        request,
        stream: prepared.stream,
        response_mode: prepared.response_mode,
        format: outgoing,
        translation,
        response_model,
    })
}

#[cfg(test)]
#[path = "google_translation_prepare_tests.rs"]
pub(crate) mod google_translation_prepare_tests;

fn protocol_format(format: Format) -> Result<crate::protocol::Format> {
    match format {
        Format::OpenAi => Ok(crate::protocol::Format::OpenAiChat),
        Format::Claude => Ok(crate::protocol::Format::ClaudeMessages),
        Format::Gemini => bail!("Gemini cross-format translation not implemented"),
    }
}
pub(super) fn format(path: &str) -> Result<Format> {
    match path {
        "/v1/chat/completions" | "/v1/completions" | "/v1/responses" | "/v1/responses/compact" => {
            Ok(Format::OpenAi)
        }
        "/v1/messages" | "/v1/messages/count_tokens" => Ok(Format::Claude),
        p if p.starts_with("/v1beta/models/")
            && (p.ends_with(":generateContent") || p.ends_with(":streamGenerateContent")) =>
        {
            Ok(Format::Gemini)
        }
        _ => bail!("unsupported route"),
    }
}
