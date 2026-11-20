//! Subscription-local tokens and process-wide, bounded RTT history.
use super::HomeClient;
use crate::home::cluster::{LatencyReport, LatencySample, RebalanceReply, Target};
use std::{collections::BTreeMap, sync::atomic::Ordering, time::Duration};
use tokio::time::Instant;

pub(super) const TIMEOUT: Duration = Duration::from_secs(3);
const TTL: Duration = Duration::from_secs(120);
#[derive(Default)]
pub(crate) struct History {
    sequence: i64,
    entries: BTreeMap<String, Entry>,
    baseline: Option<Instant>,
}
struct Entry {
    sample: LatencySample,
    measured: Instant,
    next_cycle: u64,
    next_at: Option<Instant>,
    backoff: Duration,
}
impl History {
    fn observe(&mut self, target: Target, rtt: Duration, valid: bool, now: Instant) {
        let key = format!("{}:{}", target.ip, target.port);
        if self.entries.len() >= 64
            && !self.entries.contains_key(&key)
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.measured)
                .map(|(k, _)| k.clone())
        {
            self.entries.remove(&oldest);
        }
        let previous = self.entries.remove(&key);
        let mut micros = rtt.as_micros().min(60_000_000) as i64;
        if valid
            && let Some(old) = &previous
            && old.sample.valid
            && now.saturating_duration_since(old.measured) <= TTL
        {
            micros = (old.sample.rtt_us * 3 + micros) / 4;
        }
        self.sequence += 1;
        let mut entry = previous.unwrap_or_else(|| Entry {
            sample: LatencySample {
                ip: String::new(),
                port: 0,
                sequence: 0,
                observed_at: chrono::Utc::now(),
                age_ms: 0,
                rtt_us: 0,
                valid: false,
            },
            measured: now,
            next_cycle: 0,
            next_at: None,
            backoff: Duration::ZERO,
        });
        entry.sample = LatencySample {
            ip: target.ip,
            port: target.port,
            sequence: self.sequence,
            observed_at: chrono::Utc::now(),
            age_ms: 0,
            rtt_us: micros.max(1),
            valid: valid && !rtt.is_zero(),
        };
        entry.measured = now;
        self.entries.insert(key, entry);
    }
    fn report(&self, now: Instant) -> Vec<LatencySample> {
        self.entries
            .values()
            .filter_map(|e| {
                let age = now.checked_duration_since(e.measured)?;
                if age > TTL {
                    return None;
                }
                let mut sample = e.sample.clone();
                sample.age_ms = age.as_millis() as i64;
                Some(sample)
            })
            .collect()
    }
    fn cycle(&self, at: Instant) -> u64 {
        self.baseline.map_or(0, |base| {
            ((at.saturating_duration_since(base).as_nanos() + 30_000_000_000) / 60_000_000_000)
                as u64
        })
    }
    fn due(&self, target: &Target, at: Instant) -> bool {
        self.entries
            .get(&format!("{}:{}", target.ip, target.port))
            .is_none_or(|e| {
                e.next_at.is_none_or(|next| at >= next) && self.cycle(at) >= e.next_cycle
            })
    }
    fn probe(&mut self, target: Target, rtt: Duration, valid: bool, at: Instant, now: Instant) {
        self.observe(target.clone(), rtt, valid, now);
        self.baseline.get_or_insert(at);
        let cycle = self.cycle(at);
        let entry = self
            .entries
            .get_mut(&format!("{}:{}", target.ip, target.port))
            .unwrap();
        entry.next_cycle = cycle + 1;
        entry.backoff = if valid {
            Duration::ZERO
        } else {
            (entry.backoff * 2)
                .max(Duration::from_secs(60))
                .min(Duration::from_secs(600))
        };
        entry.next_at = (!valid).then_some(now + entry.backoff);
    }
}
#[derive(Default)]
pub(super) struct Sampler {
    pending: Option<(String, Instant)>,
}
impl Sampler {
    pub fn begin(&mut self, home: &HomeClient) -> String {
        self.expire(home, Instant::now(), true);
        let token = format!("home-rtt/{}", uuid::Uuid::new_v4());
        self.pending = Some((token.clone(), Instant::now()));
        token
    }
    pub fn pong(&mut self, home: &HomeClient, token: &str, now: Instant) {
        if let Some((expected, sent)) = &self.pending
            && token == expected
        {
            let rtt = now.checked_duration_since(*sent);
            home.membership
                .latency
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .observe(
                    home.connector.node(),
                    rtt.unwrap_or_default().max(Duration::from_micros(1)),
                    rtt.is_some_and(|v| v <= TIMEOUT),
                    now,
                );
            self.pending = None;
        }
    }
    pub fn expire(&mut self, home: &HomeClient, now: Instant, force: bool) {
        if self
            .pending
            .as_ref()
            .is_some_and(|(_, at)| force || now.saturating_duration_since(*at) > TIMEOUT)
        {
            home.membership
                .latency
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .observe(home.connector.node(), Duration::ZERO, false, now);
            self.pending = None;
        }
    }
}
fn random_nanos(bound: u64) -> u64 {
    (uuid::Uuid::new_v4().as_u128() as u64) % bound
}
pub(super) fn sample_delay() -> Duration {
    Duration::from_secs(27) + Duration::from_nanos(random_nanos(6_000_000_000))
}
fn report_delay() -> Duration {
    Duration::from_secs(60) + Duration::from_nanos(1 + random_nanos(60_000_000_000))
}
#[cfg(test)]
#[path = "latency_tests.rs"]
mod tests;

impl HomeClient {
    pub(super) async fn latency_reports(self: std::sync::Arc<Self>) {
        let mut ticker =
            tokio::time::interval_at(Instant::now() + report_delay(), Duration::from_secs(60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let at = tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.handoff.cancelled() => return,
                at = ticker.tick() => at,
            };
            if self.stable.load(Ordering::Acquire)
                && self.membership.managed.load(Ordering::Acquire)
                && self.ready()
                && self.balance_enabled.load(Ordering::Acquire)
                && self.latency_supported.load(Ordering::Acquire)
                && self.report_latency(at).await.is_err()
            {
                tracing::debug!(
                    operation = "latency",
                    reason = "telemetry_failed",
                    "Home latency report deferred"
                );
            }
        }
    }
    async fn report_latency(&self, at: Instant) -> anyhow::Result<()> {
        self.sampler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expire(self, Instant::now(), false);
        let samples = self
            .membership
            .latency
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .report(Instant::now());
        let report = LatencyReport {
            process_id: self.instance.clone(),
            samples,
        };
        let reply = RebalanceReply::decode(
            tokio::time::timeout(TIMEOUT, self.command(report.command()?)).await??,
        )?;
        anyhow::ensure!(reply.ok, "latency report rejected");
        let Some(target) = reply.probe else {
            return Ok(());
        };
        if target == self.connector.node()
            || !self.discovered_targets().contains(&target)
            || !self
                .membership
                .latency
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .due(&target, at)
        {
            return Ok(());
        }
        let result = tokio::time::timeout(TIMEOUT, async {
            let connector = self
                .connector
                .for_target(target.ip.clone(), target.port)
                .await?;
            let mut connection = connector.connect().await?;
            anyhow::ensure!(
                connection.command(&[b"PING".to_vec()]).await?.bytes()? == b"PONG",
                "probe warmup rejected"
            );
            let token = format!("home-probe/{}", uuid::Uuid::new_v4());
            let started = Instant::now();
            let reply = connection
                .command(&[b"PING".to_vec(), token.as_bytes().to_vec()])
                .await?;
            anyhow::ensure!(reply.bytes()? == token.as_bytes(), "probe echo rejected");
            anyhow::Ok(started.elapsed().max(Duration::from_micros(1)))
        })
        .await;
        let rtt = result.ok().and_then(Result::ok);
        self.membership
            .latency
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .probe(
                target,
                rtt.unwrap_or_default(),
                rtt.is_some(),
                at,
                Instant::now(),
            );
        Ok(())
    }
}
