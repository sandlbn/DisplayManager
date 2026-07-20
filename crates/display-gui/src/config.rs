//! Persisted GUI preferences.
//!
//! Kept separate from the daemon's state: these are choices about the *menu*
//! (which sliders to show, whether to auto-launch), not about the hardware.
//! Stored as TOML in Application Support, keyed by each display's stable
//! settings key so a preference survives reconnects and id changes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Codes shown in the menu by default (before any customization): brightness,
/// contrast, volume. Everything else a monitor advertises is opt-in.
pub const DEFAULT_VISIBLE: &[u8] = &[0x10, 0x12, 0x62];

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct GuiConfig {
    /// Launch the GUI at login.
    #[serde(default)]
    pub launch_at_login: bool,
    /// Per-display explicit set of visible slider codes, keyed by settings key.
    /// Absent = use [`DEFAULT_VISIBLE`]; present = exactly this set. Stored as
    /// hex strings ("0x59") so the file stays readable.
    #[serde(default)]
    pub visible: BTreeMap<String, BTreeSet<String>>,
    /// Per-display codes the monitor advertises but ignores writes to (detected
    /// by read-back). Their sliders are shown greyed-out. Persisted so a known
    /// dead control stays disabled across restarts.
    #[serde(default)]
    pub ignored: BTreeMap<String, BTreeSet<String>>,
}

fn code_key(code: u8) -> String {
    format!("0x{code:02X}")
}

impl GuiConfig {
    fn path() -> PathBuf {
        let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("DisplayStudio").join("gui.toml")
    }

    /// Load, treating any error (missing, corrupt) as defaults — a broken
    /// preferences file must never stop the menu from working.
    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = toml::to_string_pretty(self) {
            // Best-effort atomic write.
            let tmp = path.with_extension("toml.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    /// Whether a code is shown in the menu for a display.
    ///
    /// Uses the per-display explicit set if the user has customized this
    /// display, otherwise the default (brightness/contrast/volume).
    pub fn is_visible(&self, key: &str, code: u8) -> bool {
        match self.visible.get(key) {
            Some(set) => set.contains(&code_key(code)),
            None => DEFAULT_VISIBLE.contains(&code),
        }
    }

    /// Show or hide a code for a display, persisting immediately. The first
    /// change to a display seeds its set from the defaults, so unrelated
    /// defaults stay visible.
    pub fn set_visible(&mut self, key: &str, code: u8, visible: bool) {
        let set = self
            .visible
            .entry(key.to_string())
            .or_insert_with(|| DEFAULT_VISIBLE.iter().map(|c| code_key(*c)).collect());
        if visible {
            set.insert(code_key(code));
        } else {
            set.remove(&code_key(code));
        }
        self.save();
    }

    /// Whether a code was detected as ignored (write did not stick) for a display.
    pub fn is_ignored(&self, key: &str, code: u8) -> bool {
        self.ignored
            .get(key)
            .is_some_and(|s| s.contains(&code_key(code)))
    }

    /// Record whether a code is ignored, persisting only on an actual change so
    /// a working control does not thrash the file on every write.
    pub fn set_ignored(&mut self, key: &str, code: u8, ignored: bool) {
        let changed = if ignored {
            self.ignored
                .entry(key.to_string())
                .or_default()
                .insert(code_key(code))
        } else {
            let removed = self
                .ignored
                .get_mut(key)
                .is_some_and(|s| s.remove(&code_key(code)));
            if self.ignored.get(key).is_some_and(|s| s.is_empty()) {
                self.ignored.remove(key);
            }
            removed
        };
        if changed {
            self.save();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_show_brightness_contrast_volume_only() {
        let c = GuiConfig::default();
        let k = "DEL:U2720Q:123";
        assert!(c.is_visible(k, 0x10)); // brightness
        assert!(c.is_visible(k, 0x12)); // contrast
        assert!(c.is_visible(k, 0x62)); // volume
        assert!(!c.is_visible(k, 0x59)); // saturation: opt-in
    }

    #[test]
    fn enabling_a_code_keeps_defaults_visible() {
        let mut c = GuiConfig::default();
        let k = "DEL:U2720Q:123";
        // Enable saturation red; brightness/contrast/volume must remain shown.
        c.set_visible(k, 0x59, true);
        assert!(c.is_visible(k, 0x59));
        assert!(c.is_visible(k, 0x10));
        assert!(c.is_visible(k, 0x62));
    }

    #[test]
    fn hiding_a_default_persists_and_round_trips() {
        let mut c = GuiConfig::default();
        let k = "DEL:U2720Q:123";
        c.set_visible(k, 0x62, false); // hide volume
        assert!(!c.is_visible(k, 0x62));
        assert!(c.is_visible(k, 0x10)); // brightness still shown

        let text = toml::to_string_pretty(&c).unwrap();
        let back: GuiConfig = toml::from_str(&text).unwrap();
        assert!(!back.is_visible(k, 0x62));
        assert!(back.is_visible(k, 0x10));
    }

    #[test]
    fn launch_at_login_defaults_off() {
        assert!(!GuiConfig::default().launch_at_login);
    }
}
