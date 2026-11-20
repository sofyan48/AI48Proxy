//! Home discovery and rebalance wire helpers.
//! Schemas: Home internal/cluster/{resp.go,rebalance.go,rebalance_latency.go}.
use super::{client::HomeClient, resp::Frame};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const REBALANCE_CAPABILITY: &str = "cpa_rebalance_v1";
pub const LATENCY_CAPABILITY: &str = "cpa_latency_swap_v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub ip: String,
    pub port: u16,
}
impl Target {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.ip.trim().is_empty() && self.port != 0,
            "invalid Home target"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Node {
    pub ip: String,
    pub port: u16,
    #[serde(default, deserialize_with = "nonnegative_count")]
    pub client_count: u64,
    #[serde(default)]
    pub is_master: bool,
    #[serde(default)]
    pub last_seen_at: Option<DateTime<Utc>>,
}
fn nonnegative_count<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u64, D::Error> {
    Ok(i64::deserialize(deserializer)?.max(0) as u64)
}
#[derive(Debug, Clone, Deserialize)]
pub struct NodesReply {
    pub ok: bool,
    pub nodes: Vec<Node>,
}
impl NodesReply {
    pub fn decode(frame: Frame) -> Result<Self> {
        let mut reply: Self = decode_json(frame)?;
        ensure!(reply.ok, "Home rejected cluster discovery");
        let mut seen = std::collections::HashSet::new();
        reply.nodes.retain_mut(|node| {
            node.ip = node.ip.trim().to_owned();
            super::transport::validate_target(&node.ip, node.port).is_ok()
                && seen.insert((node.ip.clone(), node.port))
        });
        reply.nodes.sort_by_key(|node| node.client_count);
        Ok(reply)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instruction {
    pub id: String,
    pub term: i64,
    pub target: Target,
}
impl Instruction {
    /// Decode the JSON payload on the existing `rebalance` subscription channel.
    pub fn decode(payload: &[u8]) -> Result<Self> {
        let instruction: Self =
            serde_json::from_slice(payload).context("invalid Home rebalance instruction")?;
        instruction.validate()?;
        Ok(instruction)
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.trim().is_empty() && self.term > 0,
            "invalid rebalance identity"
        );
        self.target.validate()
    }
    pub fn claim_command(&self) -> Result<Vec<Vec<u8>>> {
        self.validate()?;
        let mut args = rebalance_command("CLAIM");
        args.extend([
            self.id.as_bytes().to_vec(),
            self.term.to_string().into_bytes(),
            self.target.ip.as_bytes().to_vec(),
            self.target.port.to_string().into_bytes(),
        ]);
        Ok(args)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RebalanceReply {
    /// False is a valid rejection (e.g. stale claim), not a transport failure.
    pub ok: bool,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub probe: Option<Target>,
}
impl RebalanceReply {
    pub fn decode(frame: Frame) -> Result<Self> {
        let reply: Self = decode_json(frame)?;
        if let Some(probe) = &reply.probe {
            probe.validate()?;
        }
        Ok(reply)
    }
    pub fn supports_latency(&self) -> bool {
        self.capabilities.iter().any(|v| v == LATENCY_CAPABILITY)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencySample {
    pub ip: String,
    pub port: u16,
    pub sequence: i64,
    pub observed_at: DateTime<Utc>,
    pub age_ms: i64,
    pub rtt_us: i64,
    pub valid: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyReport {
    pub process_id: String,
    pub samples: Vec<LatencySample>,
}
impl LatencyReport {
    pub fn command(&self) -> Result<Vec<Vec<u8>>> {
        ensure!(
            !self.process_id.is_empty() && self.process_id.len() <= 128,
            "invalid latency process ID"
        );
        ensure!(self.samples.len() <= 64, "too many latency samples");
        for sample in &self.samples {
            Target {
                ip: sample.ip.clone(),
                port: sample.port,
            }
            .validate()?;
            ensure!(
                sample.sequence > 0
                    && (0..=180_000).contains(&sample.age_ms)
                    && (0..=60_000_000).contains(&sample.rtt_us)
                    && (!sample.valid || sample.rtt_us > 0),
                "invalid latency sample"
            );
        }
        let payload = serde_json::to_vec(self)?;
        ensure!(payload.len() <= 32768, "latency report too large");
        let mut args = rebalance_command("LATENCY");
        args.push(payload);
        Ok(args)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MigrationCapability {
    #[default]
    Unsupported,
    /// The caller has implemented safe migration outside these wire helpers.
    SupportedByCaller,
}
impl MigrationCapability {
    fn require(self) -> Result<()> {
        ensure!(
            self == Self::SupportedByCaller,
            "rebalance requires actual migration support"
        );
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failed,
}
impl Outcome {
    pub fn command(self, id: &str) -> Result<Vec<Vec<u8>>> {
        ensure!(!id.trim().is_empty(), "empty rebalance ID");
        let mut args = rebalance_command("RESULT");
        args.push(id.as_bytes().to_vec());
        args.push(match self {
            Self::Success => b"success".to_vec(),
            Self::Failed => b"failed".to_vec(),
        });
        Ok(args)
    }
}

pub struct Cluster<'a> {
    home: &'a HomeClient,
    capability: MigrationCapability,
}
impl<'a> Cluster<'a> {
    /// Construction performs no I/O and never enables rebalance.
    pub fn new(home: &'a HomeClient) -> Self {
        Self {
            home,
            capability: MigrationCapability::Unsupported,
        }
    }
    pub fn with_migration_support(home: &'a HomeClient, capability: MigrationCapability) -> Self {
        Self { home, capability }
    }
    pub fn migration_capability(&self) -> MigrationCapability {
        self.capability
    }
    pub async fn nodes(&self) -> Result<NodesReply> {
        NodesReply::decode(
            self.home
                .command(vec![b"CLUSTER".to_vec(), b"NODES".to_vec()])
                .await?,
        )
    }
    /// Explicit opt-in only. No discovery, latency report, or claim calls this.
    pub async fn enable_rebalance(&self) -> Result<RebalanceReply> {
        self.capability.require()?;
        RebalanceReply::decode(self.home.command(rebalance_command("ENABLE")).await?)
    }
    pub async fn claim(&self, instruction: &Instruction) -> Result<RebalanceReply> {
        self.capability.require()?;
        RebalanceReply::decode(self.home.command(instruction.claim_command()?).await?)
    }
    /// Success must be reported through the new target's acknowledged HomeClient.
    pub async fn result(&self, id: &str, outcome: Outcome) -> Result<RebalanceReply> {
        self.capability.require()?;
        RebalanceReply::decode(self.home.command(outcome.command(id)?).await?)
    }
    /// Home must already have accepted ENABLE for this lifetime. Does not enable it.
    pub async fn latency(&self, report: &LatencyReport) -> Result<RebalanceReply> {
        self.capability.require()?;
        RebalanceReply::decode(self.home.command(report.command()?).await?)
    }
}
fn rebalance_command(action: &str) -> Vec<Vec<u8>> {
    vec![
        b"CLUSTER".to_vec(),
        b"BALANCE".to_vec(),
        action.as_bytes().to_vec(),
    ]
}
fn decode_json<T: serde::de::DeserializeOwned>(frame: Frame) -> Result<T> {
    let Frame::Bulk(Some(bytes)) = frame else {
        anyhow::bail!("expected Home JSON bulk reply")
    };
    serde_json::from_slice(&bytes).context("invalid Home cluster reply")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bulk(raw: &str) -> Frame {
        Frame::Bulk(Some(raw.as_bytes().to_vec()))
    }
    #[test]
    fn discovery_schema() {
        let reply = NodesReply::decode(bulk(r#"{"ok":true,"nodes":[{"ip":"::1","port":8327,"client_count":2,"is_master":true,"last_seen_at":"2026-01-01T00:00:00Z"}]}"#)).unwrap();
        assert_eq!(reply.nodes[0].client_count, 2);
        for raw in [
            r#"{"ok":false,"nodes":[]}"#,
            r#"{"ok":true}"#,
            r#"{"ok":true,"nodes":[{"ip":"x","port":0,"client_count":-1,"is_master":true,"last_seen_at":"bad"}]}"#,
        ] {
            assert!(NodesReply::decode(bulk(raw)).is_err());
        }
        assert!(NodesReply::decode(Frame::Error("private".into())).is_err());
    }
    #[test]
    fn instruction_and_commands() {
        let instruction =
            Instruction::decode(br#"{"id":"plan","term":4,"target":{"ip":"::1","port":8327}}"#)
                .unwrap();
        assert_eq!(
            instruction.claim_command().unwrap(),
            ["CLUSTER", "BALANCE", "CLAIM", "plan", "4", "::1", "8327"]
                .map(|v| v.as_bytes().to_vec())
        );
        assert_eq!(Outcome::Failed.command("plan").unwrap()[4], b"failed");
        assert!(Outcome::Success.command("").is_err());
        assert!(
            Instruction::decode(br#"{"id":"p","term":0,"target":{"ip":"x","port":1}}"#).is_err()
        );
        assert!(MigrationCapability::default().require().is_err());
        assert!(MigrationCapability::SupportedByCaller.require().is_ok());
    }
    #[test]
    fn rebalance_reply_and_latency_schema() {
        let reply = RebalanceReply::decode(bulk(r#"{"ok":false,"capabilities":["cpa_latency_swap_v1"],"probe":{"ip":"127.0.0.1","port":8327}}"#)).unwrap();
        assert!(!reply.ok && reply.supports_latency());
        assert!(
            !RebalanceReply::decode(bulk(r#"{"ok":true}"#))
                .unwrap()
                .supports_latency()
        );
        for raw in [
            r#"{}"#,
            r#"{"ok":1}"#,
            r#"{"ok":true,"probe":{"ip":"x","port":0}}"#,
        ] {
            assert!(RebalanceReply::decode(bulk(raw)).is_err());
        }
        let mut report = LatencyReport {
            process_id: "process".into(),
            samples: vec![LatencySample {
                ip: "::1".into(),
                port: 8327,
                sequence: 1,
                observed_at: "2026-01-01T00:00:00Z".parse().unwrap(),
                age_ms: 0,
                rtt_us: 42,
                valid: true,
            }],
        };
        let args = report.command().unwrap();
        assert_eq!(args[2], b"LATENCY");
        let decoded: LatencyReport = serde_json::from_slice(&args[3]).unwrap();
        assert_eq!(decoded.samples[0].rtt_us, 42);
        report.samples[0].rtt_us = 0;
        assert!(report.command().is_err());
        report.samples.clear();
        report.process_id.clear();
        assert!(report.command().is_err());
    }
}
