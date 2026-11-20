use super::*;

fn completed(id: &str) -> Value {
    json!({"type":"response.completed", "response":{"id":id,"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}]}})
}
fn seeded() -> (History, Value) {
    let mut history = History::default();
    let request = history.hydrate(json!({"type":"response.create","model":"m","instructions":"keep","input":[{"role":"user","content":"hello"}]}), 10000).unwrap();
    history
        .commit(request.clone(), &completed("r1"), 10000)
        .unwrap();
    (history, request)
}
#[test]
fn continuation_hydrates_before_provider_normalization() {
    let (history, _) = seeded();
    let body = history.hydrate(json!({"type":"response.create","previous_response_id":"r1","input":[{"role":"user","content":"next"}]}), 10000).unwrap();
    assert!(body.get("previous_response_id").is_none());
    assert!(body.get("type").is_none());
    assert_eq!(body["model"], "m");
    assert_eq!(body["instructions"], "keep");
    assert_eq!(body["input"].as_array().unwrap().len(), 3);
    crate::providers::codex::normalize_request(&mut body.clone(), "m").unwrap();
}
#[test]
fn unknown_and_failed_history_never_silently_drop_context() {
    let (mut history, _) = seeded();
    assert!(
        history
            .hydrate(
                json!({"type":"response.create","previous_response_id":"foreign","input":[]}),
                10000
            )
            .is_err()
    );
    history.invalidate();
    assert!(
        history
            .hydrate(
                json!({"type":"response.create","previous_response_id":"r1","input":[]}),
                10000
            )
            .is_err()
    );
    assert!(
        history
            .hydrate(
                json!({"type":"response.create","input":[{"role":"user","content":"next"}]}),
                10000
            )
            .is_err()
    );
}
#[test]
fn cumulative_and_full_replacement_are_not_double_appended() {
    let (history, request) = seeded();
    let mut cumulative = request["input"].as_array().unwrap().clone();
    cumulative.extend(history.output.clone());
    cumulative.push(json!({"role":"user","content":"next"}));
    let body = history
        .hydrate(
            json!({"type":"response.create","previous_response_id":"r1","input":cumulative}),
            10000,
        )
        .unwrap();
    assert_eq!(body["input"].as_array().unwrap().len(), 3);
    let body = history.hydrate(json!({"type":"response.create","input":[{"role":"assistant","content":"replacement"}]}),10000).unwrap();
    assert_eq!(body["input"].as_array().unwrap().len(), 1);
}
#[test]
fn malformed_or_oversized_requests_do_not_mutate_history() {
    let (history, _) = seeded();
    assert!(
        history
            .hydrate(json!({"type":"response.create","input":"text"}), 10000)
            .is_err()
    );
    assert!(
        history
            .hydrate(json!({"type":"response.append","input":[]}), 1)
            .is_err()
    );
    assert_eq!(history.response_id, "r1");
    assert!(history.valid);
}
#[test]
fn controls_require_exact_current_response_ownership() {
    let frame =
        json!({"type":"response.interrupt","response_id":"r1","mode":"steer","extension":true});
    assert!(control(&frame, Some("r1")));
    assert!(!control(&frame, Some("r2")));
    assert!(!control(&frame, None));
    assert!(!control(&json!({"type":"response.cancel"}), Some("r1")));
}

#[test]
fn overlapping_terminals_only_close_their_own_response() {
    assert_eq!(
        terminal_response_id(
            &json!({"type":"response.completed","response":{"id":"r2"}}),
            Some("r1")
        ),
        Some("r2")
    );
    assert!(!terminal_owns_active(
        &json!({"type":"response.completed","response":{"id":"r2"}}),
        Some("r1")
    ));
    assert!(terminal_owns_active(
        &json!({"type":"response.failed","response":{"id":"r1"}}),
        Some("r1")
    ));
}

#[test]
fn proxy_and_unknown_controls_remain_fail_closed() {
    let event = json!({"type":"response.interrupt","response_id":"foreign"});
    assert!(!control(&event, Some("active")));
    assert!(!terminal_owns_active(
        &json!({"type":"response.completed","response":{"id":"foreign"}}),
        Some("active")
    ));
}

#[test]
fn idless_failures_are_attributed_to_the_active_response() {
    let event = json!({"type":"error","error":{"message":"upstream"}});
    assert_eq!(terminal_response_id(&event, Some("r1")), Some("r1"));
    assert!(terminal_owns_active(&event, Some("r1")));
    assert_eq!(terminal_response_id(&event, None), None);
}
