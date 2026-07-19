//! IPC protocol types and a blocking client.
//!
//! JSON-RPC 2.0 over a Unix domain socket, newline-framed — debuggable with
//! `nc`. Deliberately free of platform and backend dependencies so the GUI, the
//! CLI, and future REST/WS surfaces all speak the same shapes.

pub mod client;
pub mod protocol;

pub use client::{Client, ClientError};
pub use protocol::*;

use std::path::PathBuf;

/// Default daemon socket path.
///
/// Per-user under Application Support: the daemon is a LaunchAgent, not a
/// system service, and must never be shared between users.
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("DISPLAYD_SOCKET") {
        return PathBuf::from(p);
    }
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("DisplayStudio").join("displayd.sock")
}
