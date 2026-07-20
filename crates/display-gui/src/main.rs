//! Display Studio menu bar app.
//!
//! A thin native client over `displayd`: it holds no hardware logic and speaks
//! the same Unix-socket JSON-RPC protocol as `displayctl`. All AppKit here, via
//! objc2 — the plan's "SwiftUI shell" replaced by a Rust-native one.

// objc2 moves methods between its safe and unsafe surfaces across releases, so a
// block that is redundant today becomes load-bearing after an upgrade. Wrapping
// AppKit calls defensively is deliberate; allow the churn rather than chase it.
#![allow(unused_unsafe)]

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("display-gui requires macOS");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
mod client;
mod config;
mod login;
mod mediakeys;
#[cfg(target_os = "macos")]
mod settings;
mod statusbar;

#[cfg(target_os = "macos")]
fn main() {
    statusbar::run();
}
