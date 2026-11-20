//! Home-dispatched multimedia endpoints. Unsupported executors fail before upstream I/O.
use super::{error, lifecycle, rejection, upstream::Format, usage::Usage};
use crate::{
    config::runtime_controls::{BodyReadError, RuntimeControls},
    home::client::HomeClient,
};
use axum::{
    Json,
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{StreamExt, stream};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use std::sync::Arc;
#[path = "media_images.rs"]
mod images;
#[path = "media_multipart.rs"]
mod multipart;
#[path = "../providers/media.rs"]
mod provider;
#[cfg(test)]
#[path = "media_provider_tests.rs"]
mod provider_tests;
#[path = "media_retry.rs"]
mod retry;
#[path = "media_speech.rs"]
mod speech;
#[cfg(test)]
#[path = "media_tests.rs"]
mod tests;
#[path = "media_video.rs"]
mod video;

pub(super) async fn unsupported(state: State<Arc<HomeClient>>, request: Request) -> Response {
    video::handle(state, request).await
}

pub(super) async fn handle(State(home): State<Arc<HomeClient>>, request: Request) -> Response {
    if !home.ready() || home.cancellation().is_cancelled() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "not_ready",
            "Home is not ready",
        );
    }
    let (parts, body) = request.into_parts();
    let path = parts.uri.path();
    let speech_route = matches!(path, "/v1/audio/speech" | "/v1/tts");
    let config = home.config();
    if !speech_route
        && config.disable_image_generation == crate::config::multimedia::ImageGenerationMode::All
    {
        return error(
            StatusCode::NOT_FOUND,
            "not_found",
            "Image generation is disabled",
        );
    }
    let mut controls = RuntimeControls::from(config.as_ref());
    if speech_route {
        controls.max_request_body_bytes = Some(
            controls
                .max_request_body_bytes
                .unwrap_or(usize::MAX)
                .min(1 << 20),
        );
    }
    let length = parts
        .headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let bytes = match controls.read_body(body, length).await {
        Ok(bytes) => bytes,
        Err(BodyReadError::TooLarge) => {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "Request exceeds configured body limit",
            );
        }
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Unable to read request",
            );
        }
    };
    let content_type = parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let media_type = content_type.split(';').next().unwrap_or("").trim();
    let form =
        if path == "/v1/images/edits" && media_type.eq_ignore_ascii_case("multipart/form-data") {
            match multipart::Form::parse(content_type, &bytes) {
                Ok(form) => Some(form),
                Err(_) => {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        "Invalid multipart image form",
                    );
                }
            }
        } else {
            None
        };
    if form.is_none()
        && !(media_type.eq_ignore_ascii_case("application/json") || media_type.ends_with("+json"))
    {
        return error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_request_error",
            "Expected JSON or multipart image edits",
        );
    }
    let original = match form
        .as_ref()
        .map(multipart::Form::fields)
        .unwrap_or_else(|| serde_json::from_slice(&bytes).map_err(Into::into))
    {
        Ok(body) if body.is_object() => body,
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Expected a valid JSON object or form",
            );
        }
    };
    if original.get("stream").is_some_and(|v| !v.is_boolean()) {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "stream must be boolean",
        );
    }
    let image_stream =
        !speech_route && original.get("stream").and_then(Value::as_bool) == Some(true);
    let (model, payload, audio_type) = if speech_route {
        match speech::prepare(&original) {
            Ok(prepared) => prepared,
            Err(_) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "Invalid speech input, model, or output format; use grok-tts with mp3, wav, or pcm",
                );
            }
        }
    } else {
        if original
            .get("prompt")
            .and_then(Value::as_str)
            .is_none_or(|s| s.trim().is_empty())
        {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "prompt is required",
            );
        }
        if let Some(form) = &form
            && form.images().is_empty()
        {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "image is required",
            );
        }
        let model = original
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("gpt-image-2");
        (model.to_owned(), original.clone(), "")
    };

    #[allow(clippy::result_large_err)]
    let prepare = |selection: &crate::home::client::Selection| {
        let xai = selection.provider == "xai";
        let codex = selection.provider == "codex";
        if image_stream && !xai {
            return Err(error(
                StatusCode::NOT_IMPLEMENTED,
                "unsupported_capability",
                "Native compatible and Codex image SSE is not implemented",
            ));
        }

        if (speech_route && !xai)
            || (!speech_route && !xai && !codex && !provider::compat(selection))
        {
            return Err(error(
                StatusCode::NOT_IMPLEMENTED,
                "unsupported_capability",
                "Selected provider does not support this media route in Rust",
            ));
        }
        let mut payload = payload.clone();
        let edit = path == "/v1/images/edits";
        if !speech_route {
            payload = if codex {
                match provider::codex::payload(&config, selection, &original, form.as_ref(), edit) {
                    Ok(payload) => payload,
                    Err(_) => {
                        return Err(error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            "Invalid Codex image input",
                        ));
                    }
                }
            } else if xai {
                match images::xai_request(&original, &selection.model, form.as_ref(), edit) {
                    Ok(payload) => payload,
                    Err(_) => {
                        return Err(error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            "Unsupported xAI image model or missing edit image",
                        ));
                    }
                }
            } else {
                let mut payload = original.clone();
                payload["model"] = selection
                    .model
                    .split('(')
                    .next()
                    .unwrap_or(&selection.model)
                    .trim()
                    .into();
                payload
                    .as_object_mut()
                    .expect("validated object")
                    .remove("stream");
                payload
            };
        }
        provider::prepare(
            &config,
            selection,
            path,
            &parts.headers,
            &mut payload,
            &original,
            if xai || codex { None } else { form.as_ref() },
        )
        .map_err(|_| {
            error(
                StatusCode::NOT_IMPLEMENTED,
                "unsupported_capability",
                "Media credentials, transport, or payload configuration is unsupported",
            )
        })
    };
    let (selection, mut attempt, response) = match retry::execute(
        home.clone(),
        &config,
        &model,
        &original,
        &parts.headers,
        &parts.uri,
        speech_route || image_stream,
        None,
        prepare,
    )
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    let codex_bridge =
        selection.provider == "codex" && !provider::codex::direct(&original, &selection.model);
    let cancel = attempt.cancellation();
    let status = response.status();

    let headers = response.headers().clone();
    if speech_route && status.is_success() {
        return audio(response, attempt, audio_type);
    }
    let mut source = response.bytes_stream();
    let mut bytes = Vec::new();
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => { attempt.record.local_failure(503); return error(StatusCode::SERVICE_UNAVAILABLE,"not_ready","Home membership ended"); }
            next = source.next() => next,
        };
        match next {
            Some(Ok(chunk)) if bytes.len().saturating_add(chunk.len()) <= 64 * 1024 * 1024 => {
                if bytes.is_empty() {
                    attempt.first_byte();
                }
                bytes.extend_from_slice(&chunk);
            }
            Some(_) => {
                attempt.record.local_failure(502);
                return error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    "Media response failed or exceeded limit",
                );
            }
            None => break,
        }
    }
    let value = if codex_bridge && status.is_success() {
        provider::codex::response(&bytes, images::response_format(&original)).map_err(|_| ())
    } else {
        serde_json::from_slice::<Value>(&bytes).map_err(|_| ())
    };
    if !status.is_success() {
        if speech_route
            && status == StatusCode::NOT_FOUND
            && !value.as_ref().is_ok_and(speech::model_unavailable)
        {
            // Unknown voice IDs must not cool down an otherwise valid speech credential.
            attempt.record.fail_code = "request_scoped".into();
        }
        return rejection::response(status, &selection.provider, &headers);
    }
    let Ok(value) = value else {
        attempt.record.local_failure(502);
        return error(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "Invalid media response JSON",
        );
    };
    let mut usage = Usage::new(Format::OpenAi);
    usage.json(&value);
    attempt.record.breakdown = usage.breakdown();
    if usage.failed {
        attempt.record.local_failure(502);
        return error(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "Media upstream returned an error",
        );
    }
    // Compatible image responses follow the same Go response-format normalization as xAI.
    match images::response(value.clone(), images::response_format(&original)) {
        Ok(value) => {
            attempt.record.failed = false;
            attempt.record.fail_code.clear();
            attempt.record.image_usage(&value, &original);
            if image_stream {
                let prefix = if path.ends_with("/edits") {
                    "image_edit"
                } else {
                    "image_generation"
                };
                return images::completed_sse(&value, prefix, attempt);
            }
            (status, Json(value)).into_response()
        }
        Err(_) => {
            attempt.record.local_failure(502);
            error(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "Upstream did not return image output",
            )
        }
    }
}

fn audio(
    response: reqwest::Response,
    attempt: lifecycle::Attempt,
    fallback: &'static str,
) -> Response {
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .filter(|s| {
            !matches!(
                s.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
                    .as_str(),
                "" | "application/json" | "application/octet-stream" | "text/plain"
            )
        })
        .and_then(|s| HeaderValue::from_str(s).ok())
        .unwrap_or_else(|| HeaderValue::from_static(fallback));
    // Own the admission until EOF or downstream disconnect; no keepalive bytes in audio.
    let mut attempt = attempt;
    attempt.record.failed = true;
    attempt.record.fail_code = "request_scoped".into();
    let body = stream::unfold(
        Some((response.bytes_stream(), attempt)),
        |state| async move {
            let (mut source, mut attempt) = state?;
            let cancel = attempt.cancellation();
            let next = tokio::select! { biased;
                _ = cancel.cancelled() => Some(Err(std::io::Error::other("Home membership ended"))),
                next = source.next() => next.map(|r| r.map_err(|_| std::io::Error::other("Audio upstream read failed"))),
            };
            match next {
                Some(Ok(bytes)) => {
                    attempt.first_byte();
                    Some((Ok(bytes), Some((source, attempt))))
                }
                Some(Err(err)) => {
                    attempt.record.local_failure(502);
                    drop(attempt);
                    Some((Err(err), None))
                }
                None => {
                    attempt.record.failed = false;
                    attempt.record.fail_code.clear();
                    drop(attempt);
                    None
                }
            }
        },
    );
    let mut result = (status, Body::from_stream(body)).into_response();
    result.headers_mut().insert("content-type", content_type);
    result
}
