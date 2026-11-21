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
async fn hinted_rate_limits_preserve_429_and_quota_body() {
    for (error, headers, expected) in [
        (
            serde_json::json!({"code":"rate_limit_exceeded"}),
            serde_json::json!({"Retry-After":"17"}),
            "17",
        ),
        (
            serde_json::json!({"type":"rate_limit_error"}),
            serde_json::json!({"retry-after":0}),
            "0",
        ),
        (
            serde_json::json!({"code":"turn_rate_limit_exceeded"}),
            serde_json::json!({"Retry-After":"21"}),
            "21",
        ),
        (
            serde_json::json!({"type":"usage_limit_reached","code":"rate_limit_exceeded","resets_at":1,"resets_in_seconds":45}),
            serde_json::json!({"Retry-After":"9"}),
            "45",
        ),
    ] {
        let (tx, response) = upstream();
        send(
            &tx,
            &frame(
                serde_json::json!({"type":"response.failed","response":{"error":error},"headers":headers}),
            ),
        );
        let Bootstrap::Rejected(rejected) =
            bootstrap(response, None, &CancellationToken::new()).await
        else {
            panic!("hinted rate limit must reject before commit");
        };
        assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(rejected.headers()["retry-after"], expected);
        let body: Value = serde_json::from_str(&body(rejected).await).unwrap();
        assert_eq!(body["error"], error);
        if error["type"] == "usage_limit_reached" {
            let mut record = crate::reporting::UsageRecord {
                provider: "codex".into(),
                ..Default::default()
            };
            record.upstream_failure(429, &body);
            let hints: Value = serde_json::from_str(&record.fail_body).unwrap();
            assert_eq!(hints["error"]["resets_at"], 1);
            assert_eq!(hints["error"]["resets_in_seconds"], 45);
        }
    }
}

#[test]
fn retry_hint_precedence_and_status_selection_match_terminal_errors() {
    let now = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let quota = serde_json::json!({"error":{"type":"usage_limit_reached","resets_at":1_700_000_100,"resets_in_seconds":45}});
    assert_eq!(
        bootstrap_rejection(
            &quota,
            &serde_json::json!({"headers":{"Retry-After":9}}),
            now
        ),
        (StatusCode::TOO_MANY_REQUESTS, Some(100))
    );
    for (body, event, status, retry) in [
        (
            serde_json::json!({"error":{"type":"rate_limit_error"}}),
            serde_json::json!({"headers":{"Retry-After":"Tue, 14 Nov 2023 22:15:00 GMT"}}),
            StatusCode::TOO_MANY_REQUESTS,
            Some(100),
        ),
        (
            serde_json::json!({"error":{"code":"rate_limit_exceeded"}}),
            serde_json::json!({"headers":{"retry-after":"bad"},"error":{"headers":{"retry-after":30}}}),
            StatusCode::SERVICE_UNAVAILABLE,
            None,
        ),
        (
            serde_json::json!({"error":{"type":"service_unavailable_error"}}),
            serde_json::json!({"headers":{"retry-after":30}}),
            StatusCode::SERVICE_UNAVAILABLE,
            Some(30),
        ),
        (
            serde_json::json!({"error":{"type":"rate_limit_error","status":503}}),
            serde_json::json!({"body":{"headers":{"retry-after":30}}}),
            StatusCode::SERVICE_UNAVAILABLE,
            Some(30),
        ),
        (
            serde_json::json!({"error":{"code":"rate_limit_exceeded"}}),
            serde_json::json!({"error":{"headers":{"retry-after":"-1"}}}),
            StatusCode::SERVICE_UNAVAILABLE,
            None,
        ),
    ] {
        assert_eq!(bootstrap_rejection(&body, &event, now), (status, retry));
    }
}

#[tokio::test]
async fn handshake_retry_after_does_not_replace_terminal_retry_hints() {
    let event = serde_json::json!({"type":"error","error":{"type":"rate_limit_error"}});
    let response: reqwest::Response = http::Response::builder()
        .status(200)
        .header("retry-after", "99")
        .body(frame(event))
        .unwrap()
        .into();
    let Bootstrap::Rejected(rejected) = bootstrap(response, None, &CancellationToken::new()).await
    else {
        panic!("rate limit must reject")
    };
    assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(!rejected.headers().contains_key("retry-after"));
}

#[tokio::test]
async fn empty_incomplete_stays_request_scoped_and_empty_eof_stays_empty() {
    let (tx, response) = upstream();
    send(
        &tx,
        &format!(
            "{}{}",
            preamble(),
            frame(
                serde_json::json!({"type":"response.incomplete","response":{"output":[],"usage":{"output_tokens":0}}})
            )
        ),
    );
    let Bootstrap::Released(released) = bootstrap(response, None, &CancellationToken::new()).await
    else {
        panic!("empty incomplete must not fail over")
    };
    assert_eq!(released.status(), 200);
    assert!(released.bytes().await.is_err());

    let (tx, response) = upstream();
    drop(tx);
    let Bootstrap::Released(released) = bootstrap(response, None, &CancellationToken::new()).await
    else {
        panic!("clean empty end must release")
    };
    assert!(released.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn non_overload_failures_and_unknown_frames_keep_in_stream_delivery() {
    for event in [
        serde_json::json!({"type":"error","error":{"code":"invalid_prompt","message":"no"}}),
        // Go does not classify quota exhaustion alone as a bootstrap overload.
        serde_json::json!({"type":"error","error":{"type":"usage_limit_reached","resets_in_seconds":45}}),
        serde_json::json!({"type":"response.incomplete","response":{"output":[],"usage":{"output_tokens":0.5}}}),
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
