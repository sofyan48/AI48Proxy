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
