//! Planned handoffs are accepted only after local execution and release owners drain.
//! Unlike Go's shared registry, Rust registries are lifetime-owned; never move live counters.
use crate::home::{
    client::HomeClient,
    cluster::{Cluster, Instruction, MigrationCapability, Outcome},
};
use std::sync::{Arc, atomic::Ordering};

impl HomeClient {
    pub(super) fn cluster_control(self: &Arc<Self>, instruction: Option<Instruction>) {
        if self.membership.legacy()
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
            if !matches!(cluster.enable_rebalance().await, Ok(reply) if reply.ok) {
                return;
            }
            self.balance_enabled.store(true, Ordering::Release);
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
        // Fence admission before examining owners. Issued dispatches are represented by
        // pending_admissions until their guard owns the resulting accounting identity.
        self.stop_admission();
        if !self.registry.is_drained() || self.registry.pending_admissions() != 0 {
            self.ready.store(true, Ordering::Release);
            return;
        }
        // Control RPCs must remain available while gateway admission is stopped.
        let Ok(claim) = instruction.claim_command() else {
            self.ready.store(true, Ordering::Release);
            return;
        };
        let accepted = crate::home::cluster::RebalanceReply::decode(match self.send(claim).await {
            Ok(reply) => reply,
            Err(_) => {
                self.fence_reason("rebalance_claim");
                return;
            }
        });
        if !matches!(accepted, Ok(reply) if reply.ok) {
            self.ready.store(true, Ordering::Release);
            return;
        }
        let preflight = async {
            let mut connection = target.connect().await?;
            anyhow::ensure!(matches!(connection.command(&[b"PING".to_vec()]).await?, crate::home::resp::Frame::Simple(ref pong) if pong == "PONG"), "rebalance preflight rejected");
            anyhow::Ok(())
        }.await;
        if preflight.is_err()
            || !self.registry.is_drained()
            || self.registry.pending_admissions() != 0
        {
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
            self.ready.store(true, Ordering::Release);
            return;
        }
        if !self.cancel.is_cancelled() {
            self.membership.migrate(instruction);
            self.fence_reason("planned_rebalance");
        }
    }
}
