//! A stable process identity owns recovery policy across fenced transport lifetimes.
use super::{HomeClient, TerminalConfig};
use crate::{gateway, home::transport::Connector, reporting::ReportingQueue};
use anyhow::{Result, bail};
use axum::{
    Router,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;

/// Seeded full jitter with an exponential ceiling; independent of wall-clock time.
pub struct Backoff {
    state: u64,
    attempt: u32,
}
impl Backoff {
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed.max(1),
            attempt: 0,
        }
    }
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
    pub fn next_delay(&mut self) -> Duration {
        let cap = (250u64 << self.attempt.min(7)).min(30_000);
        self.attempt = self.attempt.saturating_add(1);
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        Duration::from_millis(1 + self.state % cap)
    }
}

#[derive(Clone)]
struct Current {
    home: Arc<HomeClient>,
    router: Router,
}

pub struct Supervisor {
    current: RwLock<Option<Current>>,
    connector: Connector,
    reporting: Arc<ReportingQueue>,
    instance: String,
    membership: Arc<crate::home::membership::Membership>,
    discovered: RwLock<Vec<Connector>>,
    stop: CancellationToken,
}
impl Supervisor {
    pub fn new(connector: Connector, reporting: Arc<ReportingQueue>) -> Arc<Self> {
        Arc::new(Self {
            current: RwLock::new(None),
            connector,
            reporting,
            instance: uuid::Uuid::new_v4().to_string(),
            membership: Arc::new(crate::home::membership::Membership::managed()),
            discovered: RwLock::new(Vec::new()),
            stop: CancellationToken::new(),
        })
    }
    pub fn cancellation(&self) -> CancellationToken {
        self.stop.clone()
    }
    pub fn current(&self) -> Option<Arc<HomeClient>> {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|v| v.home.clone())
    }
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .fallback(dispatch)
            .with_state(self.clone())
            .layer(axum::middleware::from_fn(gateway::cors::cors))
    }
    fn install(&self, home: Arc<HomeClient>) {
        home.stable
            .store(true, std::sync::atomic::Ordering::Release);
        let router = gateway::router(home.clone());
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = Some(Current { home, router });
    }
    /// Initial configuration supplies the immutable listener address. Startup remains cancellable.
    pub async fn connect(&self) -> Result<Arc<HomeClient>> {
        let mut backoff = Backoff::new(seed(&self.instance));
        let mut connector = self.connector.clone();
        let discovery = tokio::select! {
            _ = self.stop.cancelled() => bail!("Home startup cancelled"),
            result = discover(&connector) => result,
        };
        if let Ok(nodes) = discovery {
            let mut targets = Vec::new();
            for node in nodes.nodes {
                if let Ok(target) = self.connector.for_target(node.ip, node.port).await {
                    targets.push(target);
                }
            }
            if let Some(target) = targets.first() {
                connector = target.clone();
            }
            *self.discovered.write().unwrap_or_else(|e| e.into_inner()) = targets;
        }
        let mut failures = 0;
        loop {
            tokio::select! {
                biased;
                _ = self.stop.cancelled() => bail!("Home startup cancelled"),
                result = register(connector.clone(), self.reporting.clone(), &self.instance, self.membership.clone()) => {
                    match result {
                        Ok(home) => { self.install(home.clone()); return Ok(home); }
                        Err(err) if err.downcast_ref::<TerminalConfig>().is_some() => {
                            tracing::error!(operation = "register", target_category = "configured", reason = "terminal_config", "Home startup stopped");
                            return Err(err);
                        }
                        Err(_) => {
                                                    failures += 1;
                                                    if failures >= 3 {
                                                        connector = next_target(&connector, &self.discovered.read().unwrap_or_else(|e| e.into_inner()), &self.connector);
                                                        failures = 0;
                                                    }
                                                    tracing::warn!(operation = "register", target_category = "configured", reason = "admission_unavailable", "Home registration unavailable");
                                                },
                    }
                }
            }
            tokio::select! {
                _ = self.stop.cancelled() => bail!("Home startup cancelled"),
                _ = tokio::time::sleep(backoff.next_delay()) => {}
            }
        }
    }
    /// Runs beside a single HTTP listener, including throughout disconnection and registration.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let mut home = self
            .current()
            .ok_or_else(|| anyhow::anyhow!("supervisor not connected"))?;
        let mut targets = self
            .discovered
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !targets
            .iter()
            .any(|target| target.target() == self.connector.target())
        {
            targets.push(self.connector.clone());
        }
        let mut connector = home.connector.clone();
        let mut failures = 0usize;
        let mut backoff = Backoff::new(seed(&self.instance));
        loop {
            let lifetime_cancel = home.cancellation();
            tokio::select! {
                _ = self.stop.cancelled() => {
                    home.stop_admission();
                    let config = home.config();
                    let bound = config.lifecycle.cancel_bound;
                    // HTTP draining is opt-in; lifecycle cleanup retains its separate safety bound.
                    let drained = tokio::time::timeout(config.shutdown_timeout, home.drain()).await;
                    home.fence_reason("shutdown");
                    // Cancellation completes stream guards and may enqueue their final usage.
                    let final_drain = tokio::time::timeout(bound, home.drain()).await;
                    home.retire().await?;
                    if !matches!(drained, Ok(Ok(()))) && !matches!(final_drain, Ok(Ok(()))) {
                        tracing::warn!("Home shutdown drain incomplete; durable usage retained for replay");
                    }
                    return Ok(());
                }
                _ = lifetime_cancel.cancelled() => {}
                _ = home.handoff.cancelled() => {}
            }
            *self.current.write().unwrap_or_else(|e| e.into_inner()) = None;
            tracing::warn!(
                operation = "recovery",
                reason = "membership_fenced",
                "Home disconnected; gateway admission unavailable"
            );
            let snapshot = home.discovered_targets();
            if !snapshot.is_empty() {
                targets.clear();
            }
            for target in snapshot {
                match self.connector.for_target(target.ip, target.port).await {
                    Ok(connector) if !targets.iter().any(|v| v.target() == connector.target()) => {
                        targets.push(connector)
                    }
                    Ok(_) => {}
                    Err(_) => tracing::debug!(
                        operation = "discovery",
                        target_category = "discovered",
                        reason = "invalid_target",
                        "Home recovery target ignored"
                    ),
                }
            }
            let migration = self.membership.take_migration();
            let planned = migration.is_some() && !home.cancel.is_cancelled();
            // All releases below use the OLD connector and registry. No release is replayed
            // after normal registration: reclaimed counters belong exclusively to Home.
            if !planned {
                match tokio::time::timeout(home.config().lifecycle.cancel_bound, home.drain()).await
                {
                    Ok(Ok(())) => {
                        tracing::info!(operation = "drain", "Old Home membership drained")
                    }
                    Ok(Err(_)) => tracing::warn!(
                        operation = "drain",
                        reason = "operation_failed",
                        "Old Home drain incomplete; waiting for reclamation"
                    ),
                    Err(_) => tracing::warn!(
                        operation = "drain",
                        reason = "timeout",
                        "Old Home drain deadline reached; waiting for reclamation"
                    ),
                }
                home.retire().await?;
                // Preserving counters is safe only when all old releases are acknowledged.
                // This worker cannot rebind a live registry as the Go worker does.
                if !home.registry.is_drained() {
                    self.membership.suppress_takeover();
                }
            }
            let origin = connector.clone();
            let source = home.clone();
            // Start the origin owner before registration: target or rollback failures must
            // not orphan source executions, maintenance tasks, or shutdown cancellation.
            if planned {
                let source = source.clone();
                let stop = self.stop.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = stop.cancelled() => break,
                            _ = source.cancel.cancelled() => break,
                            _ = tokio::time::sleep(source.config().lifecycle.release_flush_interval) => {}
                        }
                        if source.registry.is_drained() && source.reporting.is_empty() {
                            break;
                        }
                    }
                    let _ = source.retire().await;
                });
            }

            if let Some(plan) = &migration {
                connector = self
                    .connector
                    .for_target(plan.target.ip.clone(), plan.target.port)
                    .await?;
            } else if self.membership.take_timeout() {
                connector = next_target(&connector, &targets, &self.connector);
                failures = 0;
            } else {
                failures += 1;
                if failures >= 3 {
                    connector = next_target(&connector, &targets, &self.connector);
                    failures = 0;
                }
            }
            let reporting = source.reporting.clone();
            let mut index = 0;
            loop {
                let delay = if migration.is_some() && index == 0 {
                    Duration::ZERO
                } else {
                    backoff.next_delay()
                };
                tokio::select! {
                    _ = self.stop.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(delay) => {}
                }
                let target_category = recovery_target_category(usize::from(
                    connector.target() != self.connector.target(),
                ));
                tracing::info!(
                    operation = "recovery_register",
                    target_category,
                    attempt = index + 1,
                    "Home recovery registration started"
                );
                index += 1;
                let result = tokio::select! {
                    _ = self.stop.cancelled() => return Ok(()),
                    result = register(connector.clone(), reporting.clone(), &self.instance, self.membership.clone()) => result,
                };
                match result {
                    Ok(next) => {
                        home = next;
                        failures = 0;
                        if let Some(plan) = migration.as_ref() {
                            let planned_target = self
                                .connector
                                .for_target(plan.target.ip.clone(), plan.target.port)
                                .await?;
                            let outcome = if connector.target() == planned_target.target() {
                                crate::home::cluster::Outcome::Success
                            } else {
                                crate::home::cluster::Outcome::Failed
                            };
                            *self
                                .membership
                                .pending_result
                                .lock()
                                .unwrap_or_else(|e| e.into_inner()) =
                                Some((plan.id.clone(), outcome));
                            let cluster = crate::home::cluster::Cluster::with_migration_support(
                                &home,
                                crate::home::cluster::MigrationCapability::SupportedByCaller,
                            );
                            let result = tokio::select! {
                                _ = self.stop.cancelled() => return Ok(()),
                                _ = home.cancel.cancelled() => None,
                                result = async {
                                    let enabled = cluster.enable_rebalance().await?;
                                    if !enabled.ok {
                                        bail!("Home rebalance enable rejected");
                                    }
                                    home.latency_supported.store(enabled.supports_latency(), std::sync::atomic::Ordering::Release);
                                    home.balance_enabled.store(true, std::sync::atomic::Ordering::Release);
                                    cluster.result(&plan.id, outcome).await
                                } => Some(result),
                            };
                            if !matches!(result, Some(Ok(reply)) if reply.ok) {
                                home.fence_reason("rebalance_result");
                                home.retire().await?;
                                connector = origin.clone();
                                continue;
                            }
                        }

                        *self
                            .membership
                            .pending_result
                            .lock()
                            .unwrap_or_else(|e| e.into_inner()) = None;
                        self.install(home.clone());
                        backoff.reset();
                        tracing::info!(
                            operation = "recovery",
                            target_category,
                            "Home membership recovered; gateway admission restored"
                        );
                        break;
                    }
                    Err(err) if err.downcast_ref::<TerminalConfig>().is_some() => {
                        if migration.is_some() && connector.target() != origin.target() {
                            connector = origin.clone();
                            continue;
                        }
                        tracing::error!(
                            operation = "recovery",
                            target_category,
                            reason = "terminal_config",
                            "Home recovery stopped"
                        );
                        return Err(err);
                    }
                    Err(_) => {
                        if migration.is_some() && connector.target() != origin.target() {
                            connector = origin.clone();
                        } else {
                            failures += 1;
                            if failures >= 3 {
                                connector = next_target(&connector, &targets, &self.connector);
                                failures = 0;
                            }
                        }
                        tracing::warn!(
                            operation = "recovery",
                            target_category,
                            reason = "admission_unavailable",
                            "Home recovery deferred; normal registration not accepted"
                        )
                    }
                }
            }
        }
    }
}
async fn discover(connector: &Connector) -> Result<crate::home::cluster::NodesReply> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut connection = connector.connect().await?;
        crate::home::cluster::NodesReply::decode(
            connection
                .command(&[b"CLUSTER".to_vec(), b"NODES".to_vec()])
                .await?,
        )
    })
    .await?
}
async fn register(
    connector: Connector,
    reporting: Arc<ReportingQueue>,
    instance: &str,
    membership: Arc<crate::home::membership::Membership>,
) -> Result<Arc<HomeClient>> {
    tokio::time::timeout(
        Duration::from_secs(30),
        HomeClient::start_membership(connector, reporting, instance, membership),
    )
    .await
    .map_err(|_| {
        tracing::warn!(
            operation = "register",
            reason = "timeout",
            "Home registration deadline exceeded"
        );
        anyhow::anyhow!("Home registration deadline exceeded")
    })?
}
fn next_target(current: &Connector, targets: &[Connector], seed: &Connector) -> Connector {
    targets
        .iter()
        .find(|target| target.target() != current.target())
        .unwrap_or(seed)
        .clone()
}
fn recovery_target_category(index: usize) -> &'static str {
    if index == 0 {
        "configured"
    } else {
        "discovered"
    }
}

fn seed(instance: &str) -> u64 {
    instance.bytes().fold(0xcbf29ce484222325u64, |n, b| {
        (n ^ b as u64).wrapping_mul(0x100000001b3)
    })
}
async fn dispatch(State(supervisor): State<Arc<Supervisor>>, request: Request) -> Response {
    let current = supervisor
        .current
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    match current {
        Some(current) if current.home.ready() => {
            let cancel = current.home.cancellation();
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                                    tracing::debug!(operation = "dispatch", reason = "membership_cancelled", status = 503, "Home admission unavailable");
                                    return (StatusCode::SERVICE_UNAVAILABLE, "Home is not ready").into_response();
                                }
                response = current.router.oneshot(request) => response.unwrap(),
            };
            let (parts, body) = response.into_parts();
            // Also drop streaming bodies when their owning membership is fenced.
            let stream = body.into_data_stream().take_until(cancel.cancelled_owned());
            Response::from_parts(parts, axum::body::Body::from_stream(stream))
        }
        _ => {
            tracing::debug!(
                operation = "dispatch",
                reason = "disconnected",
                status = 503,
                "Home admission unavailable"
            );
            (StatusCode::SERVICE_UNAVAILABLE, "Home is not ready").into_response()
        }
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn recovery_categories_never_include_target_identity() {
        assert_eq!(recovery_target_category(0), "configured");
        assert_eq!(recovery_target_category(1), "discovered");
        assert_eq!(recovery_target_category(usize::MAX), "discovered");
    }
}
