//! CPA-compatible bearer API CORS. This does not enable cookie credentials.
use axum::{
    extract::Request,
    http::{HeaderName, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::collections::BTreeSet;

pub(crate) async fn cors(request: Request, next: Next) -> Response {
    let mut allowed = BTreeSet::new();
    for name in [
        "authorization",
        "content-type",
        "x-api-key",
        "x-goog-api-key",
        "anthropic-version",
        "anthropic-beta",
    ] {
        allowed.insert(name.to_owned());
    }
    if request.method() == Method::OPTIONS {
        for value in request.headers().get_all("access-control-request-headers") {
            if let Ok(value) = value.to_str() {
                for name in value.split(',').map(str::trim) {
                    if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
                        allowed.insert(name.as_str().to_owned());
                    }
                }
            }
        }
    }
    let preflight = request.method() == Method::OPTIONS;
    let mut response = if preflight {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    headers.insert("access-control-allow-origin", HeaderValue::from_static("*"));
    headers.insert(
        "access-control-allow-methods",
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
    );
    let names = allowed.into_iter().collect::<Vec<_>>().join(", ");
    if let Ok(value) = HeaderValue::from_str(&names) {
        headers.insert("access-control-allow-headers", value);
    }
    headers.insert("access-control-expose-headers", HeaderValue::from_static("X-CPA-Trace-Id, X-CPA-VERSION, X-CPA-COMMIT, X-CPA-BUILD-DATE, X-CPA-SUPPORT-PLUGIN, X-CPA-HOME-VERSION, X-CPA-HOME-BUILD-DATE, X-SERVER-VERSION, X-SERVER-BUILD-DATE, Location, Retry-After, ETag, X-Request-Id, OpenAI-Request-Id"));
    headers.append(
        "vary",
        HeaderValue::from_static("Access-Control-Request-Headers"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, routing::post};
    use tower::ServiceExt;
    fn app() -> Router {
        Router::new()
            .route(
                "/v1/chat/completions",
                post(|| async { StatusCode::UNAUTHORIZED }),
            )
            .layer(axum::middleware::from_fn(cors))
    }
    #[tokio::test]
    async fn browser_preflight_explicitly_allows_auth_and_custom_headers() {
        let response = app()
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/v1/chat/completions")
                    .header("origin", "http://localhost:8327")
                    .header("access-control-request-method", "POST")
                    .header(
                        "access-control-request-headers",
                        "authorization, content-type, x-session-id",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(response.headers()["access-control-allow-origin"], "*");
        let names = response.headers()["access-control-allow-headers"]
            .to_str()
            .unwrap();
        assert!(names.split(", ").any(|v| v == "authorization"));
        assert!(names.split(", ").any(|v| v == "x-session-id"));
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-credentials")
        );
    }
    #[tokio::test]
    async fn actual_error_and_unavailable_responses_are_cors_readable() {
        for method in ["POST", "GET"] {
            let response = app()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri("/v1/chat/completions")
                        .header("origin", "http://localhost:8327")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(response.status().is_client_error());
            assert_eq!(response.headers()["access-control-allow-origin"], "*");
            assert!(
                response.headers()["access-control-expose-headers"]
                    .to_str()
                    .unwrap()
                    .contains("X-Request-Id")
            );
        }
        let response = Router::new()
            .fallback(|| async { StatusCode::SERVICE_UNAVAILABLE })
            .layer(axum::middleware::from_fn(cors))
            .oneshot(Request::new(Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["access-control-allow-origin"], "*");
    }
}
