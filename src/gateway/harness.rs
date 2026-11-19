//! Standalone validation against the last stable core artifact while integration is in flight.
pub use ai48proxy::{config, execution, home, payload, reporting, thinking};
#[path = "../gateway.rs"]
mod gateway;
#[path = "../protocol.rs"]
mod protocol;
#[path = "../providers.rs"]
mod providers;
