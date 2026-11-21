//! Codex stream-bootstrap-buffering probe: hold, reject and release semantics.
use super::*;
use bytes::Bytes;
use std::time::Duration;
use tokio::sync::mpsc;

type Sender = mpsc::UnboundedSender<std::result::Result<Bytes, std::io::Error>>;

fn upstream() -> (Sender, reqwest::Response) {
    let (tx, rx) = mpsc::unbounded_channel();
    let body = reqwest::Body::wrap_stream(tokio_stream::wrappers::UnboundedReceiverStream::new(rx));
    let response = http::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(body)
        .unwrap();
    (tx, response.into())
}

fn frame(event: Value) -> String {
    format!(
        "event: {}\ndata: {event}\n\n",
        event["type"].as_str().unwrap()
    )
}

fn send(tx: &Sender, text: &str) {
    tx.send(Ok(Bytes::from(text.to_owned()))).unwrap();
}

fn preamble() -> String {
    [
        frame(serde_json::json!({"type":"response.created","response":{"id":"r"}})),
        frame(serde_json::json!({"type":"response.in_progress","response":{"id":"r"}})),
        "data: \n\n".into(),
        ": comment\n".into(),
        frame(serde_json::json!({"type":"response.output_item.added",
            "item":{"type":"message","content":[]}})),
        frame(serde_json::json!({"type":"response.content_part.added",
            "part":{"type":"output_text","text":""}})),
    ]
    .concat()
}

async fn body(response: reqwest::Response) -> String {
    String::from_utf8(response.bytes().await.unwrap().to_vec()).unwrap()
}

#[tokio::test]
async fn frames_are_held_until_first_generated_event_then_replayed_in_order() {
    let (tx, response) = upstream();
    let cancellation = CancellationToken::new();
    let probe = bootstrap(response, None, &cancellation);
    tokio::pin!(probe);
    let held = preamble();
    // Split mid-line to prove partial lines are reassembled before classification.
    let (a, b) = held.split_at(17);
    send(&tx, a);
    send(&tx, b);
    for _ in 0..8 {
        assert!(futures_util::poll!(&mut probe).is_pending());
        tokio::task::yield_now().await;
    }
    let delta = frame(serde_json::json!({"type":"response.output_text.delta","delta":"hi"}));
    send(&tx, &delta);
    let Bootstrap::Released(released) = probe.await else {
        panic!("generation must release the stream");
    };
    assert_eq!(released.status(), 200);
    assert_eq!(released.headers()["content-type"], "text/event-stream");
    let tail = "data: [tail]\n\n";
    send(&tx, tail);
    drop(tx);
    assert_eq!(body(released).await, format!("{held}{delta}{tail}"));
}

#[tokio::test]
async fn overload_rejections_become_http_503_before_commit() {
    for event in [
        serde_json::json!({"type":"error","error":{"type":"service_unavailable_error",
            "code":"server_is_overloaded","message":"busy"}}),
        serde_json::json!({"type":"response.failed","response":{"id":"r",
            "error":{"code":"rate_limit_exceeded","message":"slow down"}}}),
        serde_json::json!({"type":"error","code":"turn_rate_limit_exceeded","message":"x"}),
        serde_json::json!({"type":"error","error":{"type":"server_error",
            "message":"An error occurred. You can retry your request."}}),
        serde_json::json!({"type":"error","error":{"message":"Model is at capacity"}}),
    ] {
        let (tx, response) = upstream();
        send(&tx, &format!("{}{}", preamble(), frame(event.clone())));
        let Bootstrap::Rejected(rejected) =
            bootstrap(response, None, &CancellationToken::new()).await
        else {
            panic!("{event} must fail the attempt over");
        };
        assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
        let value: Value = serde_json::from_str(&body(rejected).await).unwrap();
        assert!(value["error"].is_object(), "{value}");
    }
}

#[tokio::test]
async fn non_overload_failures_and_unknown_frames_keep_in_stream_delivery() {
    for event in [
        serde_json::json!({"type":"error","error":{"code":"invalid_prompt","message":"no"}}),
        serde_json::json!({"type":"response.failed","response":{"error":{"code":"context_length_exceeded"}}}),
        serde_json::json!({"type":"response.output_item.added","item":{"type":"web_search_call"}}),
        serde_json::json!({"type":"response.output_item.added",
            "item":{"type":"function_call","arguments":"{\"a\":1}"}}),
        serde_json::json!({"type":"response.shell_call_output_content.delta"}),
    ] {
        let (tx, response) = upstream();
        let wire = frame(event.clone());
        send(&tx, &wire);
        drop(tx);
        let Bootstrap::Released(released) =
            bootstrap(response, None, &CancellationToken::new()).await
        else {
            panic!("{event} must release");
        };
        assert_eq!(body(released).await, wire);
    }
}

#[tokio::test]
async fn frame_and_byte_budgets_release_without_further_input() {
    let (tx, response) = upstream();
    let keepalives = "data: \n".repeat(BOOTSTRAP_MAX_LINES + 1);
    send(&tx, &keepalives);
    let Bootstrap::Released(released) = bootstrap(response, None, &CancellationToken::new()).await
    else {
        panic!("frame budget must release");
    };
    drop(tx);
    assert_eq!(body(released).await, keepalives);

    let (tx, response) = upstream();
    let oversized = format!("data: {}", "x".repeat(BOOTSTRAP_MAX_BYTES));
    send(&tx, &oversized);
    let Bootstrap::Released(released) = bootstrap(response, None, &CancellationToken::new()).await
    else {
        panic!("byte budget must release");
    };
    drop(tx);
    assert_eq!(body(released).await.len(), oversized.len());
}

#[tokio::test(start_paused = true)]
async fn timeout_releases_held_frames_and_clean_end_is_not_rejected() {
    let (tx, response) = upstream();
    send(&tx, &preamble());
    let Bootstrap::Released(released) = bootstrap(
        response,
        Some(Duration::from_secs(20)),
        &CancellationToken::new(),
    )
    .await
    else {
        panic!("time budget must release");
    };
    drop(tx);
    assert_eq!(body(released).await, preamble());

    // A clean end while holding releases; the stream consumer reports it incomplete.
    let (tx, response) = upstream();
    send(&tx, &preamble());
    drop(tx);
    assert!(matches!(
        bootstrap(response, None, &CancellationToken::new()).await,
        Bootstrap::Released(_)
    ));

    let (_tx, response) = upstream();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        bootstrap(response, None, &cancellation).await,
        Bootstrap::Cancelled
    ));
}
