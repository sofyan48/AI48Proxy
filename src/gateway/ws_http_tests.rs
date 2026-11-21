use super::*;
use serde_json::json;

fn config() -> Config {
    Config::parse(
        br#"{"credential-concurrency":{"lifecycle-config-revision":1},
        "client":{"codex":{"optimize-multi-agent-v2":true}},
        "codex":{"orphan-delegation-compatibility":true}}"#,
    )
    .unwrap()
}

fn headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("user-agent", "codex_cli_rs/1.0".parse().unwrap());
    headers.insert("x-openai-subagent", "collab_spawn".parse().unwrap());
    headers
}

fn selection() -> Selection {
    serde_json::from_value(json!({"model":"m","provider":"codex","auth_index":"a",
        "auth":{"id":"a","attributes":{"api_key":"test","base_url":"http://127.0.0.1:9"}}}))
    .unwrap()
}

fn request() -> Value {
    json!({"model":"m","input":[
        {"type":"agent_message","content":[{"type":"encrypted_content","encrypted_content":"hello"}]},
        {"type":"function_call_output","call_id":"orphan","namespace":"codex_app","name":"create_thread","output":"result"}],
        "tools":[{"type":"namespace","name":"collaboration","tools":[
            {"type":"function","name":"spawn_agent","parameters":{"properties":{"message":{"encrypted":true}}}}]}]})
}

fn prepared_body(config: &Config, mut body: Value) -> (Value, ws_codex::Flags) {
    multiagent::prepare_boundary(config, "/v1/responses", &headers(), &mut body);
    let (prepared, flags) = prepare(config, &selection(), &headers(), body).unwrap();
    let request = prepared.request.build().unwrap();
    (
        serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap(),
        flags,
    )
}

fn decoder(json: bool, flags: ws_codex::Flags) -> ResponseDecoder {
    let response: reqwest::Response = http::Response::builder()
        .header(
            "content-type",
            if json {
                "application/json; charset=utf-8"
            } else {
                "text/event-stream"
            },
        )
        .body("")
        .unwrap()
        .into();
    ResponseDecoder::new(&response, flags)
}

fn terminal() -> Value {
    json!({"type":"response.completed","response":{"id":"r1","object":"response","status":"completed",
        "output":[{"type":"function_call","call_id":"c1","namespace":"collaboration-optimize","name":"spawn_agent",
            "arguments":"{\"name\":\"collaboration-optimize\"}"}],
        "usage":{"input_tokens":5,"output_tokens":3,"total_tokens":8}}})
}

#[test]
fn rewrites_and_orphans_precede_final_payload_barrier() {
    let (sent, flags) = prepared_body(&config(), request());
    assert!(flags.optimized && !flags.conflict);
    assert_eq!(sent["tools"][0]["name"], "collaboration-optimize");
    assert!(
        sent.pointer("/tools/0/tools/0/parameters/properties/message/encrypted")
            .is_none()
    );
    assert_eq!(
        sent["input"][0]["content"][0],
        json!({"type":"input_text","text":"hello"})
    );
    assert_eq!(sent["input"][1]["type"], "message");
    assert_eq!(
        sent["input"][1]["content"][0]["text"],
        "Tool output from codex_app__create_thread:\nresult"
    );

    let mut config = config();
    config.payload = Config::parse(
        br#"{"credential-concurrency":{"lifecycle-config-revision":1},
        "payload":{"override":[{"models":[{"name":"*","protocol":"codex"}],
        "params":{"tools":[],"input":[{"role":"user","content":"final"}],"store":true}}],
        "filter":[{"models":[{"name":"*","protocol":"codex"}],"params":["instructions"]}]}}"#,
    )
    .unwrap()
    .payload;
    let (sent, flags) = prepared_body(&config, request());
    assert!(flags.optimized);
    assert_eq!(sent["tools"], json!([]));
    assert_eq!(sent["input"], json!([{"role":"user","content":"final"}]));
    assert_eq!(sent["store"], true);
    assert!(sent.get("instructions").is_none());
}

#[test]
fn json_and_fragmented_sse_restore_identically_before_history_and_usage() {
    let (_, flags) = prepared_body(&config(), request());
    let terminal = terminal();
    let wire = format!("event: response.completed\r\ndata: {terminal}\r\n\r\ndata: [DONE]\n\n");
    let mut sse = decoder(false, flags);
    let mut events = Vec::new();
    for byte in wire.bytes() {
        events.extend(sse.feed(&[byte], 10000).unwrap());
    }
    events.extend(sse.finish(10000).unwrap());
    let mut json = decoder(true, flags);
    for byte in terminal["response"].to_string().bytes() {
        assert!(json.feed(&[byte], 10000).unwrap().is_empty());
    }
    let restored = json.finish(10000).unwrap();
    assert_eq!(events, restored);
    assert_eq!(events.len(), 1);
    let mut usage =
        super::super::super::usage::Usage::new(super::super::super::upstream::Format::OpenAi);
    for event in &events {
        usage.json(event);
    }
    assert_eq!(usage.breakdown().total_tokens, 8);
    assert_eq!(
        events[0]["response"]["output"][0]["namespace"],
        "collaboration"
    );
    assert_eq!(
        events[0]["response"]["output"][0]["arguments"],
        terminal["response"]["output"][0]["arguments"]
    );
    assert_eq!(
        events[0]["response"]["usage"],
        terminal["response"]["usage"]
    );

    let mut history = ws_state::History::default();
    history
        .commit(json!({"model":"m","input":[]}), &events[0], 10000)
        .unwrap();
    let next = history
        .hydrate(
            json!({"type":"response.create","previous_response_id":"r1",
        "input":[{"type":"function_call_output","call_id":"c1","output":"ok"}]}),
            10000,
        )
        .unwrap();
    assert_eq!(next["input"][0]["namespace"], "collaboration");
    // A replayed client-facing call stays intact while tools optimize again.
    let mut next = next;
    next["tools"] = request()["tools"].clone();
    let (sent, flags) = prepared_body(&config(), next);
    assert!(flags.optimized);
    assert_eq!(sent["tools"][0]["name"], "collaboration-optimize");
    assert_eq!(sent["input"][0]["namespace"], "collaboration");
}

#[test]
fn restoration_is_owned_by_attempt_not_prior_turn_or_latest_request() {
    let (_, optimized) = prepared_body(&config(), request());
    let mut conflict = request();
    conflict["tools"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"function","name":"collaboration-optimize__custom"}));
    let (_, conflict) = prepared_body(&config(), conflict);
    let (_, plain) = prepared_body(&config(), json!({"model":"m","input":[]}));
    assert!(conflict.conflict && !conflict.optimized);
    assert!(!plain.optimized && !plain.conflict);
    // Keep all three decoders live: later preparation cannot change the first owner.
    let mut first = decoder(false, optimized);
    let mut second = decoder(false, conflict);
    let mut third = decoder(true, plain);
    let terminal = terminal();
    let wire = format!("data: {terminal}\n\n");
    assert_eq!(
        first.feed(wire.as_bytes(), 10000).unwrap()[0]["response"]["output"][0]["namespace"],
        "collaboration"
    );
    assert_eq!(
        second.feed(wire.as_bytes(), 10000).unwrap(),
        vec![terminal.clone()]
    );
    third.feed(terminal.to_string().as_bytes(), 10000).unwrap();
    assert_eq!(third.finish(10000).unwrap(), vec![terminal]);
}

#[test]
fn decoder_eof_errors_limits_and_failed_json() {
    let flags = ws_codex::Flags {
        optimized: true,
        conflict: false,
    };
    let terminal = terminal();
    let mut sse = decoder(false, flags);
    assert!(
        sse.feed(format!("data: {terminal}").as_bytes(), 10000)
            .unwrap()
            .is_empty()
    );
    assert_eq!(sse.finish(10000).unwrap().len(), 1);
    assert!(sse.finish(10000).unwrap().is_empty());
    let mut sse = decoder(false, flags);
    assert!(sse.feed(&[b'x'; 100], 10).is_err());
    let mut json = decoder(true, flags);
    assert!(json.feed(&[b'x'; 100], 10).is_err());
    let mut json = decoder(true, flags);
    json.feed(b"{", 10).unwrap();
    assert!(json.finish(10).is_err());
    let mut json = decoder(true, flags);
    json.feed(
        br#"{"object":"response","id":"r","status":"failed","error":{"message":"no"}}"#,
        1000,
    )
    .unwrap();
    assert_eq!(json.finish(1000).unwrap()[0]["type"], "response.failed");
    let mut json = decoder(true, flags);
    json.feed(
        br#"{"object":"response","id":"r","status":"in_progress"}"#,
        1000,
    )
    .unwrap();
    assert!(json.finish(1000).is_err());
}

#[test]
fn item_events_restore_but_disabled_optimization_preserves_names() {
    let (_, flags) = prepared_body(&config(), request());
    let event = json!({"type":"response.output_item.added","item":{
        "type":"custom_tool_call","name":"collaboration-optimize.spawn_agent","input":"opaque"}});
    let wire = format!("data: {event}\n\n");
    let mut sse = decoder(false, flags);
    let restored = sse.feed(wire.as_bytes(), 10000).unwrap();
    assert_eq!(restored[0]["item"]["namespace"], "collaboration");
    assert_eq!(restored[0]["item"]["name"], "spawn_agent");
    assert_eq!(restored[0]["item"]["input"], "opaque");

    let mut config = config();
    config.providers.codex_optimize_multi_agent_v2 = false;
    let (sent, flags) = prepared_body(&config, request());
    assert!(!flags.optimized);
    assert_eq!(sent["tools"][0]["name"], "collaboration");
    let mut sse = decoder(false, flags);
    assert_eq!(sse.feed(wire.as_bytes(), 10000).unwrap(), vec![event]);
}

#[test]
fn fragmented_sse_and_limits() {
    let mut decoder = Decoder::default();
    let mut events = Vec::new();
    for b in
        b"event: response.created\r\ndata: {\"type\":\"response.created\"}\r\n\r\ndata: [DONE]\n\n"
    {
        events.extend(decoder.feed(&[*b], 1024).unwrap());
    }
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "response.created");
    assert!(decoder.feed(&[b'x'; 100], 10).is_err());
}
