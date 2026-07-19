//! Backend-agnostic operations behind the RPC surface.
//!
//! Lives in the library rather than the binary so `displayctl` can run the exact
//! same code when it falls back to direct hardware access — a divergence between
//! "with daemon" and "without daemon" behaviour would be a bug factory.

use display_api::protocol as api;
use display_core::profile::{DisplayProfile, Profile, ProfileStore};
use display_core::{ControlPath, DisplayBackend, DisplayId, Monitor};
use display_ddc::caps;
use display_ddc::vcp::{self, ValueKind, VcpCode};
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("no display matches {0:?}")]
    NoMatch(String),
    #[error("{0:?} is ambiguous — matches {1}. Use --display with an id or a longer name.")]
    Ambiguous(String, String),
    #[error("no display specified and {0} displays are controllable — use --display")]
    NeedsSelector(usize),
    #[error("no controllable displays found")]
    NoDisplays,
    #[error("unknown VCP code {0:?} — use a name like \"brightness\" or a number like \"0x10\"")]
    BadCode(String),
    #[error("profile {name:?} writes {code}, which cannot be undone — pass --yes to confirm")]
    DestructiveProfile { name: String, code: String },
    #[error(transparent)]
    Backend(#[from] display_core::Error),
    #[error(transparent)]
    Profile(#[from] display_core::ProfileError),
    #[error(transparent)]
    Rules(#[from] display_core::RulesError),
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// How long to skip DDC reads to a display after one fails.
///
/// A sleeping display makes `IOAVServiceReadI2C` block for 5s+ per attempt, and
/// with retries a single read can wedge the (single) worker for 25s+, starving
/// every other client. After a failure we fast-fail reads to that display for
/// this window instead of re-wedging on every poll. Writes bypass it, so waking
/// the display still works.
const READ_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(15);

pub struct Engine<B: DisplayBackend> {
    backend: B,
    monitors: Vec<Monitor>,
    profiles: ProfileStore,
    /// display id → time its read-cooldown expires.
    read_cooldown: std::collections::HashMap<DisplayId, std::time::Instant>,
}

impl<B: DisplayBackend> Engine<B> {
    pub fn new(backend: B) -> Self {
        Engine::with_profiles(backend, ProfileStore::default_location())
    }

    pub fn with_profiles(backend: B, profiles: ProfileStore) -> Self {
        Engine {
            backend,
            monitors: Vec::new(),
            profiles,
            read_cooldown: std::collections::HashMap::new(),
        }
    }

    pub fn profiles(&self) -> &ProfileStore {
        &self.profiles
    }

    /// Re-enumerate. Required after hot-plug: AV services do not survive it.
    pub fn refresh(&mut self) -> Result<()> {
        self.monitors = self.backend.list()?;
        Ok(())
    }

    /// Re-enumerate if the set of attached displays has changed.
    ///
    /// The daemon is long-lived and caches an I2C channel per display; those
    /// channels do **not** survive hot-plug. Without this check, a dock/undock
    /// would leave every later `get`/`set` talking to a dead service — and
    /// dock/undock is a headline use case. The id comparison costs one
    /// CoreGraphics call, far less than the enumeration it avoids.
    ///
    /// Known gap: if a display is unplugged and replugged between two calls
    /// *and* is assigned the same id, this cannot tell. Closing that needs
    /// `CGDisplayRegisterReconfigurationCallback` (planned) rather than polling.
    fn ensure(&mut self) -> Result<()> {
        if self.monitors.is_empty() {
            return self.refresh();
        }
        let mut online = self.backend.online_ids()?;
        let mut cached: Vec<DisplayId> = self.monitors.iter().map(|m| m.id).collect();
        online.sort();
        cached.sort();
        if online != cached {
            self.refresh()?;
        }
        Ok(())
    }

    pub fn list(&mut self) -> Result<Vec<api::MonitorInfo>> {
        self.refresh()?;
        Ok(self.monitors.iter().map(to_info).collect())
    }

    /// Resolve a selector to display ids.
    ///
    /// Accepts `all`, a numeric id, or a case-insensitive substring of the
    /// vendor/product. An empty selector auto-picks when exactly one display is
    /// controllable, and otherwise refuses rather than guessing — guessing wrong
    /// on an input switch costs the user their picture.
    pub fn resolve(&mut self, selector: &str) -> Result<Vec<DisplayId>> {
        self.ensure()?;
        let controllable: Vec<&Monitor> = self
            .monitors
            .iter()
            .filter(|m| m.is_controllable())
            .collect();

        if controllable.is_empty() {
            return Err(EngineError::NoDisplays);
        }

        let sel = selector.trim();
        if sel.eq_ignore_ascii_case("all") {
            return Ok(controllable.iter().map(|m| m.id).collect());
        }
        if sel.is_empty() {
            return match controllable.as_slice() {
                [only] => Ok(vec![only.id]),
                many => Err(EngineError::NeedsSelector(many.len())),
            };
        }
        if let Ok(n) = sel.parse::<u32>() {
            if let Some(m) = controllable.iter().find(|m| m.id.0 == n) {
                return Ok(vec![m.id]);
            }
        }

        let needle = sel.to_lowercase();
        let hits: Vec<&&Monitor> = controllable
            .iter()
            .filter(|m| {
                m.identity.product_name.to_lowercase().contains(&needle)
                    || m.identity.vendor.to_lowercase().contains(&needle)
            })
            .collect();

        match hits.as_slice() {
            [] => Err(EngineError::NoMatch(selector.to_string())),
            [one] => Ok(vec![one.id]),
            many => Err(EngineError::Ambiguous(
                selector.to_string(),
                many.iter()
                    .map(|m| format!("{} (id {})", m.identity, m.id))
                    .collect::<Vec<_>>()
                    .join(", "),
            )),
        }
    }

    fn single(&mut self, selector: &str) -> Result<DisplayId> {
        let ids = self.resolve(selector)?;
        match ids.as_slice() {
            [one] => Ok(*one),
            many => Err(EngineError::Ambiguous(
                selector.to_string(),
                format!("{} displays", many.len()),
            )),
        }
    }

    pub fn get(&mut self, selector: &str, code_str: &str) -> Result<api::VcpValue> {
        let code =
            vcp::parse_code(code_str).ok_or_else(|| EngineError::BadCode(code_str.into()))?;
        let id = self.single(selector)?;

        // Fast-fail if this display recently wedged a read, rather than blocking
        // the worker on it again.
        if let Some(&until) = self.read_cooldown.get(&id) {
            if std::time::Instant::now() < until {
                return Err(EngineError::Backend(display_core::Error::Transport(
                    "display not responding (recently failed a read; cooling down)".into(),
                )));
            }
        }

        match self.backend.get_vcp(id, code) {
            Ok((current, max)) => {
                self.read_cooldown.remove(&id); // it answered; clear any cooldown
                Ok(api::VcpValue {
                    code: code.code(),
                    name: code.display_name(),
                    current,
                    max,
                    kind: to_kind(code.kind()),
                })
            }
            Err(e) => {
                self.read_cooldown
                    .insert(id, std::time::Instant::now() + READ_COOLDOWN);
                Err(e.into())
            }
        }
    }

    /// Write a value, optionally reading it back to see whether it stuck.
    ///
    /// DDC has no write acknowledgement, so without `verify` a successful return
    /// means only that the bytes were sent. Monitors commonly advertise codes
    /// they ignore.
    pub fn set(
        &mut self,
        selector: &str,
        code_str: &str,
        value: u16,
        verify: bool,
    ) -> Result<api::SetResult> {
        let code =
            vcp::parse_code(code_str).ok_or_else(|| EngineError::BadCode(code_str.into()))?;
        let ids = self.resolve(selector)?;
        let mut ignored = Vec::new();

        for id in &ids {
            self.backend.set_vcp(*id, code, value)?;
            // A write went through, so the display is reachable — let reads try
            // again immediately (e.g. right after "Turn On").
            self.read_cooldown.remove(id);
            if verify {
                // A read failure is not proof the write was ignored — the
                // monitor may simply not answer reads for this code. Only a
                // successful read showing a different value is evidence.
                if let Ok((current, _)) = self.backend.get_vcp(*id, code) {
                    if !value_matches(code.kind(), current, value) {
                        ignored.push(id.0);
                    }
                }
            }
        }
        Ok(api::SetResult {
            displays: ids.len(),
            ignored,
        })
    }

    pub fn caps(&mut self, selector: &str) -> Result<api::CapsResult> {
        let id = self.single(selector)?;
        let raw = self.backend.capability_string(id)?;
        let parsed = raw.as_deref().and_then(|r| caps::parse(r).ok());
        let value_lists = parsed
            .as_ref()
            .map(|c| {
                c.vcp
                    .iter()
                    .filter(|f| !f.values.is_empty())
                    .map(|f| (f.code.code(), f.values.clone()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(api::CapsResult {
            vcp_codes: parsed
                .as_ref()
                .map(|c| c.vcp.iter().map(|f| f.code.code()).collect())
                .unwrap_or_default(),
            mccs_version: parsed.as_ref().and_then(|c| c.mccs_version.clone()),
            unknown_sections: parsed.map(|c| c.unknown_sections).unwrap_or_default(),
            value_lists,
            raw,
        })
    }

    /// Look up a monitor for display purposes. Does not re-enumerate.
    pub fn monitor(&self, id: DisplayId) -> Option<&Monitor> {
        self.monitors.iter().find(|m| m.id == id)
    }

    // ── Profiles ───────────────────────────────────────────────────────────

    pub fn profile_list(&mut self) -> Result<Vec<api::ProfileSummary>> {
        let mut out = Vec::new();
        for name in self.profiles.list()? {
            // A profile that fails to parse is still worth listing; hiding it
            // would make a typo look like the file vanished.
            let displays = self.profiles.load(&name).map(|p| p.displays.len()).ok();
            out.push(api::ProfileSummary { name, displays });
        }
        Ok(out)
    }

    pub fn profile_show(&mut self, name: &str) -> Result<Profile> {
        Ok(self.profiles.load(name)?)
    }

    pub fn profile_delete(&mut self, name: &str) -> Result<()> {
        Ok(self.profiles.delete(name)?)
    }

    /// Snapshot the current state of every controllable display.
    ///
    /// Only records codes that read back successfully — a code the display will
    /// not report is one this profile has no business claiming to restore.
    /// Destructive codes are never captured: a profile that silently switched
    /// inputs on apply would be a trap.
    pub fn profile_save(
        &mut self,
        name: &str,
        codes: &[VcpCode],
        force: bool,
    ) -> Result<api::ProfileSummary> {
        display_core::profile::validate_name(name)?;
        self.refresh()?;

        let mut profile = Profile::new(name)?;
        let monitors: Vec<Monitor> = self
            .monitors
            .iter()
            .filter(|m| m.is_controllable())
            .cloned()
            .collect();

        for m in &monitors {
            let mut settings = BTreeMap::new();
            for &code in codes {
                if code.is_destructive() {
                    continue;
                }
                if let Ok((current, _)) = self.backend.get_vcp(m.id, code) {
                    settings.insert(setting_key(code), current);
                }
            }
            if settings.is_empty() {
                continue;
            }
            profile.displays.push(DisplayProfile {
                selector: selector_for(m, &monitors),
                settings,
            });
        }

        let displays = profile.displays.len();
        self.profiles.save(&profile, force)?;
        Ok(api::ProfileSummary {
            name: name.to_string(),
            displays: Some(displays),
        })
    }

    /// Apply a profile.
    ///
    /// # On "atomic with rollback"
    ///
    /// The plan asks for atomic application with rollback. True atomicity is not
    /// achievable over DDC: writes are unacknowledged, so a write that lands and
    /// a write that vanishes are indistinguishable without reading back, and the
    /// read itself can fail. What this does instead:
    ///
    /// - A **transport error** aborts the apply and restores every value already
    ///   written in this call, best-effort (values that could not be read
    ///   beforehand cannot be restored, and are reported).
    /// - A **silently ignored** write is not an error — the monitor simply does
    ///   not implement that code. It is reported as `ignored` so the caller can
    ///   say so rather than claiming success.
    pub fn profile_apply(
        &mut self,
        name: &str,
        verify: bool,
        yes: bool,
    ) -> Result<api::ApplyResult> {
        let profile = self.profiles.load(name)?;

        // Refuse the whole profile before touching anything, rather than
        // discovering a destructive code halfway through.
        if !yes {
            for d in &profile.displays {
                for key in d.settings.keys() {
                    if let Some(code) = vcp::parse_code(key) {
                        if code.is_destructive() {
                            return Err(EngineError::DestructiveProfile {
                                name: name.to_string(),
                                code: code.display_name(),
                            });
                        }
                    }
                }
            }
        }

        let mut outcomes = Vec::new();
        let mut undo: Vec<(DisplayId, VcpCode, u16)> = Vec::new();

        for d in &profile.displays {
            let ids = match self.resolve(&d.selector) {
                Ok(ids) => ids,
                // A profile naming a display that is not attached is normal
                // (dock/undock), not a failure.
                Err(EngineError::NoMatch(_)) | Err(EngineError::NoDisplays) => {
                    outcomes.push(api::ApplyOutcome {
                        display: None,
                        selector: d.selector.clone(),
                        code: 0,
                        name: String::new(),
                        value: 0,
                        status: api::ApplyStatus::NotConnected,
                    });
                    continue;
                }
                Err(e) => return Err(e),
            };

            for id in ids {
                for (key, &value) in &d.settings {
                    let Some(code) = vcp::parse_code(key) else {
                        return Err(EngineError::BadCode(key.clone()));
                    };

                    let previous = self.backend.get_vcp(id, code).ok().map(|(c, _)| c);

                    if let Err(e) = self.backend.set_vcp(id, code, value) {
                        // Transport failure: undo what this call changed.
                        let restored = self.rollback(&undo);
                        return Err(EngineError::Backend(display_core::Error::Transport(
                            format!(
                                "{e} — applying {} to display {id}; rolled back {restored} \
                                 of {} earlier change(s)",
                                code.display_name(),
                                undo.len()
                            ),
                        )));
                    }
                    if let Some(prev) = previous {
                        undo.push((id, code, prev));
                    }

                    let status = if !verify {
                        api::ApplyStatus::Unverified
                    } else {
                        match self.backend.get_vcp(id, code) {
                            Ok((current, _)) if value_matches(code.kind(), current, value) => {
                                api::ApplyStatus::Applied
                            }
                            Ok(_) => api::ApplyStatus::Ignored,
                            // Cannot read it back; absence of evidence.
                            Err(_) => api::ApplyStatus::Unverified,
                        }
                    };

                    outcomes.push(api::ApplyOutcome {
                        display: Some(id.0),
                        selector: d.selector.clone(),
                        code: code.code(),
                        name: code.display_name(),
                        value,
                        status,
                    });
                }
            }
        }

        Ok(api::ApplyResult {
            profile: name.to_string(),
            outcomes,
        })
    }

    // ── Automation ─────────────────────────────────────────────────────────

    /// Names attached displays for rule matching: product name plus id, so a
    /// selector can use either.
    pub fn display_names(&mut self) -> Result<Vec<String>> {
        self.ensure()?;
        Ok(self
            .monitors
            .iter()
            .flat_map(|m| {
                let mut names = vec![m.id.to_string()];
                if !m.identity.product_name.is_empty() {
                    names.push(format!("{} {}", m.identity.vendor, m.identity.product_name));
                }
                names
            })
            .collect())
    }

    pub fn power_source(&mut self) -> Result<display_core::PowerState> {
        Ok(self.backend.power_source()?)
    }

    /// Run a rule's action.
    ///
    /// `force` comes from the rule, not from a global: a rule that switches
    /// inputs must opt in explicitly, because it fires unattended and a wrong
    /// input leaves the user with no picture to fix it with.
    pub fn execute(&mut self, rule: &display_core::Rule) -> Result<String> {
        match &rule.action {
            display_core::Action::Profile(name) => {
                let r = self.profile_apply(name, true, rule.force)?;
                let ignored = r.count(api::ApplyStatus::Ignored);
                Ok(format!(
                    "applied profile {name:?}: {} confirmed, {ignored} ignored",
                    r.count(api::ApplyStatus::Applied)
                ))
            }
            display_core::Action::Set {
                display,
                code,
                value,
            } => {
                if let Some(c) = vcp::parse_code(code) {
                    if c.is_destructive() && !rule.force {
                        return Err(EngineError::DestructiveProfile {
                            name: rule.name.clone(),
                            code: c.display_name(),
                        });
                    }
                }
                let r = self.set(display, code, *value, true)?;
                Ok(format!(
                    "set {code}={value} on {} display(s){}",
                    r.displays,
                    if r.ignored.is_empty() {
                        String::new()
                    } else {
                        format!(" ({} ignored it)", r.ignored.len())
                    }
                ))
            }
            display_core::Action::Run(cmd) => run_command(cmd),
        }
    }

    /// Best-effort restore. Returns how many were put back.
    fn rollback(&mut self, undo: &[(DisplayId, VcpCode, u16)]) -> usize {
        let mut restored = 0;
        for (id, code, prev) in undo.iter().rev() {
            if self.backend.set_vcp(*id, *code, *prev).is_ok() {
                restored += 1;
            }
        }
        restored
    }
}

/// Run a rule's shell command.
///
/// Runs detached from the caller's stdio and is **not** waited on beyond a short
/// bound: a rule that launches a long-running program must not wedge the I2C
/// worker, which is single-threaded and shared by every other request.
fn run_command(cmd: &str) -> Result<String> {
    use std::process::{Command, Stdio};
    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            EngineError::Backend(display_core::Error::Transport(format!(
                "cannot run {cmd:?}: {e}"
            )))
        })?;
    Ok(format!("spawned {cmd:?} (pid {})", child.id()))
}

/// How a code is written into a profile file: a name when we have one, so the
/// TOML stays readable, and a hex number otherwise.
fn setting_key(code: VcpCode) -> String {
    match code {
        VcpCode::Raw(c) => format!("0x{c:02X}"),
        _ => code.name().to_lowercase().replace(' ', "-"),
    }
}

/// Pick a selector that survives being applied on another machine.
///
/// Prefers the product name — display ids are assigned per-boot and would make a
/// profile machine-specific — but falls back to the id when the name is absent
/// or shared with another attached display.
fn selector_for(m: &Monitor, all: &[Monitor]) -> String {
    let name = &m.identity.product_name;
    if name.is_empty() {
        return m.id.to_string();
    }
    let unique = all
        .iter()
        .filter(|o| o.identity.product_name.eq_ignore_ascii_case(name))
        .count()
        == 1;
    if unique {
        name.clone()
    } else {
        m.id.to_string()
    }
}

/// Whether a read-back value corresponds to what was written.
///
/// Non-continuous codes are compared on the low byte only: MCCS puts the value
/// in the SL byte, and monitors put arbitrary things in SH. The dev-bench
/// MB169CK answers Mute with `0x0202` where the value is `0x02`, so a strict
/// u16 comparison would call a successful write a failure.
fn value_matches(kind: ValueKind, current: u16, written: u16) -> bool {
    match kind {
        ValueKind::NonContinuous => current & 0xFF == written & 0xFF,
        _ => current == written,
    }
}

fn to_kind(k: ValueKind) -> api::ValueKind {
    match k {
        ValueKind::Continuous => api::ValueKind::Continuous,
        ValueKind::NonContinuous => api::ValueKind::NonContinuous,
        ValueKind::Unknown => api::ValueKind::Unknown,
    }
}

fn to_info(m: &Monitor) -> api::MonitorInfo {
    api::MonitorInfo {
        id: m.id.0,
        vendor: m.identity.vendor.clone(),
        product: m.identity.product_name.clone(),
        serial: m.identity.serial,
        alphanumeric_serial: m.identity.alphanumeric_serial.clone(),
        serial_trustworthy: m.identity.has_trustworthy_serial(),
        location: m.identity.location.clone(),
        control: match m.control {
            ControlPath::Ddc => api::ControlPath::Ddc,
            ControlPath::Native => api::ControlPath::Native,
            ControlPath::None => api::ControlPath::None,
        },
        key: m.identity.settings_key(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use display_core::{DisplayIdentity, Error as CoreError, Result as CoreResult};
    use display_ddc::vcp::VcpCode;
    use std::collections::HashMap;

    /// Scripted backend, including misbehaving personalities. Lets the protocol
    /// and selector logic be tested with no hardware present.
    #[derive(Default)]
    struct MockBackend {
        monitors: Vec<Monitor>,
        values: HashMap<(u32, u8), (u16, u16)>,
        caps: HashMap<u32, String>,
        /// Displays that fail every transaction, like a monitor asleep.
        dead: Vec<u32>,
        /// Codes the display accepts writes for but silently ignores — the
        /// dominant real-world behaviour, not an edge case. The MB169CK
        /// advertises 32 codes and honours 2.
        ignores: Vec<u8>,
        pub sets: Vec<(u32, u8, u16)>,
        pub list_calls: usize,
        pub online_calls: usize,
    }

    impl MockBackend {
        fn with(monitors: Vec<Monitor>) -> Self {
            MockBackend {
                monitors,
                ..Default::default()
            }
        }
    }

    impl DisplayBackend for MockBackend {
        fn online_ids(&mut self) -> CoreResult<Vec<DisplayId>> {
            self.online_calls += 1;
            Ok(self.monitors.iter().map(|m| m.id).collect())
        }
        fn list(&mut self) -> CoreResult<Vec<Monitor>> {
            self.list_calls += 1;
            Ok(self.monitors.clone())
        }
        fn get_vcp(&mut self, id: DisplayId, code: VcpCode) -> CoreResult<(u16, u16)> {
            if self.dead.contains(&id.0) {
                return Err(CoreError::Transport("no response".into()));
            }
            self.values
                .get(&(id.0, code.code()))
                .copied()
                .ok_or(CoreError::Unsupported(code))
        }
        fn set_vcp(&mut self, id: DisplayId, code: VcpCode, value: u16) -> CoreResult<()> {
            if self.dead.contains(&id.0) {
                return Err(CoreError::Transport("no response".into()));
            }
            self.sets.push((id.0, code.code(), value));
            // DDC has no write ack, so an ignored write still "succeeds".
            if !self.ignores.contains(&code.code()) {
                self.values.insert((id.0, code.code()), (value, 100));
            }
            Ok(())
        }
        fn capability_string(&mut self, id: DisplayId) -> CoreResult<Option<String>> {
            Ok(self.caps.get(&id.0).cloned())
        }
    }

    fn mon(id: u32, vendor: &str, product: &str, control: ControlPath) -> Monitor {
        Monitor {
            id: DisplayId(id),
            identity: DisplayIdentity {
                vendor: vendor.into(),
                product_name: product.into(),
                serial: 0x1A2B_3C4D,
                location: format!("/port@{id}"),
                ..Default::default()
            },
            control,
            capabilities: None,
        }
    }

    fn engine(monitors: Vec<Monitor>) -> Engine<MockBackend> {
        Engine::new(MockBackend::with(monitors))
    }

    /// Engine with a throwaway profile directory, so tests never touch the
    /// user's real profiles.
    fn engine_with_profiles(monitors: Vec<Monitor>) -> (tempfile::TempDir, Engine<MockBackend>) {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(dir.path());
        (
            dir,
            Engine::with_profiles(MockBackend::with(monitors), store),
        )
    }

    #[test]
    fn empty_selector_auto_picks_the_only_display() {
        let mut e = engine(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        assert_eq!(e.resolve("").unwrap(), vec![DisplayId(1)]);
    }

    /// Must refuse rather than pick one: a wrong guess on an input switch costs
    /// the user their picture.
    #[test]
    fn empty_selector_refuses_when_ambiguous() {
        let mut e = engine(vec![
            mon(1, "AUS", "MB169CK", ControlPath::Ddc),
            mon(2, "DEL", "U2720Q", ControlPath::Ddc),
        ]);
        assert!(matches!(e.resolve(""), Err(EngineError::NeedsSelector(2))));
    }

    #[test]
    fn resolves_by_id_and_by_name_substring() {
        let mut e = engine(vec![
            mon(1, "AUS", "MB169CK", ControlPath::Ddc),
            mon(3, "DEL", "U2720Q", ControlPath::Ddc),
        ]);
        assert_eq!(e.resolve("3").unwrap(), vec![DisplayId(3)]);
        assert_eq!(e.resolve("u2720q").unwrap(), vec![DisplayId(3)]);
        assert_eq!(e.resolve("DEL").unwrap(), vec![DisplayId(3)]);
    }

    #[test]
    fn all_selects_every_controllable_display() {
        let mut e = engine(vec![
            mon(1, "AUS", "MB169CK", ControlPath::Ddc),
            mon(2, "APP", "Built-in", ControlPath::Native),
            mon(9, "XXX", "Dummy", ControlPath::None),
        ]);
        let ids = e.resolve("all").unwrap();
        assert_eq!(ids, vec![DisplayId(1), DisplayId(2)]);
        assert!(
            !ids.contains(&DisplayId(9)),
            "uncontrollable must be excluded"
        );
    }

    #[test]
    fn ambiguous_name_is_an_error_listing_candidates() {
        let mut e = engine(vec![
            mon(1, "DEL", "U2720Q", ControlPath::Ddc),
            mon(2, "DEL", "U2723QE", ControlPath::Ddc),
        ]);
        let err = e.resolve("DEL").unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "{err}");
        assert!(err.contains("id 1") && err.contains("id 2"), "{err}");
    }

    #[test]
    fn unmatched_selector_reports_no_match() {
        let mut e = engine(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        assert!(matches!(e.resolve("nope"), Err(EngineError::NoMatch(_))));
    }

    #[test]
    fn bad_vcp_code_is_rejected_before_touching_hardware() {
        let mut e = engine(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        assert!(matches!(
            e.get("", "not-a-code"),
            Err(EngineError::BadCode(_))
        ));
        assert!(e.backend.sets.is_empty());
    }

    #[test]
    fn set_all_writes_to_every_display() {
        let mut e = engine(vec![
            mon(1, "AUS", "MB169CK", ControlPath::Ddc),
            mon(2, "DEL", "U2720Q", ControlPath::Ddc),
        ]);
        let r = e.set("all", "brightness", 42, false).unwrap();
        assert_eq!(r.displays, 2);
        assert_eq!(e.backend.sets, vec![(1, 0x10, 42), (2, 0x10, 42)]);
    }

    /// The behaviour that motivated `verify`: the monitor accepts the write,
    /// reports success, and changes nothing.
    #[test]
    fn verify_detects_a_silently_ignored_write() {
        let mut e = engine(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        e.backend.ignores.push(0x16); // red gain, as the real MB169CK does
        e.backend.values.insert((1, 0x16), (100, 100));

        let r = e.set("", "0x16", 90, true).unwrap();
        assert_eq!(r.displays, 1);
        assert_eq!(r.ignored, vec![1], "ignored write must be reported");
    }

    #[test]
    fn verify_passes_when_the_write_lands() {
        let mut e = engine(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        let r = e.set("", "brightness", 42, true).unwrap();
        assert!(r.ignored.is_empty());
    }

    /// Without verify we cannot know, so `ignored` must stay empty rather than
    /// implying the write was confirmed.
    #[test]
    fn without_verify_ignored_is_empty_even_for_a_lying_display() {
        let mut e = engine(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        e.backend.ignores.push(0x10);
        let r = e.set("", "brightness", 42, false).unwrap();
        assert!(r.ignored.is_empty());
    }

    /// Non-continuous read-backs carry junk in the high byte; comparing the
    /// full u16 would report a successful write as ignored. Real case: the
    /// MB169CK answers Mute with 0x0202.
    #[test]
    fn verify_compares_non_continuous_values_on_the_low_byte() {
        assert!(value_matches(ValueKind::NonContinuous, 0x0202, 0x02));
        assert!(!value_matches(ValueKind::NonContinuous, 0x0203, 0x02));
        // Continuous values must still compare exactly.
        assert!(!value_matches(ValueKind::Continuous, 0x0202, 0x02));
        assert!(value_matches(ValueKind::Continuous, 42, 42));
    }

    /// A monitor that ignores reads for a code must not be accused of ignoring
    /// the write — absence of evidence is not evidence.
    #[test]
    fn unreadable_code_is_not_reported_as_ignored() {
        let mut e = engine(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        e.backend.ignores.push(0x87);
        // No entry in `values`, so get_vcp errors with Unsupported.
        let r = e.set("", "0x87", 40, true).unwrap();
        assert!(r.ignored.is_empty());
    }

    /// A display that NACKs everything must surface an error, not report zeros.
    #[test]
    fn unresponsive_display_surfaces_a_transport_error() {
        let mut e = engine(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        e.backend.dead.push(1);
        assert!(matches!(
            e.get("", "brightness"),
            Err(EngineError::Backend(CoreError::Transport(_)))
        ));
    }

    #[test]
    fn get_reports_kind_so_clients_do_not_invent_a_range() {
        let mut e = engine(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        e.backend.values.insert((1, 0x60), (0x1A, 3));
        let v = e.get("", "input").unwrap();
        assert_eq!(v.kind, api::ValueKind::NonContinuous);
        assert_eq!(v.current, 0x1A);
    }

    #[test]
    fn no_controllable_displays_is_distinct_from_no_match() {
        let mut e = engine(vec![mon(9, "XXX", "Dummy", ControlPath::None)]);
        assert!(matches!(e.resolve("all"), Err(EngineError::NoDisplays)));
    }

    #[test]
    fn caps_reports_parsed_codes_and_survives_absent_string() {
        let mut e = engine(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        e.backend
            .caps
            .insert(1, "(prot(monitor)vcp(10 12 60))".into());
        let c = e.caps("").unwrap();
        assert_eq!(c.vcp_codes, vec![0x10, 0x12, 0x60]);

        let mut e2 = engine(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        let c2 = e2.caps("").unwrap();
        assert!(c2.raw.is_none());
        assert!(c2.vcp_codes.is_empty());
    }

    /// Garbage capability strings must not fail the call — the monitor still works.
    #[test]
    fn unparseable_caps_string_still_returns_the_raw_text() {
        let mut e = engine(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        e.backend.caps.insert(1, "total garbage".into());
        let c = e.caps("").unwrap();
        assert_eq!(c.raw.as_deref(), Some("total garbage"));
        assert!(c.vcp_codes.is_empty());
    }

    /// Hot-plug invalidates cached I2C channels. The engine must notice a
    /// changed display set rather than driving a dead service.
    #[test]
    fn hot_plug_forces_re_enumeration() {
        let mut e = engine(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        e.resolve("").unwrap();
        let after_first = e.backend.list_calls;

        // A second display appears.
        e.backend
            .monitors
            .push(mon(2, "DEL", "U2720Q", ControlPath::Ddc));
        e.resolve("all").unwrap();
        assert!(
            e.backend.list_calls > after_first,
            "changed display set must trigger re-enumeration"
        );
        assert_eq!(e.resolve("all").unwrap().len(), 2);
    }

    #[test]
    fn unplug_forces_re_enumeration() {
        let mut e = engine(vec![
            mon(1, "AUS", "MB169CK", ControlPath::Ddc),
            mon(2, "DEL", "U2720Q", ControlPath::Ddc),
        ]);
        assert_eq!(e.resolve("all").unwrap().len(), 2);
        e.backend.monitors.retain(|m| m.id.0 != 2);
        assert_eq!(e.resolve("all").unwrap(), vec![DisplayId(1)]);
    }

    /// The steady-state path must stay cheap: no enumeration when nothing moved.
    #[test]
    fn unchanged_display_set_does_not_re_enumerate() {
        let mut e = engine(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        e.resolve("").unwrap();
        let baseline = e.backend.list_calls;
        for _ in 0..5 {
            e.resolve("").unwrap();
        }
        assert_eq!(e.backend.list_calls, baseline, "should not re-enumerate");
        assert!(e.backend.online_calls >= 5, "but should check cheaply");
    }

    // ── Profiles ───────────────────────────────────────────────────────────

    #[test]
    fn save_then_apply_round_trips() {
        let (_d, mut e) = engine_with_profiles(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        e.backend.values.insert((1, 0x10), (60, 100));
        e.backend.values.insert((1, 0x12), (75, 100));

        let s = e
            .profile_save("coding", &[VcpCode::Brightness, VcpCode::Contrast], false)
            .unwrap();
        assert_eq!(s.displays, Some(1));

        e.backend.values.insert((1, 0x10), (10, 100));
        let r = e.profile_apply("coding", true, false).unwrap();
        assert_eq!(r.count(api::ApplyStatus::Applied), 2);
        assert_eq!(e.backend.values[&(1, 0x10)].0, 60);
    }

    /// A profile must be portable between machines, where display ids differ.
    #[test]
    fn save_uses_product_name_as_selector_when_unique() {
        let (_d, mut e) = engine_with_profiles(vec![mon(7, "AUS", "MB169CK", ControlPath::Ddc)]);
        e.backend.values.insert((7, 0x10), (60, 100));
        e.profile_save("p", &[VcpCode::Brightness], false).unwrap();
        assert_eq!(e.profile_show("p").unwrap().displays[0].selector, "MB169CK");
    }

    /// Two identical models cannot be told apart by name, so fall back to ids
    /// rather than writing a profile that applies to the wrong one.
    #[test]
    fn save_falls_back_to_id_for_duplicate_models() {
        let (_d, mut e) = engine_with_profiles(vec![
            mon(1, "DEL", "U2720Q", ControlPath::Ddc),
            mon(2, "DEL", "U2720Q", ControlPath::Ddc),
        ]);
        e.backend.values.insert((1, 0x10), (60, 100));
        e.backend.values.insert((2, 0x10), (60, 100));
        e.profile_save("p", &[VcpCode::Brightness], false).unwrap();
        let p = e.profile_show("p").unwrap();
        let selectors: Vec<&str> = p.displays.iter().map(|d| d.selector.as_str()).collect();
        assert_eq!(selectors, vec!["1", "2"]);
    }

    /// Only codes the display actually reports get captured.
    #[test]
    fn save_skips_unreadable_codes() {
        let (_d, mut e) = engine_with_profiles(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        e.backend.values.insert((1, 0x10), (60, 100));
        // 0x12 absent from `values`, so reads fail.
        e.profile_save("p", &[VcpCode::Brightness, VcpCode::Contrast], false)
            .unwrap();
        let p = e.profile_show("p").unwrap();
        assert_eq!(p.displays[0].settings.len(), 1);
        assert!(p.displays[0].settings.contains_key("brightness"));
    }

    /// A profile that silently switched inputs on apply would be a trap.
    #[test]
    fn save_never_captures_destructive_codes() {
        let (_d, mut e) = engine_with_profiles(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        e.backend.values.insert((1, 0x60), (0x11, 3));
        e.backend.values.insert((1, 0x10), (60, 100));
        e.profile_save("p", &[VcpCode::Brightness, VcpCode::InputSource], false)
            .unwrap();
        let p = e.profile_show("p").unwrap();
        assert!(!p.displays[0].settings.contains_key("input-source"));
    }

    /// Note the profile values must differ from what the display currently
    /// holds: writing a value a display already has is indistinguishable from
    /// an ignored write, because the read-back matches either way.
    #[test]
    fn apply_reports_settings_the_display_ignores() {
        let (d, mut e) = engine_with_profiles(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        e.backend.values.insert((1, 0x10), (5, 100));
        e.backend.values.insert((1, 0x16), (100, 100));
        e.backend.ignores.push(0x16); // as the real MB169CK does

        let p = Profile {
            name: "p".into(),
            displays: vec![DisplayProfile {
                selector: "MB169CK".into(),
                settings: BTreeMap::from([
                    ("brightness".to_string(), 60u16),
                    ("red-gain".to_string(), 90u16),
                ]),
            }],
        };
        ProfileStore::new(d.path()).save(&p, true).unwrap();

        let r = e.profile_apply("p", true, false).unwrap();
        assert_eq!(
            r.count(api::ApplyStatus::Applied),
            1,
            "brightness should land"
        );
        assert_eq!(r.count(api::ApplyStatus::Ignored), 1, "red gain is ignored");
    }

    /// Without verification we cannot claim anything was applied.
    #[test]
    fn apply_without_verify_reports_unverified_not_applied() {
        let (_d, mut e) = engine_with_profiles(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        e.backend.values.insert((1, 0x10), (60, 100));
        e.profile_save("p", &[VcpCode::Brightness], false).unwrap();

        let r = e.profile_apply("p", false, false).unwrap();
        assert_eq!(r.count(api::ApplyStatus::Unverified), 1);
        assert_eq!(r.count(api::ApplyStatus::Applied), 0);
    }

    /// Dock/undock: a profile covering absent displays must still apply to the
    /// ones that are present.
    #[test]
    fn apply_tolerates_displays_that_are_not_connected() {
        let (d, mut e) = engine_with_profiles(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        e.backend.values.insert((1, 0x10), (60, 100));
        e.profile_save("p", &[VcpCode::Brightness], false).unwrap();

        // Hand-edit in a display that isn't attached.
        let mut p = e.profile_show("p").unwrap();
        p.displays.push(DisplayProfile {
            selector: "U2720Q".into(),
            settings: BTreeMap::from([("brightness".to_string(), 30u16)]),
        });
        ProfileStore::new(d.path()).save(&p, true).unwrap();

        let r = e.profile_apply("p", true, false).unwrap();
        assert_eq!(r.count(api::ApplyStatus::NotConnected), 1);
        assert_eq!(r.count(api::ApplyStatus::Applied), 1);
    }

    /// A hand-edited profile containing an input switch must not fire silently.
    #[test]
    fn apply_refuses_destructive_profile_without_confirmation() {
        let (d, mut e) = engine_with_profiles(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        let p = Profile {
            name: "risky".into(),
            displays: vec![DisplayProfile {
                selector: "MB169CK".into(),
                settings: BTreeMap::from([("0x60".to_string(), 0x11u16)]),
            }],
        };
        ProfileStore::new(d.path()).save(&p, true).unwrap();

        assert!(matches!(
            e.profile_apply("risky", true, false),
            Err(EngineError::DestructiveProfile { .. })
        ));
        // Nothing was written.
        assert!(e.backend.sets.is_empty());

        e.backend.values.insert((1, 0x60), (0x1A, 3));
        assert!(e.profile_apply("risky", true, true).is_ok());
    }

    /// A transport failure part-way through must restore what it changed.
    #[test]
    fn apply_rolls_back_on_transport_failure() {
        let (d, mut e) = engine_with_profiles(vec![
            mon(1, "AUS", "MB169CK", ControlPath::Ddc),
            mon(2, "DEL", "U2720Q", ControlPath::Ddc),
        ]);
        e.backend.values.insert((1, 0x10), (60, 100));
        e.backend.values.insert((2, 0x10), (70, 100));

        let p = Profile {
            name: "two".into(),
            displays: vec![
                DisplayProfile {
                    selector: "MB169CK".into(),
                    settings: BTreeMap::from([("brightness".to_string(), 10u16)]),
                },
                DisplayProfile {
                    selector: "U2720Q".into(),
                    settings: BTreeMap::from([("brightness".to_string(), 20u16)]),
                },
            ],
        };
        ProfileStore::new(d.path()).save(&p, true).unwrap();

        // The second display dies mid-apply.
        e.refresh().unwrap();
        e.backend.dead.push(2);

        assert!(e.profile_apply("two", false, false).is_err());
        // The first display's change must have been undone.
        assert_eq!(
            e.backend.values[&(1, 0x10)].0,
            60,
            "display 1 should have been rolled back to its prior value"
        );
    }

    #[test]
    fn profile_list_reports_unparseable_profiles_rather_than_hiding_them() {
        let (d, mut e) = engine_with_profiles(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        std::fs::write(d.path().join("broken.toml"), "not = = toml").unwrap();
        let list = e.profile_list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "broken");
        assert!(list[0].displays.is_none());
    }

    #[test]
    fn profile_names_are_validated_before_hitting_the_filesystem() {
        let (_d, mut e) = engine_with_profiles(vec![mon(1, "AUS", "MB169CK", ControlPath::Ddc)]);
        assert!(e
            .profile_save("../escape", &[VcpCode::Brightness], false)
            .is_err());
        assert!(e.profile_apply("../escape", false, false).is_err());
    }

    #[test]
    fn info_flags_placeholder_serials() {
        let mut m = mon(1, "AUS", "MB169CK", ControlPath::Ddc);
        m.identity.serial = 0x0101_0101;
        let mut e = engine(vec![m]);
        let list = e.list().unwrap();
        assert!(!list[0].serial_trustworthy);
    }
}
