//! `displayd` internals, exposed as a library so `displayctl` can reuse the
//! engine when the daemon is not running.

pub mod automation;
pub mod engine;
pub mod rpc;
pub mod worker;

pub use engine::{Engine, EngineError};

pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");
