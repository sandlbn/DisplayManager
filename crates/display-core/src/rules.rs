//! Automation rules: trigger → action.
//!
//! Stored as a single hand-editable TOML file, so rules are versionable in a
//! dotfiles repo:
//!
//! ```toml
//! [[rule]]
//! name = "docked"
//! trigger = { display_connected = "U2720Q" }
//! action = { profile = "docked" }
//!
//! [[rule]]
//! name = "dim-on-battery"
//! trigger = { power = "battery" }
//! action = { set = { display = "all", code = "brightness", value = 30 } }
//! ```

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum RulesError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("rule {name:?}: {problem}")]
    Invalid { name: String, problem: String },
    #[error("serialize: {0}")]
    Serialize(#[from] toml::ser::Error),
}

pub type Result<T> = std::result::Result<T, RulesError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PowerState {
    Ac,
    Battery,
    /// Platform could not report it. Never matches a `power` trigger — a rule
    /// must not fire on a guess.
    Unknown,
}

/// What causes a rule to fire.
///
/// Externally tagged, so TOML reads as `trigger = { power = "battery" }`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// A display matching this selector was attached.
    DisplayConnected(String),
    /// A display matching this selector went away.
    DisplayDisconnected(String),
    /// Local wall-clock time, `HH:MM`. Fires once per day.
    Time(String),
    /// Power source changed to this state.
    Power(PowerState),
}

/// What a rule does when it fires.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Apply a named profile.
    Profile(String),
    /// Write one VCP value.
    Set {
        display: String,
        code: String,
        value: u16,
    },
    /// Run a shell command.
    ///
    /// The daemon runs as the user, so this is no more privileged than the
    /// user's own shell — but it does mean anything that can write the rules
    /// file can run code at login. That is the same trust level as
    /// `~/.zshrc`, and is why the loader warns on world-writable files.
    Run(String),
}

impl std::fmt::Display for PowerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PowerState::Ac => "AC power",
            PowerState::Battery => "battery",
            PowerState::Unknown => "unknown",
        })
    }
}

// Display rather than Debug for these: they are shown to users by
// `displayctl rules list`, where `Set { display: "all", code: "brightness" }`
// is Rust talking to itself rather than an explanation.
impl std::fmt::Display for Trigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Trigger::DisplayConnected(s) => write!(f, "display {s:?} is connected"),
            Trigger::DisplayDisconnected(s) => write!(f, "display {s:?} is disconnected"),
            Trigger::Time(t) => write!(f, "the clock reaches {t}"),
            Trigger::Power(p) => write!(f, "power switches to {p}"),
        }
    }
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::Profile(name) => write!(f, "apply profile {name:?}"),
            Action::Set {
                display,
                code,
                value,
            } => write!(f, "set {code} = {value} on {display}"),
            Action::Run(cmd) => write!(f, "run {cmd:?}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Rule {
    pub name: String,
    pub trigger: Trigger,
    pub action: Action,
    /// Allow an action that cannot be undone (input switch, factory reset).
    ///
    /// Off by default: a rule that silently switched inputs on hot-plug could
    /// leave the user with a blank screen and no way back.
    #[serde(default)]
    pub force: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RuleSet {
    #[serde(default, rename = "rule")]
    pub rules: Vec<Rule>,
}

/// Parse `HH:MM` into (hour, minute).
pub fn parse_time(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    (h < 24 && m < 60).then_some((h, m))
}

impl RuleSet {
    /// Reject rules that cannot work, at load time rather than at 3am when the
    /// trigger would have fired.
    pub fn validate(&self) -> Result<()> {
        for r in &self.rules {
            if r.name.trim().is_empty() {
                return Err(RulesError::Invalid {
                    name: r.name.clone(),
                    problem: "name must not be empty".into(),
                });
            }
            if let Trigger::Time(t) = &r.trigger {
                if parse_time(t).is_none() {
                    return Err(RulesError::Invalid {
                        name: r.name.clone(),
                        problem: format!("{t:?} is not a valid HH:MM time"),
                    });
                }
            }
            if let Trigger::Power(PowerState::Unknown) = &r.trigger {
                return Err(RulesError::Invalid {
                    name: r.name.clone(),
                    problem: "power trigger must be \"ac\" or \"battery\"".into(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct RulesStore {
    path: PathBuf,
}

impl RulesStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        RulesStore { path: path.into() }
    }

    pub fn default_location() -> Self {
        let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        RulesStore::new(base.join("DisplayStudio").join("rules.toml"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load rules. A missing file means no rules, not an error.
    pub fn load(&self) -> Result<RuleSet> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(RuleSet::default()),
            Err(e) => return Err(e.into()),
        };
        let set: RuleSet = toml::from_str(&text).map_err(|source| RulesError::Parse {
            path: self.path.display().to_string(),
            source,
        })?;
        set.validate()?;
        Ok(set)
    }

    pub fn save(&self, set: &RuleSet) -> Result<()> {
        set.validate()?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(set)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// True if the rules file is writable by anyone other than its owner.
    ///
    /// The file can run shell commands, so a world-writable one is a privilege
    /// escalation vector worth naming rather than silently honouring.
    #[cfg(unix)]
    pub fn is_insecure(&self) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&self.path)
            .map(|m| m.permissions().mode() & 0o022 != 0)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, RulesStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = RulesStore::new(dir.path().join("rules.toml"));
        (dir, s)
    }

    /// Hand-editing is the supported workflow, so this shape is a contract.
    #[test]
    fn parses_the_documented_toml_shape() {
        let (d, s) = store();
        std::fs::write(
            s.path(),
            r#"
[[rule]]
name = "docked"
trigger = { display_connected = "U2720Q" }
action = { profile = "docked" }

[[rule]]
name = "dim-on-battery"
trigger = { power = "battery" }
action = { set = { display = "all", code = "brightness", value = 30 } }

[[rule]]
name = "night"
trigger = { time = "22:00" }
action = { run = "echo goodnight" }
enabled = false
"#,
        )
        .unwrap();
        let set = s.load().unwrap();
        drop(d);

        assert_eq!(set.rules.len(), 3);
        assert_eq!(
            set.rules[0].trigger,
            Trigger::DisplayConnected("U2720Q".into())
        );
        assert_eq!(set.rules[0].action, Action::Profile("docked".into()));
        assert_eq!(set.rules[1].trigger, Trigger::Power(PowerState::Battery));
        assert_eq!(
            set.rules[1].action,
            Action::Set {
                display: "all".into(),
                code: "brightness".into(),
                value: 30
            }
        );
        assert_eq!(set.rules[2].trigger, Trigger::Time("22:00".into()));
        assert!(!set.rules[2].enabled);
        // Rules are enabled unless disabled.
        assert!(set.rules[0].enabled);
        // And non-destructive unless opted in.
        assert!(!set.rules[0].force);
    }

    #[test]
    fn round_trips_through_save_and_load() {
        let (_d, s) = store();
        let set = RuleSet {
            rules: vec![Rule {
                name: "r".into(),
                trigger: Trigger::Time("07:30".into()),
                action: Action::Profile("morning".into()),
                force: false,
                enabled: true,
            }],
        };
        s.save(&set).unwrap();
        assert_eq!(s.load().unwrap(), set);
    }

    #[test]
    fn missing_file_means_no_rules() {
        let s = RulesStore::new("/nonexistent/rules.toml");
        assert!(s.load().unwrap().rules.is_empty());
    }

    /// Catch a bad time at load, not when it should have fired.
    #[test]
    fn rejects_invalid_times_at_load() {
        for bad in ["25:00", "12:60", "noon", "12", "", "-1:00"] {
            let set = RuleSet {
                rules: vec![Rule {
                    name: "r".into(),
                    trigger: Trigger::Time(bad.into()),
                    action: Action::Profile("p".into()),
                    force: false,
                    enabled: true,
                }],
            };
            assert!(set.validate().is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn accepts_valid_times() {
        for ok in ["00:00", "23:59", "7:05", " 22:00 "] {
            assert!(parse_time(ok).is_some(), "{ok:?} should parse");
        }
        assert_eq!(parse_time("22:00"), Some((22, 0)));
        assert_eq!(parse_time("07:05"), Some((7, 5)));
    }

    #[test]
    fn rejects_unnamed_rules() {
        let set = RuleSet {
            rules: vec![Rule {
                name: "  ".into(),
                trigger: Trigger::Power(PowerState::Ac),
                action: Action::Profile("p".into()),
                force: false,
                enabled: true,
            }],
        };
        assert!(set.validate().is_err());
    }

    /// A rule must not fire because the platform could not tell us the state.
    #[test]
    fn rejects_a_power_trigger_on_unknown() {
        let set = RuleSet {
            rules: vec![Rule {
                name: "r".into(),
                trigger: Trigger::Power(PowerState::Unknown),
                action: Action::Profile("p".into()),
                force: false,
                enabled: true,
            }],
        };
        assert!(set.validate().is_err());
    }

    #[test]
    fn malformed_toml_names_the_file() {
        let (_d, s) = store();
        std::fs::write(s.path(), "[[rule]]\nname = = broken").unwrap();
        let err = s.load().unwrap_err().to_string();
        assert!(err.contains("rules.toml"), "{err}");
    }

    #[test]
    fn a_world_writable_rules_file_is_flagged() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, s) = store();
        s.save(&RuleSet::default()).unwrap();
        assert!(!s.is_insecure());
        std::fs::set_permissions(s.path(), std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(s.is_insecure(), "0666 rules file must be flagged");
    }
}
