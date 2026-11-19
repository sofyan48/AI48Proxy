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
