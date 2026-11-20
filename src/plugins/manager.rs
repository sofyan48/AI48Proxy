use super::NativePlugin;
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::RwLock;

const MAX_PLUGIN_REQUEST: usize = 8 * 1024 * 1024;
const MAX_PLUGIN_RESPONSE: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginCapabilities {
    #[serde(default)]
    pub executor: bool,
    #[serde(default)]
    pub scheduler: bool,
    #[serde(default)]
    pub auth_provider: bool,
    #[serde(default)]
    pub model_provider: bool,
    #[serde(default)]
    pub usage_plugin: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginInfo {
    pub id: String,
    pub path: PathBuf,
    pub capabilities: PluginCapabilities,
}

struct LoadedPlugin {
    info: PluginInfo,
    plugin: NativePlugin,
}

#[derive(Default)]
pub struct PluginManager {
    plugins: RwLock<BTreeMap<String, Arc<LoadedPlugin>>>,
}

impl PluginManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn load(
        &self,
        id: impl Into<String>,
        path: impl AsRef<Path>,
        config: &Value,
    ) -> Result<PluginInfo> {
        let id = id.into();
        validate_id(&id)?;
        let plugin = NativePlugin::load(path)?;
        let request = serde_json::to_vec(&serde_json::json!({"id": id, "config": config}))?;
        let response = call_checked(&plugin, "plugin.register", &request)?;
        let capabilities = response
            .get("capabilities")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        let info = PluginInfo {
            id: id.clone(),
            path: plugin.path().to_owned(),
            capabilities,
        };
        let loaded = Arc::new(LoadedPlugin {
            info: info.clone(),
            plugin,
        });
        let mut plugins = self.plugins.write().await;
        if let Some(previous) = plugins.remove(&id) {
            let _ = call_checked(&previous.plugin, "plugin.quiesce", b"{}");
            let _ = call_checked(&previous.plugin, "plugin.shutdown", b"{}");
        }
        plugins.insert(id, loaded);
        Ok(info)
    }

    pub async fn reconfigure(&self, id: &str, config: &Value) -> Result<()> {
        let plugin = self.get(id).await?;
        let request = serde_json::to_vec(&serde_json::json!({"config": config}))?;
        call_checked(&plugin.plugin, "plugin.reconfigure", &request)?;
        Ok(())
    }

    pub async fn call(&self, id: &str, method: &str, request: &[u8]) -> Result<Vec<u8>> {
        validate_method(method)?;
        ensure!(
            request.len() <= MAX_PLUGIN_REQUEST,
            "plugin request exceeds limit"
        );
        let plugin = self.get(id).await?;
        let response = plugin.plugin.call(method, request)?;
        ensure!(
            response.len() <= MAX_PLUGIN_RESPONSE,
            "plugin response exceeds limit"
        );
        Ok(response)
    }

    pub async fn unload(&self, id: &str) -> Result<bool> {
        let mut plugins = self.plugins.write().await;
        let Some(plugin) = plugins.remove(id) else {
            return Ok(false);
        };
        call_checked(&plugin.plugin, "plugin.quiesce", b"{}")?;
        call_checked(&plugin.plugin, "plugin.shutdown", b"{}")?;
        Ok(true)
    }

    pub async fn list(&self) -> Vec<PluginInfo> {
        self.plugins
            .read()
            .await
            .values()
            .map(|plugin| plugin.info.clone())
            .collect()
    }

    async fn get(&self, id: &str) -> Result<Arc<LoadedPlugin>> {
        self.plugins
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("plugin is not loaded"))
    }
}

fn call_checked(plugin: &NativePlugin, method: &str, request: &[u8]) -> Result<Value> {
    validate_method(method)?;
    let response = plugin.call(method, request)?;
    ensure!(
        response.len() <= MAX_PLUGIN_RESPONSE,
        "plugin response exceeds limit"
    );
    if response.is_empty() {
        return Ok(Value::Null);
    }
    let value: Value = serde_json::from_slice(&response)
        .map_err(|_| anyhow::anyhow!("plugin returned invalid JSON"))?;
    if value.get("error").is_some() {
        bail!("plugin rejected method");
    }
    Ok(value)
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(!id.is_empty() && id.len() <= 128, "invalid plugin id");
    ensure!(
        id.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.'),
        "invalid plugin id"
    );
    Ok(())
}
fn validate_method(method: &str) -> Result<()> {
    ensure!(
        !method.is_empty()
            && method.len() <= 128
            && method
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-'),
        "invalid plugin method"
    );
    Ok(())
}
