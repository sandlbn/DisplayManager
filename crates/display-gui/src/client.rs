//! Background daemon worker.
//!
//! # Why this is not called inline
//!
//! The menu is built on the main thread. If the main thread makes a blocking
//! socket call and the daemon's single I2C worker is wedged on a half-dead
//! display (which happens during hot-plug — a monitor mid-connect can make
//! `IOAVServiceReadI2C` block for seconds), the entire app freezes, including
//! the ability to open the menu to quit.
//!
//! So all daemon I/O lives on a dedicated thread here. The UI reads a cached
//! [`Snapshot`] (a lock, never a socket) and posts actions over a channel. A
//! wedged daemon slows the *worker*, never the UI — the menu still opens on
//! last-known state and Quit still works.

use display_api::protocol as api;
use display_api::Client;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One slider's worth of state.
#[derive(Clone)]
pub struct SliderView {
    pub label: String,
    pub code: u8,
    pub current: u16,
    pub max: u16,
}

/// An enumerated control offered as a submenu (Input, Color Preset, …).
#[derive(Clone)]
pub struct PickerView {
    pub code: u8,
    pub title: String,
    /// (value, friendly name) for each advertised choice.
    pub values: Vec<(u8, String)>,
    /// Currently-selected value, if readable — checkmarked in the menu.
    pub current: Option<u8>,
}

#[derive(Clone)]
pub struct MonitorView {
    pub id: u32,
    pub name: String,
    /// Stable settings key, for persisting per-monitor menu preferences.
    pub key: String,
    pub is_ddc: bool,
    /// Visible sliders — the worker already applies the customization; the menu
    /// shows these as-is.
    pub sliders: Vec<SliderView>,
    /// Enumerated pickers (Input, Color Preset, Orientation, …) the display
    /// advertises with more than one choice.
    pub pickers: Vec<PickerView>,
}

/// A monitor's advertised capabilities, fetched on demand.
#[derive(Clone)]
pub struct CapsView {
    pub mccs: Option<String>,
    /// (code, friendly name) for each advertised VCP code.
    pub codes: Vec<(u8, String)>,
    /// Advertised value lists for enumerated codes, as (code, values). Used to
    /// build pickers.
    pub value_lists: Vec<(u8, Vec<u8>)>,
    pub unknown_sections: Vec<String>,
}

/// Everything the menu and settings window need, refreshed off the main thread.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub daemon_up: bool,
    pub monitors: Vec<MonitorView>,
    pub profiles: Vec<String>,
    /// Capabilities keyed by display id, populated on demand ("Fetch").
    pub caps: std::collections::HashMap<u32, CapsView>,
    /// Bumped on every change, so the settings window can rebuild only when
    /// something it shows actually changed rather than on a timer blindly.
    pub generation: u64,
}

impl Snapshot {
    fn bump(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

/// A UI action posted to the worker. All fire-and-forget from the UI's side.
pub enum Cmd {
    Set {
        display: String,
        code: u8,
        value: u16,
    },
    ApplyProfile(String),
    DeleteProfile(String),
    /// Snapshot current settings into a new (or overwritten) profile.
    SaveProfile(String),
    /// Read a display's capability string (chunked, slow) and cache it.
    FetchCaps(String),
    /// Force a snapshot refresh (e.g. just after the menu opens).
    Refresh,
}

/// Handle held by the UI: post commands, read the latest snapshot.
pub struct Worker {
    tx: Sender<Cmd>,
    snapshot: Arc<Mutex<Snapshot>>,
    /// Menu preferences, shared with the worker so it knows which codes to read.
    config: Arc<Mutex<crate::config::GuiConfig>>,
}

impl Worker {
    pub fn spawn() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let config = Arc::new(Mutex::new(crate::config::GuiConfig::load()));
        let snap_for_thread = Arc::clone(&snapshot);
        let cfg_for_thread = Arc::clone(&config);

        std::thread::Builder::new()
            .name("display-gui-daemon".into())
            .spawn(move || worker_loop(rx, snap_for_thread, cfg_for_thread))
            .expect("spawn daemon worker");

        Worker {
            tx,
            snapshot,
            config,
        }
    }

    /// Shared config handle, for the settings window to read and mutate.
    pub fn config(&self) -> Arc<Mutex<crate::config::GuiConfig>> {
        Arc::clone(&self.config)
    }

    /// Current cached state. Cheap; never touches the socket.
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().unwrap().clone()
    }

    /// Optimistically drop a profile from the cached list, so the settings
    /// window reflects a delete immediately rather than after the worker's next
    /// refresh. The real delete (posted separately) confirms it.
    pub fn forget_profile(&self, name: &str) {
        let mut s = self.snapshot.lock().unwrap();
        s.profiles.retain(|p| p != name);
        s.bump();
    }

    /// Optimistically add a profile name to the cached list.
    pub fn remember_profile(&self, name: &str) {
        let mut s = self.snapshot.lock().unwrap();
        if !s.profiles.iter().any(|p| p == name) {
            s.profiles.push(name.to_string());
            s.profiles.sort();
        }
        s.bump();
    }

    /// Current change counter — the settings window rebuilds when this moves.
    pub fn generation(&self) -> u64 {
        self.snapshot.lock().unwrap().generation
    }

    /// Post a command. Non-blocking; a dead worker is ignored (the app is
    /// quitting anyway).
    pub fn post(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }
}

/// Idle poll interval. A refresh reads every visible value over DDC, so this is
/// kept modest — the menu also posts an explicit `Refresh` when it opens, so
/// live values are current exactly when they are looked at.
const POLL_INTERVAL: Duration = Duration::from_millis(3000);

type Config = Arc<Mutex<crate::config::GuiConfig>>;

fn worker_loop(rx: Receiver<Cmd>, snapshot: Arc<Mutex<Snapshot>>, config: Config) {
    let mut client: Option<Client> = None;
    refresh(&mut client, &snapshot, &config);

    loop {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(first) => {
                // Drain everything already queued and process it as one batch.
                // A continuous slider drag posts dozens of Sets; without this
                // each would be a separate DDC write and the worker would fall
                // seconds behind the user's finger.
                let mut batch = vec![first];
                while let Ok(c) = rx.try_recv() {
                    batch.push(c);
                }
                if process_batch(batch, &mut client, &snapshot, &config).disconnected {
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                refresh(&mut client, &snapshot, &config)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

struct BatchOutcome {
    disconnected: bool,
}

/// Apply a batch of commands, coalescing Sets so only the final value per
/// control is written. Refreshes only when something other than a Set happened
/// (profile apply/delete, or an explicit Refresh) — a Set needs no read-back
/// because the slider already shows the value the user chose.
fn process_batch(
    batch: Vec<Cmd>,
    client: &mut Option<Client>,
    snapshot: &Arc<Mutex<Snapshot>>,
    config: &Config,
) -> BatchOutcome {
    // Latest value per (display, code), preserving first-seen order.
    let mut sets: Vec<(String, u8, u16)> = Vec::new();
    let mut needs_refresh = false;

    for cmd in batch {
        match cmd {
            Cmd::Set {
                display,
                code,
                value,
            } => {
                if let Some(e) = sets
                    .iter_mut()
                    .find(|(d, c, _)| d == &display && *c == code)
                {
                    e.2 = value; // coalesce: last write wins
                } else {
                    sets.push((display, code, value));
                }
            }
            Cmd::ApplyProfile(name) => {
                if let Some(c) = ensure(client) {
                    let _ = c.profile_apply(&name, false, false);
                }
                needs_refresh = true;
            }
            Cmd::DeleteProfile(name) => {
                if let Some(c) = ensure(client) {
                    let _ = c.profile_delete(&name);
                }
                needs_refresh = true;
            }
            Cmd::SaveProfile(name) => {
                if let Some(c) = ensure(client) {
                    let _ = c.profile_save(&name, &[], true);
                }
                needs_refresh = true;
            }
            Cmd::FetchCaps(display) => {
                if let Some(c) = ensure(client) {
                    if let Ok(caps) = c.caps(&display) {
                        let view = to_caps_view(caps);
                        if let Ok(id) = display.parse::<u32>() {
                            let mut s = snapshot.lock().unwrap();
                            s.caps.insert(id, view);
                            s.bump();
                        }
                    }
                }
            }
            Cmd::Refresh => needs_refresh = true,
        }
    }

    for (display, code, value) in sets {
        if let Some(c) = ensure(client) {
            let _ = c.set(&display, &format!("0x{code:02X}"), value, false);
        }
    }

    if needs_refresh {
        refresh(client, snapshot, config);
    }
    BatchOutcome {
        disconnected: false,
    }
}

/// Connect if needed; returns None if the daemon is unreachable.
fn ensure(client: &mut Option<Client>) -> Option<&mut Client> {
    if client.is_none() {
        match Client::connect(&display_api::socket_path()) {
            Ok(c) => *client = Some(c),
            Err(_) => {
                // Daemon not up — try to start it, then connect once more. Rate
                // limited inside spawn_daemon so a persistent failure does not
                // fork-bomb.
                spawn_daemon();
                *client = Client::connect(&display_api::socket_path()).ok();
            }
        }
    }
    client.as_mut()
}

/// Spawn `displayd` if we can find it, at most once every few seconds.
///
/// Looks beside this executable first — in the app bundle displayd lives in
/// `Contents/Helpers/` next to the GUI's `Contents/MacOS/`, and in a dev build
/// both binaries sit in the same target dir — then falls back to `PATH`. This
/// is what lets the app be self-contained: launching the menu bar app brings the
/// daemon up with no separate install step.
fn spawn_daemon() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 3 {
        return; // spawned recently; give it a moment to come up
    }
    LAST.store(now, Ordering::Relaxed);

    for path in daemon_candidates() {
        if path.exists() {
            let ok = std::process::Command::new(&path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .is_ok();
            if ok {
                // Give the daemon a beat to bind its socket before we retry.
                std::thread::sleep(Duration::from_millis(600));
                return;
            }
        }
    }
}

fn daemon_candidates() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // Dev build: sibling in the same target dir.
            out.push(dir.join("displayd"));
            // App bundle: Contents/MacOS/display-gui -> Contents/Helpers/displayd.
            if let Some(contents) = dir.parent() {
                out.push(contents.join("Helpers").join("displayd"));
            }
        }
    }
    out.push(std::path::PathBuf::from("displayd")); // PATH fallback
    out
}

/// Rebuild the snapshot from the daemon, publishing in stages so a slow value
/// read never blanks the whole menu.
///
/// `list()` is DDC-free and fast, so the daemon-up flag and the monitor list are
/// published the instant it returns. Slider values come from `get()`, which for
/// a sleeping display can take many seconds — those are filled in afterward,
/// per monitor, and a read that fails keeps the previously shown value rather
/// than dropping the slider. The one thing that must never happen — the menu
/// saying "displayd not running" while it plainly is — is now impossible as long
/// as `list()` succeeds.
fn refresh(client: &mut Option<Client>, snapshot: &Arc<Mutex<Snapshot>>, config: &Config) {
    let mark_down = |snapshot: &Arc<Mutex<Snapshot>>| {
        // Keep the caps cache (stale but harmless); just mark down + clear the
        // live monitor list, and bump so the settings window notices.
        let mut s = snapshot.lock().unwrap();
        s.daemon_up = false;
        s.monitors.clear();
        s.bump();
    };

    let Some(c) = ensure(client) else {
        mark_down(snapshot);
        return;
    };

    let monitors = match c.list() {
        Ok(m) => m,
        Err(_) => {
            mark_down(snapshot);
            *client = None; // reconnect next time
            return;
        }
    };

    // Last-known sliders/pickers, keyed by display id, so a slow/failed read
    // shows the previous value instead of an empty section.
    type PrevEntry = (Vec<SliderView>, Vec<PickerView>);
    let prev: std::collections::HashMap<u32, PrevEntry> = snapshot
        .lock()
        .unwrap()
        .monitors
        .iter()
        .map(|m| (m.id, (m.sliders.clone(), m.pickers.clone())))
        .collect();

    // Stage 1: publish daemon-up + monitors immediately, carrying prior values.
    let mut views: Vec<MonitorView> = monitors
        .iter()
        .filter(|m| !matches!(m.control, api::ControlPath::None))
        .map(|m| MonitorView {
            id: m.id,
            name: if m.product.is_empty() {
                m.vendor.clone()
            } else {
                format!("{} {}", m.vendor, m.product)
            },
            key: m.key.clone(),
            is_ddc: matches!(m.control, api::ControlPath::Ddc),
            sliders: prev.get(&m.id).map(|(s, _)| s.clone()).unwrap_or_default(),
            pickers: prev.get(&m.id).map(|(_, p)| p.clone()).unwrap_or_default(),
        })
        .collect();
    {
        let mut s = snapshot.lock().unwrap();
        // Bump only when the monitor set the settings window shows actually
        // changed (ids/names/type) — not on every routine value poll, which
        // would rebuild the open settings window every few seconds.
        let sig = |ms: &[MonitorView]| -> Vec<(u32, String, bool)> {
            ms.iter()
                .map(|m| (m.id, m.name.clone(), m.is_ddc))
                .collect()
        };
        let changed = !s.daemon_up || sig(&s.monitors) != sig(&views);
        s.daemon_up = true;
        s.monitors = views.clone();
        if changed {
            s.bump();
        }
    }

    // Stage 2: refine each monitor. A read that hangs on a sleeping display
    // delays only that monitor's refresh, not the menu.
    for (i, m) in views.iter_mut().enumerate() {
        let selector = m.id.to_string();

        // For DDC monitors, ensure capabilities are known. Fetched once per id —
        // the capability read is slow.
        if m.is_ddc {
            let have = snapshot.lock().unwrap().caps.contains_key(&m.id);
            if !have {
                if let Ok(caps) = c.caps(&selector) {
                    let view = to_caps_view(caps);
                    let mut s = snapshot.lock().unwrap();
                    s.caps.insert(m.id, view);
                    s.bump();
                }
            }
            // Build pickers from the advertised value lists, then read each
            // picker's current value to checkmark it.
            let value_lists = snapshot
                .lock()
                .unwrap()
                .caps
                .get(&m.id)
                .map(|cv| cv.value_lists.clone())
                .unwrap_or_default();
            m.pickers = build_pickers(&value_lists);
            for p in &mut m.pickers {
                if let Ok(v) = c.get(&selector, &format!("0x{:02X}", p.code)) {
                    p.current = Some((v.current & 0xFF) as u8);
                }
            }
        }

        // Which codes to show as sliders: the visible, adjustable, advertised
        // ones. Built-in panels have no capability string, so they get
        // brightness only.
        let codes: Vec<u8> = {
            let cfg = config.lock().unwrap();
            if m.is_ddc {
                let advertised: Vec<u8> = snapshot
                    .lock()
                    .unwrap()
                    .caps
                    .get(&m.id)
                    .map(|cv| cv.codes.iter().map(|(code, _)| *code).collect())
                    .unwrap_or_default();
                advertised
                    .into_iter()
                    .filter(|code| display_ddc::vcp::is_adjustable(*code))
                    .filter(|code| cfg.is_visible(&m.key, *code))
                    .collect()
            } else if cfg.is_visible(&m.key, 0x10) {
                vec![0x10]
            } else {
                Vec::new()
            }
        };

        // Read each visible code.
        let mut sliders = Vec::new();
        for code in &codes {
            if let Ok(v) = c.get(&selector, &format!("0x{code:02X}")) {
                if v.max > 0 && v.max <= 1000 {
                    sliders.push(SliderView {
                        label: display_ddc::vcp::VcpCode::from_code(*code).display_name(),
                        code: *code,
                        current: v.current,
                        max: v.max,
                    });
                }
            }
        }
        // Distinguish "user hid everything" (show nothing) from "all reads
        // failed because the display is asleep" (keep the prior values).
        if codes.is_empty() {
            m.sliders = Vec::new();
        } else if !sliders.is_empty() {
            m.sliders = sliders;
        }

        let mut s = snapshot.lock().unwrap();
        if let Some(slot) = s.monitors.get_mut(i) {
            slot.sliders = m.sliders.clone();
            slot.pickers = m.pickers.clone();
        }
    }

    let profiles: Vec<String> = c
        .profile_list()
        .map(|ps| ps.into_iter().map(|p| p.name).collect())
        .unwrap_or_default();
    let mut s = snapshot.lock().unwrap();
    if s.profiles != profiles {
        s.profiles = profiles;
        s.bump();
    }
}

/// Build a display-friendly capability view from the daemon's caps result.
fn to_caps_view(caps: api::CapsResult) -> CapsView {
    use display_ddc::vcp::VcpCode;
    let codes = caps
        .vcp_codes
        .iter()
        .map(|&c| (c, VcpCode::from_code(c).display_name()))
        .collect();
    CapsView {
        mccs: caps.mccs_version,
        codes,
        value_lists: caps.value_lists,
        unknown_sections: caps.unknown_sections,
    }
}

/// Build the enumerated pickers for a monitor from its advertised value lists.
/// Only curated picker codes with more than one choice become pickers.
fn build_pickers(value_lists: &[(u8, Vec<u8>)]) -> Vec<PickerView> {
    use display_ddc::vcp::{enum_value_name, is_picker, picker_title};
    value_lists
        .iter()
        .filter(|(code, values)| is_picker(*code) && values.len() > 1)
        .map(|(code, values)| PickerView {
            code: *code,
            title: picker_title(*code).unwrap_or("Options").to_string(),
            values: values
                .iter()
                .map(|&v| (v, enum_value_name(*code, v)))
                .collect(),
            current: None,
        })
        .collect()
}
