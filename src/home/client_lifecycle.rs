use super::*;

impl HomeClient {
    pub(super) async fn subscription(self: Arc<Self>, sub: Connection) {
        let (mut reader, mut writer) = sub.split();
        let (frames_tx, mut frames) = tokio::sync::mpsc::channel(64);
        let receive_cancel = self.cancel.child_token();
        let receive_stop = receive_cancel.clone();
        let receiver = tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    _ = receive_stop.cancelled() => break,
                    frame = super::super::resp::read_frame(&mut reader) => frame,
                };
                let failed = frame.is_err();
                if frames_tx
                    .send((frame, tokio::time::Instant::now()))
                    .await
                    .is_err()
                    || failed
                {
                    break;
                }
            }
        });

        let mut sample_at = tokio::time::Instant::now() + super::latency::sample_delay();
        let mut last_heartbeat = tokio::time::Instant::now();
        let reason = loop {
            let deadline = last_heartbeat + self.config().lifecycle.heartbeat_timeout;
            let frame = tokio::select! {
                _=self.cancel.cancelled()=>break "cancelled",
                _=self.handoff.cancelled()=>break "planned_rebalance",

                _=tokio::time::sleep_until(sample_at)=> {
                    sample_at = tokio::time::Instant::now() + super::latency::sample_delay();
                    if self.latency_supported.load(Ordering::Acquire) && self.balance_enabled.load(Ordering::Acquire) {
                        let token = self.sampler.lock().unwrap_or_else(|e| e.into_inner()).begin(&self);
                        let args = [b"PING".to_vec(), token.into_bytes()];
                        let sent = tokio::select! {
                            _ = self.cancel.cancelled() => break "cancelled",
                            _ = self.handoff.cancelled() => break "planned_rebalance",
                            sent = tokio::time::timeout_at(deadline, crate::home::resp::write_command(&mut writer, &args)) => sent,
                        };
                        if !matches!(sent, Ok(Ok(()))) {
                            break "telemetry_ping";
                        }
                    }
                    continue;
                },
                result=tokio::time::timeout_at(deadline,frames.recv())=>match result {
                    Ok(Some((Ok(v), received_at)))=> {
                        if let Frame::Array(Some(values)) = &v
                            && values.len() == 2 && text(&values[0]) == Some("pong")
                            && let Some(token) = text(&values[1])
                            && token.starts_with("home-rtt/") {
                            self.sampler.lock().unwrap_or_else(|e| e.into_inner()).pong(&self, token, received_at);
                            continue;
                        }
                        v
                    },
                    Ok(_)=>break "subscription_socket",
                    Err(_) =>break "heartbeat_timeout",
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
                if validate_support(&next).is_err() || validate_worker_capabilities(&next).is_err()
                {
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
                self.cluster_control(None);
            } else if let Some(bytes) = message(&frame, "rebalance")
                && let Ok(instruction) = crate::home::cluster::Instruction::decode(bytes)
            {
                self.cluster_control(Some(instruction));
            }
        };
        receive_cancel.cancel();
        receiver.abort();
        let _ = receiver.await;
        self.sampler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expire(&self, tokio::time::Instant::now(), true);
        if reason == "planned_rebalance" || reason == "cancelled" {
            return;
        }
        if reason == "heartbeat_timeout" {
            self.membership.mark_timeout();
        }
        self.fence_reason(reason);
    }
    fn install_targets(&self, bytes: &[u8]) {
        if let Ok(reply) =
            crate::home::cluster::NodesReply::decode(Frame::Bulk(Some(bytes.to_vec())))
        {
            *self.targets.write().unwrap_or_else(|e| e.into_inner()) = reply
                .nodes
                .into_iter()
                .map(|node| crate::home::cluster::Target {
                    ip: node.ip,
                    port: node.port,
                })
                .collect();
        }
    }
    pub(super) async fn maintenance(self: Arc<Self>) {
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
                let result = self.reporting.deliver(&delivery).await;
                match result {
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
            if !self.handoff.is_cancelled() && tokio::time::Instant::now() >= next_snapshot {
                // Discovery is advisory; older standalone Homes may not support it.
                if let Ok(Ok(Frame::Bulk(Some(bytes)))) = tokio::time::timeout(
                    cfg.lifecycle.cancel_bound,
                    self.command(vec![b"CLUSTER".to_vec(), b"NODES".to_vec()]),
                )
                .await
                {
                    self.install_targets(&bytes);
                    self.cluster_control(None);
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
