use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};
use url::Url;

pub const PLUGIN_SYNC_SCHEMA_VERSION: u32 = 1;
pub const PLUGIN_MANIFEST_SCHEMA_VERSION_V2: u32 = 2;
pub const MAX_PLUGIN_MANIFEST_SIZE: i64 = 512 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginSyncRequest {
    pub schema_version: u32,
    pub goos: String,
    pub goarch: String,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub installed_versions: std::collections::HashMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginSyncResponse {
    pub schema_version: u32,
    pub expires_at: DateTime<Utc>,
    pub items: Vec<PluginSyncItem>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginSyncItem {
    pub manifest: PluginManifest,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auth: Vec<ResolvedAuthConfig>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginManifest {
    #[serde(default)]
    pub schema_version: u32,
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    pub version: String,
    #[serde(default)]
    pub release_tag: String,
    #[serde(default)]
    pub repository: String,
    #[serde(default)]
    pub logo: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub install: InstallPlan,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct InstallPlan {
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub artifacts: Vec<PluginArtifact>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginArtifact {
    pub goos: String,
    pub goarch: String,
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub size: i64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResolvedAuthConfig {
    #[serde(default)]
    #[serde(rename = "match")]
    pub match_pattern: String,
    #[serde(default)]
    pub apply_to: Vec<String>,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub header_name: String,
    #[serde(default)]
    pub header_value: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginTask {
    pub id: u64,
    pub operation: String,
    pub plugin_id: String,
    #[serde(default)]
    pub target_node_type: String,
    #[serde(default)]
    pub target_node_id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginStatus {
    pub schema_version: u32,
    #[serde(default)]
    pub task_id: u64,
    pub task: String,
    pub node_id: String,
    #[serde(default)]
    pub client_ip: String,
    pub status: String,
    pub phase: String,
    pub ok: bool,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub platform: PluginPlatform,
    pub plugins: Vec<PluginStatusEntry>,
    #[serde(default)]
    pub error: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginPlatform {
    pub goos: String,
    pub goarch: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginStatusEntry {
    pub id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub release_tag: String,
    #[serde(default)]
    pub repository: String,
    pub install_status: String,
    #[serde(default)]
    pub load_status: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub skipped: bool,
    #[serde(default)]
    pub overwritten: bool,
    #[serde(default)]
    pub error: String,
}

impl PluginSyncRequest {
    pub fn new(goos: impl Into<String>, goarch: impl Into<String>) -> Self {
        Self {
            schema_version: PLUGIN_SYNC_SCHEMA_VERSION,
            goos: goos.into(),
            goarch: goarch.into(),
            installed_versions: Default::default(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == PLUGIN_SYNC_SCHEMA_VERSION,
            "unsupported plugin sync request schema_version {}",
            self.schema_version
        );
        ensure!(!self.goos.trim().is_empty(), "plugin sync goos is required");
        ensure!(
            !self.goarch.trim().is_empty(),
            "plugin sync goarch is required"
        );
        for id in self.installed_versions.keys() {
            validate_plugin_id(id)?;
        }
        Ok(())
    }
}

impl PluginSyncResponse {
    pub fn parse(bytes: &[u8], now: DateTime<Utc>) -> Result<Self> {
        let response: Self =
            serde_json::from_slice(bytes).context("decode plugin sync response")?;
        response.validate(now)?;
        Ok(response)
    }

    pub fn validate(&self, now: DateTime<Utc>) -> Result<()> {
        ensure!(
            self.schema_version == PLUGIN_SYNC_SCHEMA_VERSION,
            "unsupported plugin sync response schema_version {}",
            self.schema_version
        );
        ensure!(now < self.expires_at, "plugin sync response expired");
        let mut ids = std::collections::HashSet::new();
        for item in &self.items {
            item.manifest.validate()?;
            ensure!(ids.insert(item.manifest.id.clone()), "duplicate plugin id");
        }
        Ok(())
    }
}

impl PluginManifest {
    pub fn validate(&self) -> Result<()> {
        validate_plugin_id(&self.id)?;
        ensure!(
            !self.version.trim().is_empty(),
            "plugin version is required"
        );
        match self.install.kind.trim().to_ascii_lowercase().as_str() {
            "direct" => {
                if self.schema_version != 0
                    && self.schema_version != PLUGIN_MANIFEST_SCHEMA_VERSION_V2
                {
                    bail!(
                        "unsupported plugin manifest schema_version {}",
                        self.schema_version
                    );
                }
                ensure!(
                    !self.install.artifacts.is_empty(),
                    "direct plugin has no artifacts"
                );
                for artifact in &self.install.artifacts {
                    artifact.validate()?;
                }
            }
            "github-release" | "" => {
                ensure!(
                    !self.release_tag.trim().is_empty(),
                    "plugin release_tag is required"
                );
            }
            kind => bail!("unsupported plugin install type {kind:?}"),
        }
        Ok(())
    }
}

impl PluginArtifact {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.goos.trim().is_empty(),
            "plugin artifact goos is required"
        );
        ensure!(
            !self.goarch.trim().is_empty(),
            "plugin artifact goarch is required"
        );
        let url = Url::parse(self.url.trim()).context("invalid plugin artifact URL")?;
        ensure!(
            url.scheme() == "https",
            "plugin artifact URL must use https"
        );
        ensure!(
            url.host_str().is_some(),
            "plugin artifact URL host is required"
        );
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "plugin artifact URL has credentials"
        );
        ensure!(
            url.query().is_none() && url.fragment().is_none(),
            "plugin artifact URL has query or fragment"
        );
        ensure!(
            self.sha256.len() == 64 && self.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid plugin artifact sha256"
        );
        ensure!(
            (0..=MAX_PLUGIN_MANIFEST_SIZE).contains(&self.size),
            "invalid plugin artifact size"
        );
        Ok(())
    }
}

impl PluginStatus {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == PLUGIN_SYNC_SCHEMA_VERSION,
            "unsupported plugin status schema_version {}",
            self.schema_version
        );
        ensure!(
            !self.task.trim().is_empty(),
            "plugin status task is required"
        );
        ensure!(
            !self.node_id.trim().is_empty(),
            "plugin status node_id is required"
        );
        ensure!(
            !self.phase.trim().is_empty(),
            "plugin status phase is required"
        );
        for plugin in &self.plugins {
            validate_plugin_id(&plugin.id)?;
            validate_plugin_path(&plugin.path)?;
        }
        Ok(())
    }
}

pub fn parse_sync_plan(bytes: &[u8], now: DateTime<Utc>) -> Result<PluginSyncResponse> {
    PluginSyncResponse::parse(bytes, now)
}

pub fn validate_plugin_path(path: &str) -> Result<()> {
    if path.is_empty() {
        return Ok(());
    }
    let path = Path::new(path);
    ensure!(!path.is_absolute(), "plugin path must be relative");
    ensure!(
        !path.to_string_lossy().contains('\0'),
        "plugin path contains NUL"
    );
    for component in path.components() {
        ensure!(
            !matches!(component, Component::ParentDir),
            "plugin path escapes its root"
        );
    }
    Ok(())
}

fn validate_plugin_id(id: &str) -> Result<()> {
    ensure!(!id.is_empty() && id.len() <= 128, "invalid plugin id");
    ensure!(
        id.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')),
        "invalid plugin id"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn artifact() -> PluginArtifact {
        PluginArtifact {
            goos: "linux".into(),
            goarch: "amd64".into(),
            url: "https://downloads.example/plugin.zip".into(),
            sha256: "a".repeat(64),
            size: 42,
        }
    }

    fn response() -> PluginSyncResponse {
        PluginSyncResponse {
            schema_version: PLUGIN_SYNC_SCHEMA_VERSION,
            expires_at: Utc::now() + Duration::minutes(1),
            items: vec![PluginSyncItem {
                manifest: PluginManifest {
                    schema_version: PLUGIN_MANIFEST_SCHEMA_VERSION_V2,
                    id: "sample".into(),
                    version: "1.0.0".into(),
                    install: InstallPlan {
                        kind: "direct".into(),
                        artifacts: vec![artifact()],
                    },
                    ..Default::default()
                },
                auth: vec![],
            }],
        }
    }

    #[test]
    fn parses_and_validates_sync_plan() {
        let bytes = serde_json::to_vec(&response()).unwrap();
        assert_eq!(parse_sync_plan(&bytes, Utc::now()).unwrap().items.len(), 1);
    }

    #[test]
    fn rejects_bad_hash_size_and_url() {
        let mut manifest = response().items.remove(0).manifest;
        manifest.install.artifacts[0].sha256 = "bad".into();
        assert!(manifest.validate().is_err());
        manifest.install.artifacts[0] = PluginArtifact {
            url: "http://downloads.example/plugin.zip".into(),
            ..artifact()
        };
        assert!(manifest.validate().is_err());
        manifest.install.artifacts[0] = PluginArtifact {
            size: MAX_PLUGIN_MANIFEST_SIZE + 1,
            ..artifact()
        };
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn rejects_expired_duplicate_and_escaping_paths() {
        let mut plan = response();
        assert!(plan.validate(Utc::now() + Duration::minutes(2)).is_err());
        plan.items.push(plan.items[0].clone());
        assert!(plan.validate(Utc::now()).is_err());
        assert!(validate_plugin_path("../plugin.so").is_err());
        assert!(validate_plugin_path("plugin/plugin.so").is_ok());
    }
}
