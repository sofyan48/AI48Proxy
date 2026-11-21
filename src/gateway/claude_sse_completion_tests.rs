//! Router-level Claude translation with controlled terminal delivery and upstream EOF.
use super::*;

async fn check(outcome: u8, consume_terminal: bool, expected: u16) {
    let (gate_tx, gate_rx) = tokio::sync::oneshot::channel();
    let (client, task, mut seen, reply) = fixture_for("claude", Some(gate_rx)).await;
    let mut logs = crate::observability::capture_requests(&client);
    let request = http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer PRIVATE_CALLER")
        .body(Body::from(
            r#"{"model":"alias","stream":true,"messages":[{"role":"user","content":"hello"}]}"#,
        ))
        .unwrap();
    let response = router(client.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut output = response.into_body().into_data_stream();
    assert_eq!(
        output.next().await.unwrap().unwrap(),
        crate::config::keepalive::SSE_COMMENT
    );
    let mut next = Box::pin(output.next());
    tokio::select! {
        _ = seen.recv() => {},
        _ = &mut next => panic!("unexpected token before upstream headers"),
    }
    reply.send(outcome).unwrap();
    let first = next.await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("chat.completion.chunk"));
    assert_eq!(client.registry.active_count(), 1);
    assert!(client.reporting.is_empty());

    if outcome == 6 {
        // A real disconnect before message_stop, while upstream is still open.
        drop(output);
        drop(gate_tx);
        seen.recv().await.unwrap();
    } else {
        gate_tx.send(()).unwrap();
        // The upstream observes transport cancellation only after the producer has
        // parsed and queued its terminal output (or rejected the invalid stream).
        // No downstream poll occurs here, making the queued-but-undelivered case
        // deterministic rather than scheduler-dependent.
        seen.recv().await.unwrap();
        assert_eq!(client.registry.active_count(), 1);
        assert!(client.reporting.is_empty());
        assert!(logs.try_recv().is_err());
        if consume_terminal {
            let mut bytes = first.to_vec();
            let mut body_error = false;
            while let Some(item) = output.next().await {
                match item {
                    Ok(chunk) => bytes.extend(chunk),
                    Err(_) => {
                        body_error = true;
                        break;
                    }
                }
                if bytes
                    .windows(b"data: [DONE]".len())
                    .any(|w| w == b"data: [DONE]")
                {
                    break; // Deliberately never poll EOF after DONE.
                }
            }
            let done = bytes
                .windows(b"data: [DONE]".len())
                .any(|w| w == b"data: [DONE]");
            assert_eq!(done, expected == 200, "{}", String::from_utf8_lossy(&bytes));
            if expected != 200 {
                assert!(body_error || String::from_utf8_lossy(&bytes).contains("\"error\""));
            }
        }
        drop(output);
    }
    let log = logs.recv().await.unwrap();
    assert!(
        log.request_log.contains("Provider: claude"),
        "{}",
        log.request_log
    );
    assert!(
        log.request_log.contains(&format!("Status: {expected}")),
        "{}",
        log.request_log
    );
    assert!(
        log.request_log
            .contains(&format!("Error: {}", expected >= 400))
    );
    assert_eq!(client.registry.active_count(), 0);
    assert_eq!(client.reporting.len(), 1);
    let batch = client.reporting.pending_batch(2, usize::MAX);
    assert_eq!(batch.len(), 1);
    let usage: serde_json::Value = serde_json::from_slice(&batch[0].payload).unwrap();
    assert_eq!(usage["fail"]["status_code"], expected);
    assert_eq!(usage["failed"], expected >= 400);
    if expected == 200 {
        assert_eq!(usage["tokens"]["input_tokens"], 5);
        assert_eq!(usage["tokens"]["output_tokens"], 2);
        assert_eq!(usage["tokens"]["total_tokens"], 7);
    }
    assert_eq!(client.registry.pending_releases().len(), 1);
    assert!(logs.try_recv().is_err());
    crate::observability::stop_capture_requests(&client);
    client.fence();
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn claude_sse_done_delivered_without_eof_is_success() {
    tokio::time::timeout(Duration::from_secs(10), check(1, true, 200))
        .await
        .unwrap();
}

#[tokio::test]
async fn claude_sse_queued_done_is_not_delivery() {
    tokio::time::timeout(Duration::from_secs(10), check(1, false, 499))
        .await
        .unwrap();
}

#[tokio::test]
async fn claude_sse_disconnect_before_terminal_is_499() {
    tokio::time::timeout(Duration::from_secs(10), check(6, false, 499))
        .await
        .unwrap();
}

#[tokio::test]
async fn claude_sse_invalid_or_truncated_streams_fail() {
    tokio::time::timeout(Duration::from_secs(15), async {
        for outcome in [0, 2, 3, 4, 5] {
            check(outcome, true, 502).await;
        }
    })
    .await
    .unwrap();
}
