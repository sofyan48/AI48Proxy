use super::*;
use futures_util::{FutureExt, stream};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

fn controls(extra: &str) -> RuntimeControls {
    RuntimeControls::from(
        &Config::parse(
            format!("credential-concurrency: {{lifecycle-config-revision: 1}}\n{extra}").as_bytes(),
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn body_limit_checks_declared_actual_chunked_and_exact_sizes() {
    let c = controls("max-request-body-bytes: 4\n");
    assert!(matches!(
        c.read_body(Body::empty(), Some(5)).await,
        Err(BodyReadError::TooLarge)
    ));
    assert_eq!(
        c.read_body(Body::from("1234"), Some(4)).await.unwrap(),
        "1234"
    );
    let chunks = stream::iter([
        Ok::<_, std::io::Error>(Bytes::from_static(b"123")),
        Ok(Bytes::from_static(b"45")),
    ]);
    assert!(matches!(
        c.read_body(Body::from_stream(chunks), None).await,
        Err(BodyReadError::TooLarge)
    ));
    assert!(matches!(
        c.read_body(Body::from("12345"), Some(1)).await,
        Err(BodyReadError::TooLarge)
    ));
    let failure = stream::iter([Err::<Bytes, _>(std::io::Error::other("read failure"))]);
    assert!(matches!(
        c.read_body(Body::from_stream(failure), None).await,
        Err(BodyReadError::Read(_))
    ));
    assert_eq!(
        controls("")
            .read_body(Body::from("12345"), None)
            .await
            .unwrap(),
        "12345"
    );
}

#[tokio::test(start_paused = true)]
async fn handshake_bound_excludes_connect_and_established_io() {
    let c = controls("tls-handshake-timeout-seconds: 1\n");
    let result = c
        .connect_then_handshake(
            async {
                tokio::time::sleep(Duration::from_secs(20)).await;
                Ok::<_, std::io::Error>(42)
            },
            |transport| async move { Ok(transport) },
        )
        .await
        .unwrap();
    assert_eq!(result, 42);
    // Returned connection is not carrying an inherited deadline.
    tokio::time::sleep(Duration::from_secs(20)).await;
    assert!(matches!(
        c.handshake(async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            Ok::<_, std::io::Error>(())
        })
        .await,
        Err(HandshakeError::Timeout)
    ));
    assert!(matches!(
        c.handshake(async { Err::<(), _>(std::io::Error::other("TLS failed")) })
            .await,
        Err(HandshakeError::Transport(_))
    ));
    controls("")
        .handshake(async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            Ok::<_, std::io::Error>(())
        })
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn streaming_waits_for_first_data_and_propagates_error_once() {
    let c = controls("streaming: {keepalive-seconds: 2}\n");
    let (tx, rx) = mpsc::channel::<Result<Bytes, &'static str>>(2);
    let mut output = c.streaming(ReceiverStream::new(rx), false);
    assert!(output.next().now_or_never().is_none());
    tokio::time::advance(Duration::from_secs(10)).await;
    assert!(output.next().now_or_never().is_none());
    tx.send(Ok(Bytes::from_static(b"data: first\n\n")))
        .await
        .unwrap();
    assert_eq!(output.next().await.unwrap().unwrap(), "data: first\n\n");
    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(output.next().await.unwrap().unwrap(), ": keep-alive\n\n");
    tx.send(Err("failed")).await.unwrap();
    assert_eq!(output.next().await, Some(Err("failed")));
    assert!(output.next().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn pre_first_token_idle_reset_eof_and_drop() {
    let c = controls("streaming: {keepalive-seconds: 2, send-keepalive-before-ft: true}\n");
    let (tx, rx) = mpsc::channel::<Result<Bytes, &'static str>>(2);
    let mut output = c.streaming(ReceiverStream::new(rx), false);
    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(output.next().await.unwrap().unwrap(), ": keep-alive\n\n");
    tokio::time::advance(Duration::from_secs(1)).await;
    tx.send(Ok(Bytes::from_static(b"data: next\n\n")))
        .await
        .unwrap();
    assert_eq!(output.next().await.unwrap().unwrap(), "data: next\n\n");
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(output.next().now_or_never().is_none());
    drop(tx);
    assert!(output.next().await.is_none());
    let (tx, rx) = mpsc::channel::<Result<Bytes, &'static str>>(1);
    drop(c.streaming(ReceiverStream::new(rx), true));
    assert!(tx.is_closed());
}

#[tokio::test(start_paused = true)]
async fn nonstream_whitespace_and_disabled_keepalives() {
    let c = controls("nonstream-keepalive-interval: 3\n");
    let (tx, rx) = mpsc::channel::<Result<Bytes, &'static str>>(1);
    let mut output = c.nonstream(ReceiverStream::new(rx));
    assert_eq!(output.next().await.unwrap().unwrap(), "\n");
    tx.send(Ok(Bytes::from_static(b"{\"ok\":true}")))
        .await
        .unwrap();
    drop(tx);
    assert_eq!(output.next().await.unwrap().unwrap(), "{\"ok\":true}");
    assert!(output.next().await.is_none());
    let (tx, rx) = mpsc::channel::<Result<Bytes, &'static str>>(1);
    let mut output = controls("").streaming(ReceiverStream::new(rx), true);
    tokio::time::advance(Duration::from_secs(100)).await;
    assert!(output.next().now_or_never().is_none());
    drop(tx);
    assert!(output.next().await.is_none());
}
