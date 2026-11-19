//! One client owns one acknowledged membership. Unsafe lifetimes are never reused.
use super::{
    resp::Frame,
    transport::{Connection, Connector},
};
use crate::{
    config::Config,
    execution::{Registry, ReleaseFrame},
    reporting::ReportingQueue,
};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};
use tokio_util::sync::CancellationToken;

#[path = "supervisor.rs"]
pub mod supervisor;

#[derive(Debug)]
pub struct TerminalConfig;
impl std::fmt::Display for TerminalConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid or unsupported Home configuration")
    }
}
impl std::error::Error for TerminalConfig {}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;

#[derive(Deserialize)]
pub struct Concurrency {
    pub accounted: bool,
    pub credential_id: String,
    pub model: String,
}
#[derive(Deserialize)]
pub struct Selection {
    pub model: String,
    pub provider: String,
    pub auth_index: String,
    #[serde(default)]
    pub user_api_key: String,
    pub auth: Value,
    #[serde(default)]
    pub model_info: Option<Value>,
    #[serde(default)]
    pub concurrency: Option<Concurrency>,
    #[serde(default)]
    pub request_retry: u32,
}
#[derive(Debug)]
pub struct Rejection(pub Value);
impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Home rejected dispatch")
    }
}
impl std::error::Error for Rejection {}

pub struct HomeClient {
    connector: Connector,
    config: RwLock<Arc<Config>>,
    cancel: CancellationToken,
    ready: AtomicBool,
    retired: CancellationToken,
    tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    targets: RwLock<Vec<super::cluster::Target>>,
    pub registry: Arc<Registry>,
    pub reporting: Arc<ReportingQueue>,
}
impl HomeClient {
    pub async fn start(connector: Connector, reporting: Arc<ReportingQueue>) -> Result<Arc<Self>> {
        Self::start_with_instance(connector, reporting, &uuid::Uuid::new_v4().to_string()).await
    }
    pub async fn start_with_instance(
        connector: Connector,
        reporting: Arc<ReportingQueue>,
        instance: &str,
    ) -> Result<Arc<Self>> {
        tracing::info!(operation = "register", "Home registration started");
        let mut bootstrap = diagnosed(connector.connect().await, "config_connect")?;
        let reply = diagnosed(
            bootstrap
                .command(&[b"GET".to_vec(), b"config".to_vec()])
                .await,
            "config_fetch",
        )?;
        let bytes = diagnosed(reply.bytes(), "config_response")?;
        let initial = parse_config(&bytes, "config_initial")?;
        validate_support(&initial)?;
        drop(bootstrap);
        let mut sub = diagnosed(connector.connect().await, "subscription_connect")?;
        let revision = initial.lifecycle.lifecycle_config_revision;

        let ack = diagnosed(
            sub.command(&[
                b"SUBSCRIBE".to_vec(),
                b"config".to_vec(),
                revision.to_string().into_bytes(),
                instance.as_bytes().to_vec(),
            ])
            .await,
            "subscription_register",
        )?;
        diagnosed(
            (|| {
                ensure!(
                    matches!(ack,Frame::Array(Some(ref values)) if values.len()==3 && text(&values[0])==Some("subscribe") && text(&values[1])==Some("config") && matches!(values[2],Frame::Integer(1))),
                    "invalid subscription acknowledgement"
                );
                Ok(())
            })(),
            "subscription_ack",
        )?;
        let first = diagnosed(sub.receive().await, "config_snapshot_receive")?;
        let bytes = diagnosed(
            message(&first, "config").context("missing initial configuration snapshot"),
            "config_snapshot_missing",
        )?;
        let config = parse_config(bytes, "config_snapshot")?;
        validate_support(&config)?;
        diagnosed(
            (|| {
                ensure!(
                    config.lifecycle.lifecycle_config_revision == revision,
                    "lifecycle revision changed during registration"
                );
                Ok(())
            })(),
            "registration_config_revision",
        )?;
        // A fresh connection accepted after registration proves controlled command ownership.
        let mut worker = diagnosed(connector.connect().await, "worker_connect")?;
        let pong = diagnosed(worker.command(&[b"PING".to_vec()]).await, "worker_ping")?;
        diagnosed(
            (|| {
                ensure!(
                    matches!(pong, Frame::Simple(ref value) if value == "PONG"),
                    "Home worker ping failed"
                );
                Ok(())
            })(),
            "worker_ping_response",
        )?;
        drop(worker);
        let cancel = CancellationToken::new();
        let client = Arc::new(Self {
            connector,
            config: RwLock::new(Arc::new(config)),
            registry: Registry::new(cancel.clone()),
            cancel,
            ready: AtomicBool::new(true),
            retired: CancellationToken::new(),
            tasks: std::sync::Mutex::new(Vec::new()),
            targets: RwLock::new(Vec::new()),
            reporting,
        });
        {
            let mut tasks = client.tasks.lock().unwrap_or_else(|e| e.into_inner());
            tasks.push(tokio::spawn(client.clone().subscription(sub)));
            tasks.push(tokio::spawn(client.clone().maintenance()));
        }
        tracing::info!(operation = "register", revision, "Home membership ready");
        Ok(client)
    }
    pub fn config(&self) -> Arc<Config> {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire) && !self.cancel.is_cancelled()
    }
    pub fn fence(&self) {
        self.fence_reason("external");
    }
    fn fence_reason(&self, reason: &'static str) {
        // CancellationToken has no atomic cancel-and-test; serialize first-fence logging
        // with the task-owner lock, including after stop_admission clears readiness.
        let _tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        fence_membership(&self.ready, &self.cancel, reason);
    }
    /// Stop admission while keeping this membership alive during graceful drain.
    pub fn stop_admission(&self) {
        self.ready.store(false, Ordering::Release);
    }

    pub fn discovered_targets(&self) -> Vec<super::cluster::Target> {
        self.targets
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Permanently close old-lifetime I/O, then join every maintenance owner.
    /// External dispatch owners settle through the client cancellation budget.
    pub async fn retire(&self) -> Result<()> {
        self.fence_reason("retire");
        // Dispatch owners retain their response read for one cancellation budget.
        let settlement = tokio::time::Instant::now() + self.config().lifecycle.cancel_bound;
        while self.registry.pending_admissions() != 0 && tokio::time::Instant::now() < settlement {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        self.retired.cancel();
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|e| e.into_inner()));
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
        let deadline = tokio::time::Instant::now() + self.config().lifecycle.cancel_bound;
        while self.registry.pending_admissions() != 0 || self.registry.active_count() != 0 {
            ensure!(
                tokio::time::Instant::now() < deadline,
                "old Home lifetime has live execution/admission owners; replacement forbidden"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        Ok(())
    }

    pub async fn drain(&self) -> Result<()> {
        let deadline = tokio::time::Instant::now() + self.config().lifecycle.cancel_bound;
        loop {
            for frame in self.registry.pending_releases() {
                tokio::time::timeout_at(deadline, self.flush_release(&frame)).await??;
            }
            for delivery in self.reporting.pending_batch(64, 1024 * 1024) {
                let reply =
                    tokio::time::timeout_at(deadline, self.send(delivery.command())).await??;
                ensure!(
                    matches!(reply, Frame::Integer(1)),
                    "usage drain not acknowledged"
                );
                self.reporting.acknowledge(&delivery.id);
            }
            if self.registry.is_drained() && self.reporting.is_empty() {
                return Ok(());
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "shutdown drain deadline exceeded"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    pub async fn command(&self, args: Vec<Vec<u8>>) -> Result<Frame> {
        ensure!(self.ready(), "Home lifetime unavailable");
        self.send(args).await
    }
    async fn send(&self, args: Vec<Vec<u8>>) -> Result<Frame> {
        tokio::select! {
            biased;
            _ = self.retired.cancelled() => bail!("Home lifetime retired"),
            result = async {
                let mut connection = self.connector.connect().await?;
                connection.command(&args).await
            } => result,
        }
    }
    pub async fn dispatch(&self, request: Value) -> Result<Selection> {
        ensure!(self.ready(), "Home lifetime unavailable");
        // Once sent, cancellation must not cause the caller to retry an uncertain admission.
        let send = self.send(vec![b"RPOP".to_vec(), serde_json::to_vec(&request)?]);
        tokio::pin!(send);
        // Keep the response owner alive after cancellation, but never indefinitely.
        // A timed-out admission is ambiguous and requires Home reclamation, not takeover.
        let result = tokio::select! {
            result = &mut send => result,
            _ = self.cancel.cancelled() => {
                match tokio::time::timeout(self.config().lifecycle.cancel_bound, &mut send).await {
                    Ok(result) => result,
                    Err(_) => { self.fence_reason("dispatch_settlement_timeout"); bail!("Home admission settlement deadline exceeded"); }
                }
            }
        };
        let bytes = match result.and_then(Frame::bytes) {
            Ok(v) => v,
            Err(_) => {
                self.fence_reason("dispatch_transport");
                bail!("ambiguous Home dispatch transport failure")
            }
        };
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => {
                self.fence_reason("dispatch_response");
                bail!("invalid Home dispatch response")
            }
        };
        if value.get("error").is_some() {
            return Err(Rejection(value).into());
        }
        let selection: Selection = match serde_json::from_value(value) {
            Ok(v) => v,
            Err(_) => {
                self.fence_reason("dispatch_selection");
                bail!("malformed Home selection")
            }
        };
        if selection.model.is_empty()
            || selection.provider.is_empty()
            || selection
                .auth
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            || selection
                .concurrency
                .as_ref()
                .is_some_and(|v| v.accounted && (v.credential_id.is_empty() || v.model.is_empty()))
        {
            self.fence_reason("dispatch_identity");
            bail!("invalid Home admission identity");
        }
        Ok(selection)
    }
    pub async fn flush_release(&self, frame: &ReleaseFrame) -> Result<()> {
        let config = self.config();
        let deadline = tokio::time::Instant::now() + config.lifecycle.cancel_bound;
        let mut delay = config.lifecycle.release_flush_interval;
        loop {
            let send = self.send(vec![
                b"LPUSH".to_vec(),
                b"concurrency-release".to_vec(),
                serde_json::to_vec(frame)?,
            ]);
            match tokio::time::timeout_at(deadline, send).await {
                Ok(Ok(Frame::Integer(1))) => {
                    self.registry.acknowledge(frame);
                    return Ok(());
                }
                _ if tokio::time::Instant::now() >= deadline => {
                    tracing::warn!(
                        operation = "release",
                        reason = "ack_deadline",
                        "Home release not acknowledged; reclamation required"
                    );
                    bail!("release acknowledgement deadline exceeded")
                }
                _ => {}
            }
            tokio::time::sleep_until((tokio::time::Instant::now() + delay).min(deadline)).await;
            delay = (delay * 2).min(config.lifecycle.release_max_backoff);
        }
    }
    async fn subscription(self: Arc<Self>, mut sub: Connection) {
        let mut last_heartbeat = tokio::time::Instant::now();
        let reason = loop {
            let deadline = last_heartbeat + self.config().lifecycle.heartbeat_timeout;
            let frame = tokio::select! {
                _=self.cancel.cancelled()=>return,
                result=tokio::time::timeout_at(deadline,sub.receive())=>match result {
                    Ok(Ok(v))=>v,
                    Ok(Err(_))=>break "subscription_socket",
                    Err(_)=>break "heartbeat_timeout",
                }
            };
            if matches!(&frame,Frame::Array(Some(v)) if v.len()==2 && text(&v[0])==Some("pong") && text(&v[1])==Some(""))
            {
                last_heartbeat = tokio::time::Instant::now();
                tracing::debug!(operation = "heartbeat", "Home heartbeat received");
            } else if let Some(bytes) = message(&frame, "config") {
                let mut next = match parse_config(bytes, "config_update") {
                    Ok(next) => next,
                    Err(_) => break "config_parse",
                };
                if validate_support(&next).is_err() {
                    break "config_unsupported";
                }
                let old = self.config();
                if next.lifecycle.lifecycle_config_revision
                    != old.lifecycle.lifecycle_config_revision
                {
                    break "config_revision";
                }
                next.host = old.host.clone();
                next.port = old.port;
                *self.config.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);
                tracing::info!(operation = "config_update", "Home configuration applied");
            } else if let Some(bytes) = message(&frame, "cluster") {
                self.install_targets(bytes);
            } else if message(&frame, "rebalance").is_some() {
                tracing::warn!("Home rebalance is not supported in this release");
            }
        };
        self.fence_reason(reason);
    }
    fn install_targets(&self, bytes: &[u8]) {
        if let Ok(reply) = super::cluster::NodesReply::decode(Frame::Bulk(Some(bytes.to_vec()))) {
            *self.targets.write().unwrap_or_else(|e| e.into_inner()) = reply
                .nodes
                .into_iter()
                .map(|node| super::cluster::Target {
                    ip: node.ip,
                    port: node.port,
                })
                .collect();
        }
    }
    async fn maintenance(self: Arc<Self>) {
        let mut next_snapshot = tokio::time::Instant::now();
        let mut usage_failed = false;
        loop {
            let cfg = self.config();
            tokio::select! { _=self.cancel.cancelled()=>break, _=tokio::time::sleep(cfg.lifecycle.release_flush_interval)=>{} }
            for frame in self.registry.pending_releases() {
                if self.flush_release(&frame).await.is_err() {
                    self.fence_reason("release_ack_deadline");
                    return;
                }
            }
            for delivery in self.reporting.pending_batch(64, 1024 * 1024) {
                match self.command(delivery.command()).await {
                    Ok(Frame::Integer(1)) => {
                        self.reporting.acknowledge(&delivery.id);
                        if usage_failed {
                            tracing::info!(operation = "usage", "Home usage delivery recovered");
                            usage_failed = false;
                        }
                    }
                    _ => {
                        if !usage_failed {
                            tracing::warn!(
                                operation = "usage",
                                reason = "delivery_not_acknowledged",
                                "Home usage delivery deferred; durable batch retained"
                            );
                            usage_failed = true;
                        }
                        break;
                    }
                }
            }
            if tokio::time::Instant::now() >= next_snapshot {
                // Discovery is advisory; older standalone Homes may not support it.
                if let Ok(Ok(Frame::Bulk(Some(bytes)))) = tokio::time::timeout(
                    cfg.lifecycle.cancel_bound,
                    self.command(vec![b"CLUSTER".to_vec(), b"NODES".to_vec()]),
                )
                .await
                {
                    self.install_targets(&bytes);
                }
                match self.registry.snapshot(
                    &cfg.observation,
                    chrono::Utc::now(),
                    cfg.lifecycle.observation_barrier_revision,
                ) {
                    Ok(snapshot) => {
                        for frame in snapshot.frames {
                            let Ok(bytes) = serde_json::to_vec(&frame) else {
                                self.fence_reason("snapshot_serialize");
                                return;
                            };
                            if !matches!(
                                self.command(vec![
                                    b"LPUSH".to_vec(),
                                    b"in-flight-snapshot".to_vec(),
                                    bytes
                                ])
                                .await,
                                Ok(Frame::Integer(1))
                            ) {
                                self.fence_reason("snapshot_not_acknowledged");
                                return;
                            }
                        }
                    }
                    Err(_) => {
                        self.fence_reason("snapshot_build");
                        return;
                    }
                }
                next_snapshot = tokio::time::Instant::now() + cfg.observation.snapshot_interval;
            }
        }
    }
}
fn text(frame: &Frame) -> Option<&str> {
    match frame {
        Frame::Simple(s) => Some(s),
        Frame::Bulk(Some(b)) => std::str::from_utf8(b).ok(),
        _ => None,
    }
}
fn message<'a>(frame: &'a Frame, channel: &str) -> Option<&'a [u8]> {
    match frame {
        Frame::Array(Some(v))
            if v.len() == 3 && text(&v[0]) == Some("message") && text(&v[1]) == Some(channel) =>
        {
            match &v[2] {
                Frame::Bulk(Some(b)) => Some(b),
                _ => None,
            }
        }
        _ => None,
    }
}
// Caller serializes this transition with the membership's task-owner lock.
fn fence_membership(ready: &AtomicBool, cancel: &CancellationToken, reason: &'static str) {
    ready.store(false, Ordering::Release);
    if !cancel.is_cancelled() {
        cancel.cancel();
        tracing::warn!(
            operation = "fence",
            reason,
            "Home membership health lost; admission disabled"
        );
    }
}

// Never format upstream errors: RESP errors and parser diagnostics may contain secrets.
fn diagnosed<T>(result: Result<T>, operation: &'static str) -> Result<T> {
    if result.is_err() {
        tracing::warn!(
            operation,
            reason = "operation_failed",
            "Home operation failed"
        );
    }
    result
}
fn parse_config(bytes: &[u8], operation: &'static str) -> Result<Config> {
    diagnosed(
        Config::parse(bytes).map_err(|_| TerminalConfig.into()),
        operation,
    )
}
// unsupported_features includes arbitrary nested YAML keys. Emit only known schema
// families, deduplicated and bounded, never a user-controlled path or value.
fn unsupported_field_names(paths: &[String]) -> Vec<&'static str> {
    const FIELDS: &[&str] = &[
        "upstream",
        "client",
        "providers",
        "codex",
        "claude",
        "claude-code",
        "antigravity",
        "xai",
        "devin",
        "codex-header-defaults",
        "claude-header-defaults",
        "gemini-api-key",
        "interactions-api-key",
        "vertex-api-key",
        "codex-api-key",
        "claude-api-key",
        "xai-api-key",
        "meta-api-key",
        "openai-compatibility",
        "multimedia",
        "oauth-request-scoped-errors",
        "disable-claude-cloak-mode",
        "force-model-prefix",
        "request-log",
        "ws-auth",
        "api-keys",
        "shutdown-timeout",
        "redis-usage-queue-retention-seconds",
        "plugins",
        "oauth",
        "requests",
        "streaming",
        "nonstream-keepalive-interval",
        "passthrough-headers",
        "tls-handshake-timeout-seconds",
        "server",
        "response-compression",
        "trusted-proxies",
        "tls",
        "disable-image-generation",
        "gpt-image-2-base-model",
        "video-result-auth-cache-ttl",
        "antigravity-signature-cache-enabled",
        "antigravity-signature-bypass-strict",
        "routing",
        "access",
        "observability",
        "proxy-url",
    ];
    FIELDS
        .iter()
        .copied()
        .filter(|field| {
            paths.iter().any(|path| {
                path == field
                    || path
                        .strip_prefix(field)
                        .is_some_and(|suffix| suffix.starts_with(['.', '[']))
            })
        })
        .collect()
}
fn validate_support(config: &Config) -> Result<()> {
    let unsupported = config.unsupported_features();
    if !unsupported.is_empty() {
        for field in unsupported_field_names(&unsupported) {
            tracing::error!(
                operation = "config_validate",
                reason = "unsupported_field",
                field,
                "Home configuration unsupported"
            );
        }
        return Err(TerminalConfig.into());
    }
    Ok(())
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    use std::io::Write;

    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }

    #[test]
    fn diagnostics_do_not_format_errors_yaml_or_arbitrary_keys() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(capture.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let secret = "https://private.example/eyJ-secret-token";
        let result: Result<()> = diagnosed(Err(anyhow::anyhow!(secret)), "worker_ping");
        assert!(result.is_err());
        assert!(parse_config(secret.as_bytes(), "config_initial").is_err());
        let config = Config::parse(format!("credential-concurrency:\n  lifecycle-config-revision: 7\nproviders:\n  '{secret}': secret-value\n").as_bytes()).unwrap();
        assert!(validate_support(&config).is_err());
        let bytes = capture.0.lock().unwrap();
        let log = std::str::from_utf8(&bytes).unwrap();
        assert!(log.contains("worker_ping"));
        assert!(log.contains("config_initial"));
        assert!(log.contains("providers"));
        for forbidden in [
            secret,
            "secret-value",
            "private.example",
            "eyJ-secret-token",
        ] {
            assert!(!log.contains(forbidden), "unsafe diagnostic");
        }
    }

    #[test]
    fn first_fence_logs_once_even_after_admission_stopped() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(capture.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let ready = AtomicBool::new(false);
        let cancel = CancellationToken::new();
        fence_membership(&ready, &cancel, "heartbeat_timeout");
        fence_membership(&ready, &cancel, "subscription_socket");
        assert!(cancel.is_cancelled());
        assert!(!ready.load(Ordering::Acquire));
        let bytes = capture.0.lock().unwrap();
        let log = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(log.matches("Home membership health lost").count(), 1);
        assert!(log.contains("heartbeat_timeout"));
        assert!(!log.contains("subscription_socket"));
    }

    #[test]
    fn unsupported_names_are_bounded_schema_families_only() {
        let paths = [
            "providers.secret.token",
            "providers[0].password",
            "plugins.enabled",
            "unknown.secret",
            "providers-secret",
        ]
        .map(str::to_owned);
        assert_eq!(
            unsupported_field_names(&paths),
            vec!["providers", "plugins"]
        );
    }
}
