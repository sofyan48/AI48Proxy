//! Actual-router video jobs, Home KV and pinned credential retrieval.
use super::*;

#[tokio::test]
async fn media_rules_match_original_bounded_bytes_not_reserialized_json() {
    bounded(async {
        for (case, raw) in [
            b"{ \"message\" : \"raw-match\" }".to_vec(),
            b"{\"message\":\"\\u0072aw-match\"}".to_vec(),
            b"\xffraw-match".to_vec(),
            [b"raw-match".as_slice(), &vec![b'x'; 64 * 1024]].concat(),
        ].into_iter().enumerate() {
            let home = Home::new().await;
            let (client, _subscription) = tokio::join!(
                HomeClient::start(home.connector.clone(), Arc::new(ReportingQueue::default())),
                home.registration(),
            );
            let client = client.unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}/v1", listener.local_addr().unwrap());
            let request = Request::builder().method("POST").uri("/v1/images/generations")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {CALLER}"))
                .body(Body::from(r#"{"model":"alias","prompt":"draw"}"#)).unwrap();
            let selection = async {
                let mut peer = home.accept().await;
                assert_eq!(command(&mut peer).await[0], b"RPOP");
                bulk(&mut peer, &serde_json::to_vec(&json!({
                    "provider":"openai", "model":"selected", "auth_index":"rules", "request_retry":0,
                    "auth":{"id":"rules", "attributes":{"api_key":SECRET,"base_url":base},
                        "metadata":{"request_scoped_errors":[
                            {"status":400,"match":["{ \"message\" : \"raw-match\" }", "raw-match"],"action":"stop"}
                        ]}}
                })).unwrap()).await;
            };
            let upstream = async {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") { headers.push(socket.read_u8().await.unwrap()); }
                let length = String::from_utf8(headers).unwrap().lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                }).unwrap_or(0);
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await.unwrap();
                socket.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", raw.len()).as_bytes()).await.unwrap();
                socket.write_all(&raw).await.unwrap();
            };
            let (response, (), ()) = tokio::join!(crate::gateway::router(client.clone()).oneshot(request), selection, upstream);
            assert_eq!(response.unwrap().status(), StatusCode::BAD_REQUEST);
            let events = client.reporting.pending_batch(1, usize::MAX);
            let usage: Value = serde_json::from_slice(&events[0].payload).unwrap();
            let matches = raw.len() <= 64 * 1024 && raw.windows(9).any(|s| s == b"raw-match");
            assert_eq!(usage["fail"]["code"], if matches { "request_scoped" } else { "" }, "case {case}");
            assert!(!String::from_utf8_lossy(&events[0].payload).contains("raw-match"));
            client.fence();
            client.retire().await.unwrap();
        }
    }).await;
}

#[tokio::test]
async fn video_creation_retrieval_binding_and_expiry_fail_closed() {
    bounded(async {
        let home = Home::new().await;
        let (client, _subscription) = tokio::join!(
            HomeClient::start(home.connector.clone(), Arc::new(ReportingQueue::default())),
            home.registration(),
        );
        let client = client.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1",listener.local_addr().unwrap());
        let request = Request::builder().method("POST").uri("/v1/videos")
            .header("content-type","application/json").header("authorization",format!("Bearer {CALLER}"))
            .body(Body::from(r#"{"prompt":"waves"}"#)).unwrap();
        let exchange = async {
            home.select("xai",&base,"grok-imagine-video","grok-imagine-video").await;
            let mut peer = home.accept().await;
            let args = command(&mut peer).await;
            assert_eq!(args[0],b"SET"); assert_eq!(&args[3..], &[b"PX".to_vec(),b"10800000".to_vec()]);
            let binding: Value = serde_json::from_slice(&args[2]).unwrap();
            assert_eq!(binding,json!({"auth_id":"matrix-credential","model":"grok-imagine-video"}));
            assert!(!String::from_utf8_lossy(&args[2]).contains(SECRET));
            peer.get_mut().write_all(b"+OK\r\n").await.unwrap();
            (args[1].clone(),args[2].clone())
        };
        let (response,(key,binding),captured) = tokio::join!(crate::gateway::router(client.clone()).oneshot(request),exchange,upstream(&listener,"application/json",r#"{"request_id":"job-123"}"#));
        assert_eq!(response.unwrap().status(),StatusCode::OK);
        assert_eq!(captured.target,"/v1/videos/generations");
        let creation: Value = serde_json::from_slice(&client.reporting.pending_batch(1, usize::MAX)[0].payload).unwrap();
        assert!(creation.get("media_counts").is_none());
        assert!(creation.get("image_details").is_none());
        let retrieve_request = Request::builder().uri("/openai/v1/videos/job-123").header("authorization",format!("Bearer {CALLER}")).body(Body::empty()).unwrap();
        let retrieve_home = async {
            let mut peer = home.accept().await;
            assert_eq!(command(&mut peer).await,vec![b"MGET".to_vec(),key.clone()]);
            peer.get_mut().write_all(b"*1\r\n").await.unwrap(); bulk(&mut peer,&binding).await;
            home.select("xai",&base,"grok-imagine-video","grok-imagine-video").await;
            let mut peer = home.accept().await;
            let args = command(&mut peer).await;
            assert_eq!(args[0],b"SET"); assert_eq!(args[1],key); assert_eq!(args[2],binding);
            peer.get_mut().write_all(b"+OK\r\n").await.unwrap();
        };
        let retrieve_upstream = async {
            let (mut socket,_) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") { bytes.push(socket.read_u8().await.unwrap()); }
            let headers = String::from_utf8(bytes).unwrap();
            assert!(headers.starts_with("GET /v1/videos/job-123 HTTP/1.1\r\n"));
            assert!(headers.contains(&format!("authorization: Bearer {SECRET}")));
            let wire = r#"{"status":"done","video":{"url":"https://example.invalid/video.mp4","duration":6,"resolution":"720p"}}"#;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{wire}",wire.len()).as_bytes()).await.unwrap();
        };
        let (retrieved,(),()) = tokio::join!(crate::gateway::router(client.clone()).oneshot(retrieve_request),retrieve_home,retrieve_upstream);
        let retrieved = retrieved.unwrap(); assert_eq!(retrieved.status(),StatusCode::OK);
        let value: Value = serde_json::from_slice(&to_bytes(retrieved.into_body(),16384).await.unwrap()).unwrap();
        assert_eq!(value["status"],"completed"); assert_eq!(value["seconds"],"6");
        let events = client.reporting.pending_batch(10, usize::MAX);
        let completed: Value = serde_json::from_slice(&events[1].payload).unwrap();
        assert_eq!(completed["media_counts"], json!({"completion_media":1,"audio_seconds":6.0}));
        assert_eq!(completed["image_details"]["resolution"], "720p");
        let request = Request::builder().uri("/v1/videos/job-123").header("authorization",format!("Bearer {CALLER}"))
            .body(Body::empty()).unwrap();
        let exchange = async {
            let mut peer = home.accept().await;
            assert_eq!(command(&mut peer).await,vec![b"MGET".to_vec(),key.clone()]);
            peer.get_mut().write_all(b"*1\r\n").await.unwrap(); bulk(&mut peer,&binding).await;
            let mut peer = home.accept().await;
            let args = command(&mut peer).await;
            let dispatch: Value = serde_json::from_slice(&args[1]).unwrap();
            assert_eq!(dispatch["pinned_auth_id"],"matrix-credential");
            assert_eq!(dispatch["model"],"grok-imagine-video");
            // A misbehaving/older Home must never cause a different credential to execute.
            bulk(&mut peer,&serde_json::to_vec(&json!({"provider":"xai","model":"grok-imagine-video","auth_index":"other","auth":{"id":"other","attributes":{"api_key":SECRET,"base_url":base}}})).unwrap()).await;
        };
        let (response,()) = tokio::join!(crate::gateway::router(client.clone()).oneshot(request),exchange);
        assert_eq!(response.unwrap().status(),StatusCode::SERVICE_UNAVAILABLE);
        let request = Request::builder().uri("/v1/videos/job-123").body(Body::empty()).unwrap();
        let exchange = async {
            let mut peer = home.accept().await;
            assert_eq!(command(&mut peer).await,vec![b"MGET".to_vec(),key]);
            peer.get_mut().write_all(b"*1\r\n$-1\r\n").await.unwrap();
        };
        let (response,()) = tokio::join!(crate::gateway::router(client.clone()).oneshot(request),exchange);
        assert_eq!(response.unwrap().status(),StatusCode::NOT_FOUND);
        assert_eq!(client.registry.active_count(),0);
        client.fence(); client.retire().await.unwrap();
    }).await;
}

#[tokio::test]
async fn media_retries_only_after_release_ack() {
    bounded(async {
        let home = Home::new().await;
        let (client, _subscription) = tokio::join!(HomeClient::start(home.connector.clone(),Arc::new(ReportingQueue::default())),home.registration());
        let client = client.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1",listener.local_addr().unwrap());
        let request = Request::builder().method("POST").uri("/v1/images/generations").header("content-type","application/json").header("authorization",format!("Bearer {CALLER}")).body(Body::from(r#"{"model":"public-alias","prompt":"draw"}"#)).unwrap();
        let home_exchange = async {
            home.select("openai",&base,"public-alias","resolved-model").await;
            let mut peer = home.accept().await;
            let args = command(&mut peer).await;
            assert_eq!(&args[..2],&[b"LPUSH".to_vec(),b"concurrency-release".to_vec()]);
            peer.get_mut().write_all(b":1\r\n").await.unwrap();
            let mut peer = home.accept().await;
            let args = command(&mut peer).await;
            assert_eq!(args[0],b"RPOP");
            let dispatch: Value = serde_json::from_slice(&args[1]).unwrap();
            assert_eq!(dispatch["retry_round"],1); assert_eq!(dispatch["excluded_auth_ids"],json!(["matrix-credential"]));
            bulk(&mut peer,&serde_json::to_vec(&json!({"provider":"openai","model":"resolved-model","auth_index":"retry-index","request_retry":2,"auth":{"id":"retry-credential","attributes":{"api_key":SECRET,"base_url":base}}})).unwrap()).await;
        };
        let upstream_exchange = async {
            let (mut socket,_) = listener.accept().await.unwrap();
            let mut bytes = Vec::new(); while !bytes.ends_with(b"\r\n\r\n") { bytes.push(socket.read_u8().await.unwrap()); }
            socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
            drop(socket);
            upstream(&listener,"application/json",r#"{"data":[{"b64_json":"AA=="}]}"#).await;
        };
        let (response,(),()) = tokio::join!(crate::gateway::router(client.clone()).oneshot(request),home_exchange,upstream_exchange);
        assert_eq!(response.unwrap().status(),StatusCode::OK);
        assert_eq!(client.registry.active_count(),0); assert_eq!(client.reporting.len(),2);
        client.fence(); client.retire().await.unwrap();
    }).await;
}

#[tokio::test]
async fn media_401_refresh_rebuilds_with_home_credential_without_redispatch() {
    bounded(async {
        let home = Home::new().await;
        let (client, _subscription) = tokio::join!(HomeClient::start(home.connector.clone(),Arc::new(ReportingQueue::default())),home.registration());
        let client = client.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1",listener.local_addr().unwrap());
        let request = Request::builder().method("POST").uri("/v1/images/generations").header("content-type","application/json").header("authorization",format!("Bearer {CALLER}")).body(Body::from(r#"{"model":"public-alias","prompt":"draw"}"#)).unwrap();
        let home_exchange = async {
            let mut peer = home.accept().await;
            assert_eq!(command(&mut peer).await[0],b"RPOP");
            let auth = json!({"id":"refresh-credential","attributes":{"base_url":base},"metadata":{"access_token":"old-synthetic-token"}});
            bulk(&mut peer,&serde_json::to_vec(&json!({"provider":"xai","model":"grok-imagine-image","auth_index":"refresh-index","request_retry":2,"auth":auth})).unwrap()).await;
            let mut peer = home.accept().await;
            let args = command(&mut peer).await;
            assert_eq!(args[0],b"GET");
            let refresh: Value = serde_json::from_slice(&args[1]).unwrap();
            assert_eq!(refresh["type"],"refresh"); assert_eq!(refresh["auth_index"],"refresh-index");
            assert!(refresh.get("access_token_sha256").is_some());
            assert!(!String::from_utf8_lossy(&args[1]).contains("old-synthetic-token"));
            bulk(&mut peer,&serde_json::to_vec(&json!({"auth_index":"refresh-index","auth":{"id":"refresh-credential","attributes":{"base_url":base},"metadata":{"access_token":SECRET}}})).unwrap()).await;
        };
        let upstream_exchange = async {
            let (mut socket,_) = listener.accept().await.unwrap();
            let mut bytes = Vec::new(); while !bytes.ends_with(b"\r\n\r\n") { bytes.push(socket.read_u8().await.unwrap()); }
            assert!(String::from_utf8(bytes).unwrap().contains("authorization: Bearer old-synthetic-token"));
            socket.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
            drop(socket);
            upstream(&listener,"application/json",r#"{"data":[{"b64_json":"AA=="}]}"#).await
        };
        let (response,(),captured) = tokio::join!(crate::gateway::router(client.clone()).oneshot(request),home_exchange,upstream_exchange);
        assert_eq!(response.unwrap().status(),StatusCode::OK);
        assert_eq!(captured.headers["authorization"],format!("Bearer {SECRET}"));
        assert_eq!(client.reporting.len(),1); assert_eq!(client.registry.active_count(),0);
        client.fence(); client.retire().await.unwrap();
    }).await;
}

#[tokio::test]
async fn xai_image_stream_emits_go_completion_events() {
    let result = run("xai","/v1/images/generations",json!({"model":"public-alias","prompt":"draw","stream":true}),"application/json",r#"{"data":[{"b64_json":"AA=="},{"url":"https://example.invalid/image"}],"usage":{"total_tokens":3}}"#).await;
    assert_eq!(result.status, StatusCode::OK);
    assert_eq!(result.headers["content-type"], "text/event-stream");
    assert_eq!(
        result
            .body
            .matches("event: image_generation.completed")
            .count(),
        2
    );
    assert!(!result.body.contains("[DONE]"));
    assert_eq!(result.usage["image_count"], 2);
    assert_eq!(result.usage["stream"], true);
    assert_eq!(result.usage["failed"], false);
    assert!(result.upstream.body.get("stream").is_none());
}
