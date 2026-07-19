//! Platform-independent display model and backend abstraction.
//!
//! Nothing here may reference IOKit, CoreGraphics, or any other macOS symbol —
//! this crate and `display-ddc` must stay buildable for a Linux target so the
//! 2.0 `/dev/i2c-*` backend can slot in behind [`DisplayBackend`]. Their only
//! dependencies are each other and `thiserror`, which is what currently keeps
//! that true; the plan's CI section adds a Linux job to enforce it rather than
//! trust it.

use display_ddc::vcp::VcpCode;
use std::fmt;

pub mod identity;
pub mod profile;
pub mod rules;

pub use identity::DisplayIdentity;
pub use profile::{DisplayProfile, Profile, ProfileError, ProfileStore};
pub use rules::{Action, PowerState, Rule, RuleSet, RulesError, RulesStore, Trigger};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("display not found: {0}")]
    NotFound(DisplayId),
    #[error("display does not support {0}")]
    Unsupported(VcpCode),
    #[error("value {value} out of range for {code} (max {max})")]
    OutOfRange { code: VcpCode, value: u16, max: u16 },
    #[error("transport failure: {0}")]
    Transport(String),
    #[error(transparent)]
    Ddc(#[from] display_ddc::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Opaque handle to a display. On macOS this wraps a `CGDirectDisplayID`, but
/// callers must not depend on that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DisplayId(pub u32);

impl fmt::Display for DisplayId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// How a display is driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPath {
    /// External display speaking DDC/CI over I2C.
    Ddc,
    /// Built-in panel driven through the OS rather than DDC.
    Native,
    /// Detected but not controllable (no AV service, blocked converter, dummy).
    None,
}

/// A display as presented to callers.
#[derive(Debug, Clone)]
pub struct Monitor {
    pub id: DisplayId,
    pub identity: DisplayIdentity,
    pub control: ControlPath,
    /// Present only once capabilities have been probed.
    pub capabilities: Option<display_ddc::caps::Capabilities>,
}

impl Monitor {
    pub fn is_controllable(&self) -> bool {
        !matches!(self.control, ControlPath::None)
    }
}

/// A platform backend. Implemented by `display-macos` today; a Linux
/// `/dev/i2c-*` backend can be added without touching the daemon.
pub trait DisplayBackend {
    fn list(&mut self) -> Result<Vec<Monitor>>;

    /// Ids of every currently-attached display.
    ///
    /// Must be cheap: callers poll it to notice hot-plug. Unlike [`list`] it may
    /// not touch I2C or rebuild any per-display state, because its whole purpose
    /// is to decide whether that expensive work is needed.
    fn online_ids(&mut self) -> Result<Vec<DisplayId>>;

    fn get_vcp(&mut self, id: DisplayId, code: VcpCode) -> Result<(u16, u16)>;
    fn set_vcp(&mut self, id: DisplayId, code: VcpCode, value: u16) -> Result<()>;
    /// Raw capability string, unparsed. `None` if the display has no DDC path.
    fn capability_string(&mut self, id: DisplayId) -> Result<Option<String>>;

    /// Current power source, for automation rules.
    ///
    /// Not display-related, but the platform backend is the only thing that
    /// knows. Defaults to `Unknown` so a backend need not implement it; rules
    /// never fire on `Unknown`, so the default is inert rather than wrong.
    fn power_source(&mut self) -> Result<crate::rules::PowerState> {
        Ok(crate::rules::PowerState::Unknown)
    }
}
