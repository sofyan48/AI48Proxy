use super::*;
use crate::home::client::supervisor::Supervisor;

#[tokio::test]
async fn running_subscription_samples_and_uploads_negotiated_latency() {
    let home = Home::new().await;
    let (client, mut sub) = home.start().await;
    let config = CONFIG.replace("10s", "5m");
    publish(&mut sub, "config", config.as_bytes()).await;
    observed(|| client.config().lifecycle.heartbeat_timeout == Duration::from_secs(300)).await;
    publish(&mut sub, "cluster", br#"{"ok":true,"nodes":[]}"#).await;
    let mut enable = bounded(home.accept()).await;
    assert_eq!(command(&mut enable).await[2], b"ENABLE");
    bulk(
        &mut enable,
        br#"{"ok":true,"capabilities":["cpa_latency_swap_v1"]}"#,
    )
    .await;
    observed(|| client.latency_supported.load(Ordering::Acquire)).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(34)).await;
    tokio::time::resume();
    let args = bounded(command(&mut sub)).await;
    assert_eq!(args[0], b"PING");
    let token = std::str::from_utf8(&args[1]).unwrap();
    assert!(token.starts_with("home-rtt/"));
    sub.get_mut()
        .write_all(format!("*2\r\n$4\r\npong\r\n${}\r\n{}\r\n", token.len(), token).as_bytes())
        .await
        .unwrap();
    // A following config confirms the receiver consumed the PONG before clock advancement.
    publish(
        &mut sub,
        "config",
        format!("{config}request-retry: 5\n").as_bytes(),
    )
    .await;
    observed(|| client.config().request_retry == 5).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(87)).await;
    tokio::time::resume();
    let mut report = bounded(home.accept()).await;
    let args = command(&mut report).await;
    assert_eq!(
        &args[..3],
        &[
            b"CLUSTER".to_vec(),
            b"BALANCE".to_vec(),
            b"LATENCY".to_vec()
        ]
    );
    let payload: crate::home::cluster::LatencyReport = serde_json::from_slice(&args[3]).unwrap();
    assert_eq!(payload.process_id, client.instance);
    assert!(!payload.samples.is_empty());
    assert_eq!(
        payload.samples[0].port,
        home.listener.local_addr().unwrap().port()
    );
    bulk(&mut report, br#"{"ok":false}"#).await;
    assert!(
        client.ready(),
        "telemetry rejection must not fence dispatch"
    );
    client.retire().await.unwrap();
}

#[tokio::test]
async fn supervisor_moves_live_execution_without_cancelling_origin() {
    supervisor_live_handoff(false).await;
}

#[tokio::test]
async fn supervisor_target_failure_rolls_back_and_reports_failed_with_live_origin() {
    supervisor_live_handoff(true).await;
}

async fn supervisor_live_handoff(rollback: bool) {
    let home = Home::new().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let target = Home {
        connector: home
            .connector
            .for_target("127.0.0.1".into(), port)
            .await
            .unwrap(),
        listener,
        acceptor: home.acceptor.clone(),
        _identity: tempfile::tempdir().unwrap(),
    };
    let supervisor = Supervisor::new(home.connector.clone(), Arc::new(ReportingQueue::default()));
    let startup = async {
        let mut discovery = home.accept().await;
        let _ = command(&mut discovery).await;
        bulk(&mut discovery, br#"{"ok":true,"nodes":[]}"#).await;
        home.registration(CONFIG).await
    };
    let (first, mut sub) = bounded(async { tokio::join!(supervisor.connect(), startup) }).await;
    let first = first.unwrap();
    let mut guard = first.registry.begin(
        "credential".into(),
        "model".into(),
        true,
        "request".into(),
        "stream".into(),
    );
    let run = tokio::spawn(supervisor.clone().run());
    publish(
        &mut sub,
        "rebalance",
        format!(r#"{{"id":"live-plan","term":1,"target":{{"ip":"127.0.0.1","port":{port}}}}}"#)
            .as_bytes(),
    )
    .await;
    for action in [b"ENABLE".as_slice(), b"CLAIM".as_slice()] {
        let mut peer = bounded(home.accept()).await;
        assert_eq!(command(&mut peer).await[2], action);
        bulk(&mut peer, br#"{"ok":true}"#).await;
    }
    let mut preflight = bounded(target.accept()).await;
    assert_eq!(command(&mut preflight).await, [b"PING".to_vec()]);
    preflight.get_mut().write_all(b"+PONG\r\n").await.unwrap();
    let registration = async {
        if rollback {
            let mut failed = target.accept().await;
            assert_eq!(command(&mut failed).await[0], b"GET");
            failed
                .get_mut()
                .write_all(b"-ERR unavailable\r\n")
                .await
                .unwrap();
        }
        let destination = if rollback { &home } else { &target };
        let mut bootstrap = destination.accept().await;
        let _ = command(&mut bootstrap).await;
        bulk(&mut bootstrap, CONFIG.as_bytes()).await;
        let mut sub = destination.accept().await;
        let args = command(&mut sub).await;
        assert_eq!(args[3], b"takeover");
        assert_eq!(args[4], first.instance.as_bytes());
        sub.get_mut()
            .write_all(b"*3\r\n$9\r\nsubscribe\r\n$6\r\nconfig\r\n:1\r\n")
            .await
            .unwrap();
        publish(&mut sub, "config", CONFIG.as_bytes()).await;
        let mut worker = destination.accept().await;
        let _ = command(&mut worker).await;
        worker.get_mut().write_all(b"+PONG\r\n").await.unwrap();
        for action in ["ENABLE", "RESULT"] {
            let mut peer = destination.accept().await;
            let args = command(&mut peer).await;
            assert_eq!(args[2], action.as_bytes());
            if action == "RESULT" {
                assert_eq!(
                    &args[3..],
                    &[
                        b"live-plan".to_vec(),
                        if rollback {
                            b"failed".to_vec()
                        } else {
                            b"success".to_vec()
                        }
                    ]
                );
            }
            bulk(
                &mut peer,
                br#"{"ok":true,"capabilities":["cpa_latency_swap_v1"]}"#,
            )
            .await;
        }
        sub
    };
    let _target_sub = bounded(registration).await;
    observed(|| {
        supervisor.current().is_some_and(|next| {
            !Arc::ptr_eq(&first, &next)
                && next.connector.target()
                    == if rollback {
                        home.connector.target()
                    } else {
                        target.connector.target()
                    }
        })
    })
    .await;
    assert!(!first.cancellation().is_cancelled());
    assert!(!guard.cancellation().is_cancelled());
    assert!(!first.ready());
    let release = guard.finish().unwrap();
    let exchange = async {
        let mut peer = home.accept().await;
        assert_eq!(command(&mut peer).await[1], b"concurrency-release");
        peer.get_mut().write_all(b":1\r\n").await.unwrap();
    };
    let (result, ()) =
        bounded(async { tokio::join!(first.flush_release(&release), exchange) }).await;
    result.unwrap();
    supervisor.cancellation().cancel();
    bounded(run).await.unwrap().unwrap();
}
