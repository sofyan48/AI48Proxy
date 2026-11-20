//! Planned handoffs preserve origin-bound executions and release ledgers.
use crate::home::{
    client::HomeClient,
    cluster::{Cluster, Instruction, MigrationCapability, Outcome},
};
use std::sync::{Arc, atomic::Ordering};

impl HomeClient {
    pub(super) fn cluster_control(self: &Arc<Self>, instruction: Option<Instruction>) {
        if !self.stable.load(Ordering::Acquire)
            || !self.membership.managed.load(Ordering::Acquire)
            || !self.ready()
            || self.handoff.is_cancelled()
            || self.membership.legacy()
            || self.config().lifecycle.lifecycle_config_revision <= 0
            || self.control_busy.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let home = self.clone();
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = home.cancel.cancelled() => {},
                _ = home.balance(instruction) => {},
            }
            home.control_busy.store(false, Ordering::Release);
        });
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }
    async fn balance(&self, instruction: Option<Instruction>) {
        let cluster = Cluster::with_migration_support(self, MigrationCapability::SupportedByCaller);
        if !self.balance_enabled.load(Ordering::Acquire) {
            let Ok(reply) = cluster.enable_rebalance().await else {
                return;
            };
            if !reply.ok {
                return;
            }
            self.latency_supported
                .store(reply.supports_latency(), Ordering::Release);
            self.balance_enabled.store(true, Ordering::Release);
        }
        let pending = self
            .membership
            .pending_result
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some((id, outcome)) = pending {
            if matches!(cluster.result(&id, outcome).await, Ok(reply) if reply.ok) {
                *self
                    .membership
                    .pending_result
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None;
            } else {
                return;
            }
        }
        let Some(instruction) = instruction else {
            return;
        };
        let Ok(target) = self
            .connector
            .for_target(instruction.target.ip.clone(), instruction.target.port)
            .await
        else {
            return;
        };
        if target.target() == self.connector.target() || !self.ready() {
            return;
        }
        // Active and pending executions retain this lifetime; they do not block a move.
        let Ok(claim) = instruction.claim_command() else {
            return;
        };
        let accepted = crate::home::cluster::RebalanceReply::decode(match self.send(claim).await {
            Ok(reply) => reply,
            Err(_) => {
                self.fence_reason("rebalance_claim");
                return;
            }
        });
        match accepted {
            Ok(reply) if reply.ok => {}
            Ok(_) => return,
            Err(_) => {
                self.fence_reason("rebalance_claim_response");
                return;
            }
        }
        let preflight = async {
            let mut connection = target.connect().await?;
            anyhow::ensure!(matches!(connection.command(&[b"PING".to_vec()]).await?, crate::home::resp::Frame::Simple(ref pong) if pong == "PONG"), "rebalance preflight rejected");
            anyhow::Ok(())
        }.await;
        if preflight.is_err() {
            *self
                .membership
                .pending_result
                .lock()
                .unwrap_or_else(|e| e.into_inner()) =
                Some((instruction.id.clone(), Outcome::Failed));
            let acknowledged = match Outcome::Failed.command(&instruction.id) {
                Ok(command) => match self.send(command).await {
                    Ok(reply) => {
                        matches!(crate::home::cluster::RebalanceReply::decode(reply), Ok(reply) if reply.ok)
                    }
                    Err(_) => false,
                },
                Err(_) => false,
            };
            if !acknowledged {
                self.fence_reason("rebalance_failure_result");
                return;
            }
            *self
                .membership
                .pending_result
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            return;
        }
        if self.ready() {
            self.stop_admission();
            self.membership.migrate(instruction);
            self.handoff.cancel();
        }
    }
}
