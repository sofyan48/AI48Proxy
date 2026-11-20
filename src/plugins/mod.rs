//! Native CLIProxy-compatible plugin ABI support.
//! This module owns loading and lifetime only; higher-level Home callbacks are
//! deliberately explicit and fail closed until their contract is implemented.

mod native;
pub use native::{AbiVersion, NativePlugin, PluginBuffer, PluginCapabilities};

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
