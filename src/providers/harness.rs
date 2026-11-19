//! Standalone validation while the parent agent owns lib.rs integration.
//! rustc --test can compile this against the existing ai48proxy build artifacts.
pub use ai48proxy::{config, home};
#[path = "../providers.rs"]
mod providers;
