//! Keep the durable outbox shared while preserving each execution's issuing Home.
use super::{membership::Membership, transport::Connector};
use crate::reporting::{EnqueueError, ReportingQueue};
use std::{ops::Deref, sync::Arc};

pub struct HomeReporting {
    queue: Arc<ReportingQueue>,
    membership: Arc<Membership>,
    origin: Connector,
}
impl HomeReporting {
    pub(super) fn new(
        queue: Arc<ReportingQueue>,
        membership: Arc<Membership>,
        origin: Connector,
    ) -> Self {
        Self {
            queue,
            membership,
            origin,
        }
    }
    pub fn enqueue(&self, payload: serde_json::Value) -> Result<String, EnqueueError> {
        // Reserve routing before publication, including Full retries made by gateway owners.
        let id = payload
            .get("execution_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let mut owners = self
            .membership
            .usage_owners
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(id) = &id {
            owners
                .entry(id.clone())
                .or_insert_with(|| self.origin.clone());
        }
        let result = self.queue.enqueue(payload);
        if let Ok(id) = &result {
            owners
                .entry(id.clone())
                .or_insert_with(|| self.origin.clone());
        }
        result
    }
    pub(super) async fn deliver(
        &self,
        delivery: &crate::reporting::Delivery,
    ) -> anyhow::Result<super::resp::Frame> {
        // Origin ownership is intentionally process-local. Go's redisqueue queueItem
        // carries only payload bytes, and service_home.startHomeUsageForwarder uses
        // its current client, not persisted origin metadata. After restart, our
        // durable spool likewise replays to the current Home; live handoffs retain
        // the issuing Home through usage_owners. Do not persist connectors/secrets.
        let owner = self
            .membership
            .usage_owners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&delivery.id)
            .cloned()
            .unwrap_or_else(|| self.origin.clone());
        let mut connection = owner.connect().await?;
        connection.command(&delivery.command()).await
    }
    pub fn acknowledge(&self, id: &str) -> bool {
        let mut owners = self
            .membership
            .usage_owners
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let acknowledged = self.queue.acknowledge(id);
        if acknowledged {
            owners.remove(id);
        }
        acknowledged
    }
}
impl Deref for HomeReporting {
    type Target = Arc<ReportingQueue>;
    fn deref(&self) -> &Self::Target {
        &self.queue
    }
}
