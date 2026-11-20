use super::*;
use futures_util::stream;
use std::convert::Infallible;

#[tokio::test(start_paused = true)]
async fn comments_wait_for_first_frame_and_do_not_split_partial_frames() {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(4);
    let source = stream::unfold(rx, |mut rx| async { rx.recv().await.map(|v| (v, rx)) });
    let response = body(
        Response::new(Body::from_stream(source)),
        Duration::from_secs(2),
        true,
        false,
    );
    let mut output = response.into_body().into_data_stream();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), output.next())
            .await
            .is_err()
    );
    tx.send(Ok(Bytes::from_static(b"data: {"))).await.unwrap();
    assert_eq!(output.next().await.unwrap().unwrap(), "data: {");
    assert!(
        tokio::time::timeout(Duration::from_secs(3), output.next())
            .await
            .is_err()
    );
    tx.send(Ok(Bytes::from_static(b"}\n\n"))).await.unwrap();
    assert_eq!(output.next().await.unwrap().unwrap(), "}\n\n");
    assert_eq!(output.next().await.unwrap().unwrap(), SSE_COMMENT);
    drop(tx);
    assert!(output.next().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn pre_first_and_nonstream_wait_keepalives() {
    let response = Response::new(Body::from_stream(stream::pending::<
        Result<Bytes, Infallible>,
    >()));
    let mut output = body(response, Duration::from_secs(1), true, true)
        .into_body()
        .into_data_stream();
    assert_eq!(output.next().await.unwrap().unwrap(), SSE_COMMENT);
    let response = nonstream_wait(
        async {
            tokio::time::sleep(Duration::from_secs(4)).await;
            Response::new(Body::from("{\"ok\":true}"))
        },
        Duration::from_secs(1),
    )
    .await;
    let bytes = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert!(bytes.starts_with(b" "));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["ok"],
        true
    );
}

fn parsed_streaming(extra: &str) -> crate::config::Streaming {
    crate::config::Config::parse(
        format!("credential-concurrency: {{lifecycle-config-revision: 1}}\n{extra}").as_bytes(),
    )
    .unwrap()
    .streaming
}

#[tokio::test]
async fn gateway_committed_error_does_not_join_a_partial_sse_event() {
    for (partial, separator) in [
        ("data: {", "\n\n"),
        ("data: {\n", "\n"),
        ("data: {}\n\n", ""),
    ] {
        let source = stream::iter([
            Ok(Bytes::copy_from_slice(partial.as_bytes())),
            Err(io::Error::other("private upstream error")),
        ]);
        let response = committed(Response::new(Body::from_stream(source)), "/v1/messages");
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.starts_with(&format!("{partial}{separator}event: error\ndata: ")));
        assert_eq!(text.matches("event: error").count(), 1);
        assert!(!text.contains("private"));
    }
}

#[tokio::test(start_paused = true)]
async fn gateway_pre_header_wait_commits_on_tick_and_frames_route_errors() {
    for (path, prefix) in [
        ("/v1/chat/completions", "data: {"),
        ("/v1/messages", "event: error\ndata: {"),
        ("/v1/responses", "event: error\ndata: {"),
        (
            "/v1beta/models/m:streamGenerateContent",
            "event: error\ndata: {",
        ),
    ] {
        let settings = parsed_streaming(
            "requests: {streaming: {keepalive-seconds: 1, send-keepalive-before-ft: true}}\n",
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        let response = stream_wait(async { rx.await.unwrap() }, &settings, path).await;
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let mut output = response.into_body().into_data_stream();
        assert_eq!(output.next().await.unwrap().unwrap(), SSE_COMMENT);
        let mut failure = Response::new(Body::from("PRIVATE_UPSTREAM_BODY"));
        *failure.status_mut() = http::StatusCode::BAD_GATEWAY;
        tx.send(failure).unwrap();
        let error = output.next().await.unwrap().unwrap();
        let text = std::str::from_utf8(&error).unwrap();
        assert!(text.starts_with(prefix), "{text}");
        assert!(!text.contains("PRIVATE"));
        assert!(output.next().await.is_none());
    }
}

#[tokio::test(start_paused = true)]
async fn gateway_pre_header_drop_cancels_pending_execution() {
    let settings =
        parsed_streaming("streaming: {keepalive-seconds: 1, send-keepalive-before-ft: true}\n");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let response = stream_wait(
        async move {
            let _tx = tx;
            std::future::pending::<Response>().await
        },
        &settings,
        "/v1/chat/completions",
    )
    .await;
    drop(response);
    assert!(rx.await.is_err());
}

#[tokio::test(start_paused = true)]
async fn gateway_disabled_pre_header_wait_and_immediate_errors_preserve_http() {
    for extra in [
        "streaming: {keepalive-seconds: 1, send-keepalive-before-ft: false}\n",
        "streaming: {keepalive-seconds: 0, send-keepalive-before-ft: true}\n",
        "streaming: {keepalive-seconds: 1, send-keepalive-before-ft: true}\n",
    ] {
        let settings = parsed_streaming(extra);
        let response = stream_wait(
            async {
                let mut response = Response::new(Body::from("{}"));
                *response.status_mut() = http::StatusCode::UNAUTHORIZED;
                response
            },
            &settings,
            "/v1/chat/completions",
        )
        .await;
        assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED);
    }
}

#[test]
fn header_filter_preserves_values_without_overwriting_managed_headers() {
    let mut src = HeaderMap::new();
    for (key, value) in [
        ("connection", "x-private, x-other"),
        ("connection", "x-third"),
        ("x-private", "blocked"),
        ("x-other", "blocked"),
        ("x-third", "blocked"),
        ("set-cookie", "blocked"),
        ("content-length", "10"),
        ("content-encoding", "gzip"),
        ("x-litellm-id", "blocked"),
        ("helicone-id", "blocked"),
        ("x-portkey-id", "blocked"),
        ("cf-aig-id", "blocked"),
        ("x-kong-id", "blocked"),
        ("x-bt-id", "blocked"),
        ("access-control-allow-origin", "blocked"),
        ("x-cpa-trace-id", "blocked"),
        ("content-type", "upstream"),
        ("x-request-id", "one"),
        ("x-request-id", "two"),
    ] {
        src.append(
            http::HeaderName::from_bytes(key.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    let mut dst = HeaderMap::new();
    dst.insert("content-type", "application/json".parse().unwrap());
    passthrough(&mut dst, &src);
    assert_eq!(dst.len(), 3);
    assert_eq!(dst["content-type"], "application/json");
    assert_eq!(dst.get_all("x-request-id").iter().count(), 2);
}

#[tokio::test]
async fn loopback_header_filter_and_body_preserve_json() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-Request-ID: loopback\r\nSet-Cookie: private\r\nConnection: close\r\n\r\n{}").await.unwrap();
    });
    let upstream = reqwest::get(format!("http://{addr}")).await.unwrap();
    let headers = upstream.headers().clone();
    let mut response = Response::new(Body::from(upstream.bytes().await.unwrap()));
    passthrough(response.headers_mut(), &headers);
    assert_eq!(response.headers()["x-request-id"], "loopback");
    assert!(!response.headers().contains_key("set-cookie"));
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 100)
            .await
            .unwrap(),
        "{}"
    );
    server.await.unwrap();
}
