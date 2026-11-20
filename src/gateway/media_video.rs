//! Credential-bound xAI video jobs. Home KV stores identifiers, never credentials.
use super::{Format, Usage, error, provider, rejection, retry};
use crate::{
    config::{multimedia::video_binding_ttl, runtime_controls::RuntimeControls},
    home::{
        client::HomeClient,
        kv::{Expiry, Kv, SetOptions},
    },
};
use axum::{
    Json,
    extract::{FromRequestParts, Path, Request, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use provider::video;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
struct Binding {
    auth_id: String,
    model: String,
}
fn key(id: &str) -> String {
    format!(
        "ai48proxy:video-auth:v1:{}",
        hex::encode(Sha256::digest(id.as_bytes()))
    )
}
fn failure(status: StatusCode, message: &'static str) -> Response {
    error(status, "video_error", message)
}

pub(super) async fn handle(State(home): State<Arc<HomeClient>>, request: Request) -> Response {
    if !home.ready() || home.cancellation().is_cancelled() {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "Home is not ready");
    }
    let (mut parts, body) = request.into_parts();
    let decoded_id = if parts.method == Method::GET {
        match Path::<String>::from_request_parts(&mut parts, &()).await {
            Ok(Path(id)) if !id.trim().is_empty() => id.trim().to_owned(),
            _ => return failure(StatusCode::BAD_REQUEST, "Invalid video ID"),
        }
    } else {
        String::new()
    };
    let path = parts.uri.path();
    let openai = path.starts_with("/openai/");
    let retrieve = parts.method == Method::GET;
    let content = openai && path.ends_with("/content");
    if content
        && url::form_urlencoded::parse(parts.uri.query().unwrap_or("").as_bytes())
            .any(|(k, v)| k == "variant" && !v.is_empty() && v != "video")
    {
        return failure(
            StatusCode::BAD_REQUEST,
            "Only the video content variant is supported",
        );
    }
    let config = home.config();
    let kv = Kv::new(&home);
    let id = decoded_id.as_str();
    let binding = if retrieve {
        match kv.get_json::<Binding>(&key(id)).await {
            Ok(Some(binding)) if !binding.auth_id.is_empty() && !binding.model.is_empty() => {
                Some(binding)
            }
            Ok(_) => {
                return failure(
                    StatusCode::NOT_FOUND,
                    "Video credential binding is missing or expired",
                );
            }
            Err(_) => {
                return failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Video credential binding is unavailable",
                );
            }
        }
    } else {
        None
    };
    let (model, original, payload) = if let Some(binding) = &binding {
        let payload = json!({"request_id":id});
        (binding.model.clone(), payload.clone(), payload)
    } else {
        let content_type = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let media_type = content_type.split(';').next().unwrap_or("").trim();
        let bytes = match RuntimeControls::from(config.as_ref())
            .read_body(body, None)
            .await
        {
            Ok(bytes) => bytes,
            Err(crate::config::runtime_controls::BodyReadError::TooLarge) => {
                return failure(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Video request exceeds configured limit",
                );
            }
            Err(_) => return failure(StatusCode::BAD_REQUEST, "Unable to read video request"),
        };
        let (original, form) = if media_type.eq_ignore_ascii_case("multipart/form-data") {
            let form = match super::multipart::Form::parse(content_type, &bytes) {
                Ok(form) => form,
                Err(_) => return failure(StatusCode::BAD_REQUEST, "Invalid multipart video form"),
            };
            let fields = match form.fields() {
                Ok(fields) => fields,
                Err(_) => return failure(StatusCode::BAD_REQUEST, "Invalid video form fields"),
            };
            (form.payload(&fields), Some(form))
        } else if media_type.eq_ignore_ascii_case("application/json")
            || media_type.ends_with("+json")
        {
            let value = match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(_) => return failure(StatusCode::BAD_REQUEST, "Video body must be valid JSON"),
            };
            (value, None)
        } else {
            return failure(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Video creation requires JSON or multipart form",
            );
        };
        let mut original = original;
        // OpenAI video forms commonly upload the reference as `input_reference` or `image`.
        // Convert file parts to data URLs before provider normalization.
        if let Some(form) = &form {
            for name in ["input_reference", "image", "input_image"] {
                if let Some(part) = form
                    .parts
                    .iter()
                    .find(|part| part.name == name && part.filename.is_some())
                {
                    let data = format!(
                        "data:{};base64,{}",
                        part.content_type,
                        base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            &part.bytes
                        )
                    );
                    original[name] = data.into();
                    if name != "input_reference" {
                        original["input_reference"] = json!({"image_url": original[name].clone()});
                    }
                    break;
                }
            }
        }
        match video::create(&original, openai) {
            Ok((model, payload)) => (model, original, payload),
            Err(_) => {
                return failure(
                    StatusCode::BAD_REQUEST,
                    "Invalid video request or unsupported model",
                );
            }
        }
    };
    #[allow(clippy::result_large_err)]
    let prepare = |selection: &crate::home::client::Selection| {
        let mut payload = payload.clone();
        provider::prepare(
            &config,
            selection,
            path,
            &parts.headers,
            &mut payload,
            &original,
            None,
        )
        .map_err(|_| {
            failure(
                StatusCode::NOT_IMPLEMENTED,
                "Selected credential cannot execute xAI video requests",
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
        false,
        binding.as_ref().map(|b| b.auth_id.as_str()),
        prepare,
    )
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    let status = response.status();
    if !status.is_success() {
        return rejection::response(status, &selection.provider, response.headers());
    }
    let cancel = attempt.cancellation();
    let mut source = response.bytes_stream();
    let mut bytes = Vec::new();
    loop {
        let next = tokio::select! { biased;
            _ = cancel.cancelled() => { attempt.record.local_failure(503); return failure(StatusCode::SERVICE_UNAVAILABLE,"Home membership ended"); }
            next = source.next() => next,
        };
        match next {
            Some(Ok(chunk)) if bytes.len().saturating_add(chunk.len()) <= 64 * 1024 * 1024 => {
                attempt.first_byte();
                bytes.extend_from_slice(&chunk);
            }
            Some(_) => {
                attempt.record.local_failure(502);
                return failure(
                    StatusCode::BAD_GATEWAY,
                    "Video response failed or exceeded limit",
                );
            }
            None => break,
        }
    }
    let value: Value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) if value.is_object() => value,
        _ => {
            attempt.record.local_failure(502);
            return failure(StatusCode::BAD_GATEWAY, "Invalid video response JSON");
        }
    };
    let result_id = if retrieve {
        id
    } else {
        match video::id(&value) {
            Some(id) => id,
            None => {
                attempt.record.local_failure(502);
                return failure(StatusCode::BAD_GATEWAY, "Video response has no job ID");
            }
        }
    };
    let auth_id = selection
        .auth
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("");
    if auth_id.is_empty() {
        attempt.record.local_failure(502);
        return failure(
            StatusCode::BAD_GATEWAY,
            "Video credential has no stable identifier",
        );
    }
    let ttl = video_binding_ttl(&config);
    let options = SetOptions {
        expiry: Some(Expiry::Milliseconds(ttl.as_millis().max(1) as u64)),
        ..Default::default()
    };
    let binding_key = key(result_id);
    let binding_value = Binding {
        auth_id: auth_id.into(),
        model: model.clone(),
    };
    let stored = tokio::select! { biased;
        _ = cancel.cancelled() => false,
        result = kv.set_json(&binding_key, &binding_value, options) => result.unwrap_or(false),
    };
    if !stored {
        attempt.record.local_failure(503);
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to retain video credential binding",
        );
    }
    let mut usage = Usage::new(Format::OpenAi);
    usage.json(&value);
    attempt.record.breakdown = usage.breakdown();
    attempt
        .record
        .video_usage(video::duration_seconds(&payload, &value));
    if video::completed_output(&value) {
        let mut details = json!({});
        for key in ["resolution", "aspect_ratio"] {
            if let Some(value) = value
                .get(key)
                .or_else(|| value.get("video")?.get(key))
                .or_else(|| value.get("data")?.get(0)?.get(key))
                .filter(|v| v.is_string())
            {
                details[key] = value.clone();
            }
        }
        attempt.record.image_usage(&details, &payload);
    }
    if content {
        let Some(url) = value
            .pointer("/video/url")
            .and_then(Value::as_str)
            .and_then(|s| url::Url::parse(s).ok())
            .filter(|u| {
                matches!(u.scheme(), "http" | "https")
                    && u.host_str().is_some()
                    && u.username().is_empty()
                    && u.password().is_none()
            })
        else {
            attempt.record.local_failure(502);
            return failure(StatusCode::BAD_GATEWAY, "Video content is not available");
        };
        // Downloads never receive upstream authorization or client-supplied headers.
        let mut client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(30));
        let proxy = provider::text(&selection.auth, "/proxy_url");
        if let Some(proxy) = (!proxy.is_empty())
            .then_some(proxy)
            .or(config.proxy_url.as_deref())
            .filter(|p| *p != "direct")
        {
            let Ok(proxy) = reqwest::Proxy::all(proxy) else {
                attempt.record.local_failure(501);
                return failure(
                    StatusCode::NOT_IMPLEMENTED,
                    "Unsupported video download proxy",
                );
            };
            client = client.proxy(proxy);
        }
        let Ok(client) = client.build() else {
            attempt.record.local_failure(502);
            return failure(StatusCode::BAD_GATEWAY, "Video download transport failed");
        };
        let response = tokio::select! { biased;
            _ = cancel.cancelled() => { attempt.record.local_failure(503); return failure(StatusCode::SERVICE_UNAVAILABLE,"Home membership ended"); }
            response = client.get(url).send() => response,
        };
        let Ok(response) = response else {
            attempt.record.local_failure(502);
            return failure(StatusCode::BAD_GATEWAY, "Video download failed");
        };
        if !response.status().is_success() {
            attempt.record.local_failure(502);
            return failure(StatusCode::BAD_GATEWAY, "Video download was rejected");
        }
        let headers = response.headers().clone();
        let mut result = super::audio(response, attempt, "application/octet-stream");
        for key in [
            "content-length",
            "content-disposition",
            "cache-control",
            "etag",
            "last-modified",
        ] {
            if let Some(value) = headers.get(key) {
                result.headers_mut().insert(key, value.clone());
            }
        }
        return result;
    }
    attempt.record.failed = false;
    attempt.record.fail_code.clear();
    if openai {
        let canonical = video::model(&model, true).map(|(_, m)| m).unwrap_or(model);
        let mut out = video::facade(
            &value,
            result_id,
            &canonical,
            (!retrieve).then_some(&payload),
        );
        if !retrieve {
            out["size"] = original
                .get("size")
                .cloned()
                .unwrap_or_else(|| "720x1280".into());
        }
        (status, Json(out)).into_response()
    } else {
        (status, Json(value)).into_response()
    }
}
