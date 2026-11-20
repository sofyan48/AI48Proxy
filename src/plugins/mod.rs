//! Native CLIProxy-compatible plugin ABI support.
//! This module owns loading and lifetime only; higher-level Home callbacks are
//! deliberately explicit and fail closed until their contract is implemented.

mod home_contract;
mod manager;
mod native;
pub use home_contract::{
    InstallPlan, PluginArtifact, PluginManifest, PluginPlatform, PluginStatus, PluginStatusEntry,
    PluginSyncItem, PluginSyncRequest, PluginSyncResponse, PluginTask, ResolvedAuthConfig,
    parse_sync_plan, validate_plugin_path,
};
pub use manager::{PluginCapabilities, PluginInfo, PluginManager};
pub use native::{AbiVersion, NativePlugin, PluginBuffer};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_contract_uses_cli_proxy_version_one() {
        assert_eq!(AbiVersion::CURRENT.0, 1);
    }

    #[test]
    fn plugin_capabilities_default_to_no_execution() {
        let capabilities = PluginCapabilities::default();
        assert!(!capabilities.executor);
        assert!(!capabilities.scheduler);
        assert!(!capabilities.auth_provider);
        assert!(!capabilities.model_provider);
    }
}
