//! mTLS membership protocol regressions, using the client mock Home identity.
use super::*;
use crate::home::{client::supervisor::Supervisor, cluster::Target, membership::Membership};

async fn register_mode(home: &Home, state: Arc<Membership>, expected: &[&str]) -> Arc<HomeClient> {
    let expected = expected
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect::<Vec<_>>();
    let script = async {
        let mut bootstrap = home.accept().await;
        assert_eq!(
            command(&mut bootstrap).await,
            [b"GET".to_vec(), b"config".to_vec()]
        );
        bulk(&mut bootstrap, CONFIG.as_bytes()).await;
        let mut sub = home.accept().await;
        assert_eq!(command(&mut sub).await, expected);
        sub.get_mut()
            .write_all(b"*3\r\n$9\r\nsubscribe\r\n$6\r\nconfig\r\n:1\r\n")
            .await
            .unwrap();
        publish(&mut sub, "config", CONFIG.as_bytes()).await;
        let mut worker = home.accept().await;
        assert_eq!(command(&mut worker).await, [b"PING".to_vec()]);
        worker.get_mut().write_all(b"+PONG\r\n").await.unwrap();
        sub
    };
    let (client, _sub) = bounded(async {
        tokio::join!(
            HomeClient::start_membership(
                home.connector.clone(),
                Arc::new(ReportingQueue::default()),
                "stable",
                state
            ),
            script
        )
    })
    .await;
    let client = client.unwrap();
    client.retire().await.unwrap();
    client
}

#[tokio::test]
async fn mtls_takeover_and_legacy_use_exact_subscription_shapes() {
    let home = Home::new().await;
    let state = Arc::new(Membership::default());
    state.mark_eligible();
    register_mode(
        &home,
        state.clone(),
        &["SUBSCRIBE", "config", "7", "takeover", "stable"],
    )
    .await;
    state.rejection("ERR wrong number of arguments for 'subscribe' command");
    register_mode(&home, state, &["SUBSCRIBE", "config", "7"]).await;
}

#[tokio::test]
async fn acknowledged_membership_survives_failed_command_probe() {
    let home = Home::new().await;
    let state = Arc::new(Membership::default());
    let script = async {
        let mut bootstrap = home.accept().await;
        let _ = command(&mut bootstrap).await;
        bulk(&mut bootstrap, CONFIG.as_bytes()).await;
        let mut sub = home.accept().await;
        let _ = command(&mut sub).await;
        sub.get_mut()
            .write_all(b"*3\r\n$9\r\nsubscribe\r\n$6\r\nconfig\r\n:1\r\n")
            .await
            .unwrap();
        publish(&mut sub, "config", CONFIG.as_bytes()).await;
        let mut worker = home.accept().await;
        let _ = command(&mut worker).await;
        worker
            .get_mut()
            .write_all(b"-ERR unavailable\r\n")
            .await
            .unwrap();
    };
    let (result, ()) = bounded(async {
        tokio::join!(
            HomeClient::start_membership(
                home.connector.clone(),
                Arc::new(ReportingQueue::default()),
                "stable",
                state.clone()
            ),
            script
        )
    })
    .await;
    assert!(result.is_err());
    assert!(state.takeover());
}

#[tokio::test]
async fn cluster_updates_normalize_and_order_failover_targets_without_switching() {
    let home = Home::new().await;
    let (client, mut sub) = home.start().await;
    publish(&mut sub, "cluster", br#"{"ok":true,"nodes":[{"ip":"127.0.0.2","port":8327,"client_count":8},{"ip":"127.0.0.3","port":8327,"client_count":-1},{"ip":"127.0.0.3","port":8327,"client_count":4},{"ip":"","port":8327}]}"#).await;
    // Cluster handling may issue ENABLE; answer it while the heartbeat reader stays independent.
    let mut control = bounded(home.accept()).await;
    assert_eq!(
        command(&mut control).await,
        ["CLUSTER", "BALANCE", "ENABLE"].map(|s| s.as_bytes().to_vec())
    );
    bulk(&mut control, br#"{"ok":true}"#).await;
    observed(|| client.discovered_targets().len() == 2).await;
    assert_eq!(
        client.discovered_targets()[0],
        Target {
            ip: "127.0.0.3".into(),
            port: 8327
        }
    );
    assert_eq!(client.connector.target(), home.connector.target());
    client.retire().await.unwrap();
}

#[tokio::test]
async fn rebalance_claim_preflight_and_signal_leave_source_membership_owned() {
    let home = Home::new().await;
    let (client, mut sub) = home.start().await;
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = target.local_addr().unwrap().port();
    publish(
        &mut sub,
        "rebalance",
        format!(r#"{{"id":"plan","term":4,"target":{{"ip":"127.0.0.1","port":{port}}}}}"#)
            .as_bytes(),
    )
    .await;
    let mut enable = bounded(home.accept()).await;
    assert_eq!(command(&mut enable).await[2], b"ENABLE");
    bulk(&mut enable, br#"{"ok":true}"#).await;
    let mut claim = bounded(home.accept()).await;
    assert_eq!(
        command(&mut claim).await,
        [
            "CLUSTER",
            "BALANCE",
            "CLAIM",
            "plan",
            "4",
            "127.0.0.1",
            &port.to_string()
        ]
        .map(|s| s.as_bytes().to_vec())
    );
    bulk(&mut claim, br#"{"ok":true}"#).await;
    let (socket, _) = bounded(target.accept()).await.unwrap();
    let mut probe = BufReader::new(home.acceptor.accept(socket).await.unwrap());
    assert_eq!(command(&mut probe).await, [b"PING".to_vec()]);
    probe.get_mut().write_all(b"+PONG\r\n").await.unwrap();
    observed(|| client.cancellation().is_cancelled()).await;
    let plan = client.membership.take_migration().unwrap();
    assert_eq!(plan.id, "plan");
    assert_eq!(plan.target.port, port);
    assert_eq!(client.connector.target(), home.connector.target());
    client.retire().await.unwrap();
}

#[tokio::test]
async fn subscription_timeout_fails_over_to_discovered_mtls_node() {
    let home = Home::new().await;
    let target_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = Home {
        connector: home
            .connector
            .for_target(
                "127.0.0.1".into(),
                target_listener.local_addr().unwrap().port(),
            )
            .await
            .unwrap(),
        listener: target_listener,
        acceptor: home.acceptor.clone(),
        _identity: tempfile::tempdir().unwrap(),
    };
    let supervisor = Supervisor::new(home.connector.clone(), Arc::new(ReportingQueue::default()));
    let startup = async {
        let mut discovery = home.accept().await;
        assert_eq!(
            command(&mut discovery).await,
            [b"CLUSTER".to_vec(), b"NODES".to_vec()]
        );
        discovery
            .get_mut()
            .write_all(b"-ERR cluster disabled\r\n")
            .await
            .unwrap();
        home.registration(CONFIG).await
    };
    let (first, mut sub) = bounded(async { tokio::join!(supervisor.connect(), startup) }).await;
    let first = first.unwrap();
    let nodes = format!(
        r#"{{"ok":true,"nodes":[{{"ip":"127.0.0.1","port":{},"client_count":0}}]}}"#,
        target.listener.local_addr().unwrap().port()
    );
    publish(&mut sub, "cluster", nodes.as_bytes()).await;
    let mut enable = bounded(home.accept()).await;
    let _ = command(&mut enable).await;
    bulk(&mut enable, br#"{"ok":true}"#).await;
    observed(|| first.discovered_targets().len() == 1).await;
    let run = tokio::spawn(supervisor.clone().run());
    // Preserve real socket I/O but advance only the heartbeat timer deterministically.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    observed(|| first.cancellation().is_cancelled()).await;
    tokio::time::resume();
    let registration = async {
        let mut bootstrap = target.accept().await;
        assert_eq!(
            command(&mut bootstrap).await,
            [b"GET".to_vec(), b"config".to_vec()]
        );
        bulk(&mut bootstrap, CONFIG.as_bytes()).await;
        let mut next = target.accept().await;
        let args = command(&mut next).await;
        assert_eq!(args[3], b"takeover");
        next.get_mut()
            .write_all(b"*3\r\n$9\r\nsubscribe\r\n$6\r\nconfig\r\n:1\r\n")
            .await
            .unwrap();
        publish(&mut next, "config", CONFIG.as_bytes()).await;
        let mut worker = target.accept().await;
        assert_eq!(command(&mut worker).await, [b"PING".to_vec()]);
        worker.get_mut().write_all(b"+PONG\r\n").await.unwrap();
        next
    };
    let _next = bounded(registration).await;
    observed(|| {
        supervisor
            .current()
            .is_some_and(|current| current.connector.target() == target.connector.target())
    })
    .await;
    supervisor.cancellation().cancel();
    bounded(run).await.unwrap().unwrap();
}

#[tokio::test]
async fn rebalance_never_claims_with_live_execution_owners() {
    let home = Home::new().await;
    let (client, mut sub) = home.start().await;
    let guard = client.registry.begin(
        "credential".into(),
        "model".into(),
        true,
        "request".into(),
        "request".into(),
    );
    publish(
        &mut sub,
        "rebalance",
        br#"{"id":"busy","term":1,"target":{"ip":"127.0.0.2","port":8327}}"#,
    )
    .await;
    let mut enable = bounded(home.accept()).await;
    assert_eq!(command(&mut enable).await[2], b"ENABLE");
    bulk(&mut enable, br#"{"ok":true}"#).await;
    observed(|| !client.control_busy.load(Ordering::Acquire)).await;
    assert!(client.ready());
    assert!(client.membership.take_migration().is_none());
    let accept = home.listener.accept();
    tokio::pin!(accept);
    assert!(futures_util::poll!(accept).is_pending());
    drop(guard);
    client.retire().await.unwrap();
}

#[tokio::test]
async fn blocked_rebalance_rpc_cannot_extend_subscription_heartbeat() {
    let home = Home::new().await;
    let (client, mut sub) = home.start().await;
    publish(
        &mut sub,
        "rebalance",
        br#"{"id":"blocked","term":1,"target":{"ip":"127.0.0.2","port":8327}}"#,
    )
    .await;
    let mut enable = bounded(home.accept()).await;
    let _ = command(&mut enable).await;
    bulk(&mut enable, br#"{"ok":true}"#).await;
    let mut claim = bounded(home.accept()).await;
    assert_eq!(command(&mut claim).await[2], b"CLAIM");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    observed(|| client.cancellation().is_cancelled()).await;
    tokio::time::resume();
    assert!(client.membership.take_timeout());
    assert!(client.membership.take_migration().is_none());
    client.retire().await.unwrap();
}
