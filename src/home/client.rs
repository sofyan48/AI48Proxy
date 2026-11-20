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

const PLUGIN_SYNC_OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const PLUGIN_STATUS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[path = "config_validation.rs"]
mod config_validation;
#[path = "client_lifecycle.rs"]
mod lifecycle;
use config_validation::{parse_config, validate_support};
#[path = "latency.rs"]
pub(super) mod latency;
#[path = "rebalance.rs"]
mod rebalance;
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
    membership: Arc<super::membership::Membership>,
    control_busy: AtomicBool,
    balance_enabled: AtomicBool,
    stable: AtomicBool,
    latency_supported: AtomicBool,
    sampler: std::sync::Mutex<latency::Sampler>,
    handoff: CancellationToken,

    instance: String,
    config: RwLock<Arc<Config>>,
    cancel: CancellationToken,
    ready: AtomicBool,
    retired: CancellationToken,
    tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    targets: RwLock<Vec<super::cluster::Target>>,
    pub registry: Arc<Registry>,
    pub reporting: super::reporting::HomeReporting,
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
        Self::start_membership(
            connector,
            reporting,
            instance,
            Arc::new(super::membership::Membership::default()),
        )
        .await
    }
    pub(super) async fn start_membership(
        connector: Connector,
        reporting: Arc<ReportingQueue>,
        instance: &str,
        membership: Arc<super::membership::Membership>,
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
        validate_worker_capabilities(&initial)?;
        drop(bootstrap);
        let mut sub = diagnosed(connector.connect().await, "subscription_connect")?;
        let revision = initial.lifecycle.lifecycle_config_revision;

        let ack = diagnosed(
            sub.command(&membership.subscription(revision, instance))
                .await,
            "subscription_register",
        )?;
        if let Frame::Error(error) = &ack {
            membership.rejection(error);
        }
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
        if revision > 0 {
            membership.mark_eligible();
        }
        let first = diagnosed(sub.receive().await, "config_snapshot_receive")?;
        let bytes = diagnosed(
            message(&first, "config").context("missing initial configuration snapshot"),
            "config_snapshot_missing",
        )?;
        let config = parse_config(bytes, "config_snapshot")?;
        validate_support(&config)?;
        validate_worker_capabilities(&config)?;
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
            reporting: super::reporting::HomeReporting::new(
                reporting,
                membership.clone(),
                connector.clone(),
            ),
            connector,
            membership: membership.clone(),
            control_busy: AtomicBool::new(false),
            stable: AtomicBool::new(!membership.managed.load(Ordering::Acquire)),
            balance_enabled: AtomicBool::new(false),
            latency_supported: AtomicBool::new(false),
            sampler: std::sync::Mutex::new(latency::Sampler::default()),
            handoff: CancellationToken::new(),

            instance: instance.to_owned(),
            config: RwLock::new(Arc::new(config)),
            registry: Registry::new(cancel.clone()),
            cancel,
            ready: AtomicBool::new(true),
            retired: CancellationToken::new(),
            tasks: std::sync::Mutex::new(Vec::new()),
            targets: RwLock::new(Vec::new()),
        });
        {
            let mut tasks = client.tasks.lock().unwrap_or_else(|e| e.into_inner());
            tasks.push(tokio::spawn(client.clone().subscription(sub)));
            tasks.push(tokio::spawn(client.clone().maintenance()));
            tasks.push(tokio::spawn(client.clone().latency_reports()));
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
                    tokio::time::timeout_at(deadline, self.reporting.deliver(&delivery)).await??;
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
        ensure!(!self.cancel.is_cancelled(), "Home lifetime unavailable");
        self.send(args).await
    }
    /// Forward one request-log envelope over the existing mTLS command path.
    pub async fn push_request_log(&self, payload: Vec<u8>) -> Result<()> {
        let reply = self
            .command(vec![b"RPUSH".to_vec(), b"request-log".to_vec(), payload])
            .await?;
        ensure!(
            matches!(reply, Frame::Integer(1)),
            "request log not acknowledged"
        );
        Ok(())
    }

    /// Forward one application-log envelope over the existing mTLS command path.
    pub async fn push_app_log(&self, payload: Vec<u8>) -> Result<()> {
        let reply = self
            .command(vec![b"RPUSH".to_vec(), b"app-log".to_vec(), payload])
            .await?;
        ensure!(
            matches!(reply, Frame::Integer(1)),
            "application log not acknowledged"
        );
        Ok(())
    }

    /// Request the Home-owned plugin synchronization plan for this mTLS worker.
    /// The request and response are sensitive because manifests may contain URLs
    /// and verification material; callers must not log either body.
    pub async fn plugin_sync(&self, request: Value) -> Result<Value> {
        let command = self.command(vec![
            b"GET".to_vec(),
            b"plugin-sync".to_vec(),
            serde_json::to_vec(&request)?,
        ]);
        let frame = tokio::time::timeout(PLUGIN_SYNC_OPERATION_TIMEOUT, command)
            .await
            .context("Home plugin sync timed out")??;
        let bytes = frame.bytes().context("invalid Home plugin sync frame")?;
        let response: Value =
            serde_json::from_slice(&bytes).context("invalid Home plugin sync response")?;
        if response
            .pointer("/error/type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.eq_ignore_ascii_case("plugin_sync_unsupported"))
        {
            bail!("Home plugin sync unsupported")
        }
        Ok(response)
    }

    /// Fetch pending Home plugin tasks for this worker node.
    pub async fn plugin_tasks(&self) -> Result<Value> {
        let bytes = self
            .command(vec![b"GET".to_vec(), b"plugin-tasks".to_vec()])
            .await?
            .bytes()
            .context("invalid Home plugin task frame")?;
        serde_json::from_slice(&bytes).context("invalid Home plugin task response")
    }

    /// Acknowledge a plugin status report after local processing has completed.
    pub async fn push_plugin_status(&self, payload: Vec<u8>) -> Result<()> {
        let command = self.command(vec![b"RPUSH".to_vec(), b"plugin-status".to_vec(), payload]);
        let reply = tokio::time::timeout(PLUGIN_STATUS_TIMEOUT, command)
            .await
            .context("Home plugin status timed out")??;
        ensure!(
            matches!(reply, Frame::Integer(1)),
            "plugin status not acknowledged"
        );
        Ok(())
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
                    Err(_) => { self.membership.suppress_takeover(); self.fence_reason("dispatch_settlement_timeout"); bail!("Home admission settlement deadline exceeded"); }
                }
            }
        };
        let bytes = match result.and_then(Frame::bytes) {
            Ok(v) => v,
            Err(_) => {
                self.membership.suppress_takeover();
                self.fence_reason("dispatch_transport");
                bail!("ambiguous Home dispatch transport failure")
            }
        };
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => {
                self.membership.suppress_takeover();
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
                self.membership.suppress_takeover();
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
            self.membership.suppress_takeover();
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
}
fn validate_worker_capabilities(config: &Config) -> Result<()> {
    // AI48Proxy currently has no native plugin host/ABI loader. Home's plugin
    // scheduler/auth/model/usage paths therefore must not be enabled: accepting
    // this snapshot would advertise a worker that cannot execute its contract.
    if config
        .raw
        .get("plugins")
        .and_then(|value| value.get("enabled"))
        .and_then(serde_yaml::Value::as_bool)
        == Some(true)
    {
        bail!("Home plugin execution is not implemented by this worker")
    }
    Ok(())
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
