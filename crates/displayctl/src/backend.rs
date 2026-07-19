//! Daemon-or-direct access.
//!
//! Both arms run the same `Engine`; the daemon arm just runs it in another
//! process. Direct mode exists so the CLI works before the daemon is installed,
//! but it takes the hardware for itself — with `displayd` running, prefer the
//! socket so I2C stays serialized through one owner.

use display_api::protocol as api;
use display_api::{Client, ClientError};

#[cfg(target_os = "macos")]
use display_daemon::Engine;
#[cfg(target_os = "macos")]
use display_macos::MacosBackend;

pub enum Access {
    Daemon(Box<Client>),
    #[cfg(target_os = "macos")]
    Direct(Box<Engine<MacosBackend>>),
}

impl Access {
    /// Connect to the daemon, or fall back to direct hardware access.
    pub fn open(force_direct: bool) -> Result<Self, String> {
        if !force_direct {
            let path = display_api::socket_path();
            match Client::connect(&path) {
                Ok(c) => return Ok(Access::Daemon(Box::new(c))),
                // Daemon absent is the normal pre-install case, not an error.
                Err(ClientError::NotRunning(_)) => {}
                Err(e) => return Err(format!("daemon connection failed: {e}")),
            }
        }
        Self::direct()
    }

    #[cfg(target_os = "macos")]
    fn direct() -> Result<Self, String> {
        let backend = MacosBackend::new().map_err(|e| format!("backend unavailable: {e}"))?;
        Ok(Access::Direct(Box::new(Engine::new(backend))))
    }

    #[cfg(not(target_os = "macos"))]
    fn direct() -> Result<Self, String> {
        Err("displayctl requires macOS".into())
    }

    pub fn describe(&self) -> &'static str {
        match self {
            Access::Daemon(_) => "displayd (socket)",
            #[cfg(target_os = "macos")]
            Access::Direct(_) => "direct hardware access (displayd not running)",
        }
    }

    pub fn version(&mut self) -> Result<api::VersionInfo, String> {
        match self {
            Access::Daemon(c) => c.version().map_err(|e| e.to_string()),
            #[cfg(target_os = "macos")]
            Access::Direct(_) => Ok(api::VersionInfo {
                protocol: api::PROTOCOL_VERSION,
                daemon: "not running".into(),
            }),
        }
    }

    pub fn list(&mut self) -> Result<Vec<api::MonitorInfo>, String> {
        match self {
            Access::Daemon(c) => c.list().map_err(|e| e.to_string()),
            #[cfg(target_os = "macos")]
            Access::Direct(e) => e.list().map_err(|e| e.to_string()),
        }
    }

    pub fn get(&mut self, display: &str, code: &str) -> Result<api::VcpValue, String> {
        match self {
            Access::Daemon(c) => c.get(display, code).map_err(|e| e.to_string()),
            #[cfg(target_os = "macos")]
            Access::Direct(e) => e.get(display, code).map_err(|e| e.to_string()),
        }
    }

    pub fn set(
        &mut self,
        display: &str,
        code: &str,
        value: u16,
        verify: bool,
    ) -> Result<api::SetResult, String> {
        match self {
            Access::Daemon(c) => c
                .set(display, code, value, verify)
                .map_err(|e| e.to_string()),
            #[cfg(target_os = "macos")]
            Access::Direct(e) => e
                .set(display, code, value, verify)
                .map_err(|e| e.to_string()),
        }
    }

    pub fn caps(&mut self, display: &str) -> Result<api::CapsResult, String> {
        match self {
            Access::Daemon(c) => c.caps(display).map_err(|e| e.to_string()),
            #[cfg(target_os = "macos")]
            Access::Direct(e) => e.caps(display).map_err(|e| e.to_string()),
        }
    }

    pub fn profile_list(&mut self) -> Result<Vec<api::ProfileSummary>, String> {
        match self {
            Access::Daemon(c) => c.profile_list().map_err(|e| e.to_string()),
            #[cfg(target_os = "macos")]
            Access::Direct(e) => e.profile_list().map_err(|e| e.to_string()),
        }
    }

    pub fn profile_apply(
        &mut self,
        name: &str,
        verify: bool,
        yes: bool,
    ) -> Result<api::ApplyResult, String> {
        match self {
            Access::Daemon(c) => c
                .profile_apply(name, verify, yes)
                .map_err(|e| e.to_string()),
            #[cfg(target_os = "macos")]
            Access::Direct(e) => e
                .profile_apply(name, verify, yes)
                .map_err(|e| e.to_string()),
        }
    }

    pub fn profile_save(
        &mut self,
        name: &str,
        codes: &[String],
        force: bool,
    ) -> Result<api::ProfileSummary, String> {
        match self {
            Access::Daemon(c) => c
                .profile_save(name, codes, force)
                .map_err(|e| e.to_string()),
            #[cfg(target_os = "macos")]
            Access::Direct(e) => {
                let resolved = resolve_codes(codes)?;
                e.profile_save(name, &resolved, force)
                    .map_err(|e| e.to_string())
            }
        }
    }

    pub fn profile_delete(&mut self, name: &str) -> Result<(), String> {
        match self {
            Access::Daemon(c) => c.profile_delete(name).map_err(|e| e.to_string()),
            #[cfg(target_os = "macos")]
            Access::Direct(e) => e.profile_delete(name).map_err(|e| e.to_string()),
        }
    }

    pub fn profile_show(&mut self, name: &str) -> Result<display_core::Profile, String> {
        match self {
            Access::Daemon(c) => {
                let v = c.profile_show(name).map_err(|e| e.to_string())?;
                serde_json::from_value(v).map_err(|e| format!("bad profile from daemon: {e}"))
            }
            #[cfg(target_os = "macos")]
            Access::Direct(e) => e.profile_show(name).map_err(|e| e.to_string()),
        }
    }
}

/// Rule inspection.
///
/// Automation only runs inside the daemon — it needs a long-lived process to
/// observe edges, which a one-shot CLI cannot do. Rather than pretend otherwise,
/// direct mode says so.
impl Access {
    pub fn rules_list(&mut self) -> Result<Vec<api::RuleInfo>, String> {
        match self {
            Access::Daemon(c) => c.rules_list().map_err(|e| e.to_string()),
            #[cfg(target_os = "macos")]
            Access::Direct(_) => Err(NO_DAEMON_AUTOMATION.into()),
        }
    }

    pub fn rules_reload(&mut self) -> Result<Vec<api::RuleInfo>, String> {
        match self {
            Access::Daemon(c) => c.rules_reload().map_err(|e| e.to_string()),
            #[cfg(target_os = "macos")]
            Access::Direct(_) => Err(NO_DAEMON_AUTOMATION.into()),
        }
    }

    pub fn rules_tick(&mut self) -> Result<Vec<api::RuleFired>, String> {
        match self {
            Access::Daemon(c) => c.rules_tick().map_err(|e| e.to_string()),
            #[cfg(target_os = "macos")]
            Access::Direct(_) => Err(NO_DAEMON_AUTOMATION.into()),
        }
    }
}

#[cfg(target_os = "macos")]
const NO_DAEMON_AUTOMATION: &str =
    "automation needs displayd running — rules fire on state changes, which a one-shot \
     command cannot observe. Start the daemon, or use `displayctl rules path` to find the file.";

/// Mirror the daemon's default-code resolution for the direct path, so a profile
/// saved without the daemon captures the same codes as one saved with it.
#[cfg(target_os = "macos")]
fn resolve_codes(names: &[String]) -> Result<Vec<display_ddc::vcp::VcpCode>, String> {
    use display_ddc::vcp;
    if names.is_empty() {
        return Ok(display_daemon::rpc::DEFAULT_SNAPSHOT_CODES.to_vec());
    }
    names
        .iter()
        .map(|n| vcp::parse_code(n).ok_or_else(|| format!("unknown VCP code {n:?}")))
        .collect()
}
