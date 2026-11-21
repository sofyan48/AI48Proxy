use super::*;
use crate::{config::Config, execution::Registry, home::client::Selection};

#[test]
fn auth_rejects_missing_changed_or_error_principal() {
    assert_eq!(
        ws_home::principal(
            &json!({"authenticated":true,"principal":"user"}),
            Some("user")
        )
        .unwrap(),
        "user"
    );
    for value in [
        json!({"authenticated":false,"principal":"user"}),
        json!({"authenticated":true}),
        json!({"authenticated":true,"principal":"other"}),
        json!({"authenticated":true,"principal":"user","error":{}}),
    ] {
        assert!(ws_home::principal(&value, Some("user")).is_err());
    }
}
#[test]
fn client_payload_cannot_pin_credentials_or_replace_auth_headers() {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer downstream".parse().unwrap());
    let request = ws_home::dispatch_request(
        &headers,
        &json!({"model":"m","pinned_auth_id":"attacker","headers":{"authorization":"Bearer attacker"},"session_id":"s"}),
    );
    assert!(request.get("pinned_auth_id").is_none());
    assert_eq!(
        request["headers"]["authorization"],
        json!(["Bearer downstream"])
    );
    assert_eq!(request["session_id"], "s");
    assert_eq!(request["concurrency_protocol"], 1);
}
#[test]
fn cancellation_and_terminals_do_not_release_stream_lease() {
    let cancellation = CancellationToken::new();
    let registry = Registry::new(cancellation.clone());
    let mut guard =
        registry
            .admission()
            .begin("a".into(), "m".into(), true, "r".into(), "stream".into());
    let mut history = ws_state::History::default();
    let body = history
        .hydrate(
            json!({"type":"response.create","model":"m","input":[]}),
            10000,
        )
        .unwrap();
    history
        .commit(body, &json!({"response":{"id":"r","output":[]}}), 10000)
        .unwrap();
    assert_eq!(registry.active_count(), 1);
    assert!(registry.pending_releases().is_empty());
    cancellation.cancel();
    assert!(guard.cancellation().is_cancelled());
    assert_eq!(registry.active_count(), 1);
    assert_eq!(guard.finish().unwrap().release_seq, 1);
    assert!(guard.finish().is_none());
    drop(guard);
    assert_eq!(registry.pending_releases()[0].release_seq, 1);
}
#[test]
fn ws_failure_ownership_does_not_consume_running_or_ambiguous_creates() {
    let make = |event: &str, automatic| ws_codex::Owner {
        request: json!({"event_id":event}),
        flags: ws_codex::Flags::default(),
        automatic,
        turn: (),
    };
    let mut owners = ws_codex::Owners::default();
    owners.active.insert("r1".into(), make("c1", false));
    owners.pending.push_back(make("c2", false));
    assert_eq!(
        owners
            .failure_owner(&json!({"response_id":"r1"}), false)
            .unwrap()
            .as_deref(),
        Some("r1")
    );
    assert_eq!(
        owners
            .failure_owner(&json!({"response_id":"rejected"}), false)
            .unwrap(),
        None
    );
    assert!(owners.failure_owner(&json!({}), false).is_err());
    assert!(owners.failure_owner(&json!({}), true).is_err());
    assert_eq!(owners.pending.front().unwrap().event_id(), "c2");
    owners.pending.clear();
    assert_eq!(
        owners.failure_owner(&json!({}), false).unwrap().as_deref(),
        Some("r1")
    );
    owners.active.insert("r2".into(), make("c2", false));
    assert!(owners.failure_owner(&json!({}), false).is_err());
}

#[test]
fn ws_response_owners_finish_once_even_when_other_responses_are_abandoned() {
    struct Reporter(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for Reporter {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut owners = ws_codex::Owners::default();
    for event in ["c1", "c2", "c3"] {
        owners.pending.push_back(ws_codex::Owner {
            request: json!({"event_id":event}),
            flags: ws_codex::Flags::default(),
            automatic: false,
            turn: Reporter(count.clone()),
        });
    }
    owners
        .active
        .insert("r1".into(), owners.pending.pop_front().unwrap());
    owners
        .active
        .insert("r2".into(), owners.pending.pop_front().unwrap());
    drop(owners.active.remove("r1"));
    drop(owners.active.remove("r1"));
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    drop(owners);
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[test]
fn ws_pipeline_and_abandoned_events_keep_request_correlation() {
    let mut steering = ws_steering::Steering::new(true);
    assert!(
        steering.ready(false),
        "an explicit running response permits pipelining"
    );
    assert!(
        !steering.ready(true),
        "automatic successor blocks pipelining"
    );
    steering.submitted("r1", "s-unacked");
    steering.submitted("r1", "s-accepted");
    steering.observe(
        &json!({"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}),
    );
    steering
        .enqueue(json!({"type":"response.create","event_id":"c-queued"}))
        .unwrap();
    let (sent, queued) = steering.abandoned();
    assert_eq!(sent, ["s-accepted", "s-unacked"]);
    assert_eq!(queued, ["c-queued"]);
    assert_eq!(
        ws_failure::lost(&sent[0], false)["error"]["code"],
        "upstream_websocket_lost"
    );
    assert_eq!(
        ws_failure::lost(&queued[0], true)["error"]["event_id"],
        "c-queued"
    );
    let mut history = ws_state::History::default();
    history.native = true;
    let body = history
        .hydrate(
            json!({"type":"response.create","model":"m","input":[],"event_id":"c1"}),
            10000,
        )
        .unwrap();
    history.submitted(&body);
    history.created("r1");
    let next = history
        .hydrate(
            json!({"type":"response.create","input":[],"event_id":"c2"}),
            10000,
        )
        .unwrap();
    assert_eq!(body["event_id"], "c1");
    assert_eq!(next["model"], "m");
    assert_eq!(next["previous_response_id"], "r1");
    assert_eq!(next["event_id"], "c2");
}

#[test]
fn websocket_encoding_preserves_final_payload_rules() {
    let config = Config::parse(br#"{"credential-concurrency":{"lifecycle-config-revision":1},"payload":{"override":[{"models":[{"name":"*","protocol":"codex"}],"params":{"instructions":"final-rule","store":true,"custom_final":42}}],"filter":[{"models":[{"name":"*","protocol":"codex"}],"params":["include"]}]}}"#).unwrap();
    let selection: Selection = serde_json::from_value(json!({"model":"m","provider":"codex","auth_index":"idx","user_api_key":"user","auth":{"id":"a","attributes":{"api_key":"upstream","base_url":"http://127.0.0.1:9"}}})).unwrap();
    let prepared = ws_transport::prepare(
        &config,
        &selection,
        &HeaderMap::new(),
        json!({"model":"m","input":[],"instructions":"caller"}),
    )
    .unwrap();
    let value: Value = serde_json::from_str(&prepared.message).unwrap();
    assert_eq!(value["type"], "response.create");
    assert_eq!(value["instructions"], "final-rule");
    assert_eq!(value["store"], true);
    assert_eq!(value["custom_final"], 42);
    assert!(value.get("include").is_none());
    assert_eq!(prepared.handshake.uri(), "ws://127.0.0.1:9/responses");
    assert_eq!(
        prepared.handshake.headers()["authorization"],
        "Bearer upstream"
    );
}
