//! Named snapshots of per-monitor settings.
//!
//! Stored as one TOML file per profile so they are human-editable and can live
//! in a dotfiles repo:
//!
//! ```toml
//! name = "coding"
//!
//! [[display]]
//! match = "MB169CK"
//!
//! [display.settings]
//! brightness = 60
//! contrast = 75
//! ```
//!
//! `match` uses the same selector grammar as the CLI (`all`, a display id, or a
//! case-insensitive product/vendor substring), so a profile written against one
//! machine's display ids still applies on another by name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("profile name {0:?} is invalid — use letters, digits, '-' or '_'")]
    InvalidName(String),
    #[error("profile {0:?} not found")]
    NotFound(String),
    #[error("profile {0:?} already exists — pass --force to overwrite")]
    Exists(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("serialize: {0}")]
    Serialize(#[from] toml::ser::Error),
}

pub type Result<T> = std::result::Result<T, ProfileError>;

/// Settings for one display within a profile.
///
/// `settings` keys are VCP codes by name ("brightness") or number ("0x10"), left
/// as strings so a hand-edited profile stays readable and unknown codes survive
/// a round-trip.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DisplayProfile {
    #[serde(rename = "match")]
    pub selector: String,
    #[serde(default)]
    pub settings: BTreeMap<String, u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default, rename = "display")]
    pub displays: Vec<DisplayProfile>,
}

impl Profile {
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Profile {
            name,
            displays: Vec::new(),
        })
    }
}

/// Reject anything that could escape the profiles directory or collide with
/// filesystem semantics.
///
/// A profile name becomes a filename, and names arrive from the CLI, the IPC
/// socket, and eventually a REST API — so `../../.ssh/authorized_keys` must be
/// rejected here rather than trusted to callers.
pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(ProfileError::InvalidName(name.to_string()))
    }
}

/// Where profiles live.
#[derive(Debug, Clone)]
pub struct ProfileStore {
    dir: PathBuf,
}

impl ProfileStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        ProfileStore { dir: dir.into() }
    }

    /// The default per-user location.
    pub fn default_location() -> Self {
        let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        ProfileStore::new(base.join("DisplayStudio").join("profiles"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_for(&self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        Ok(self.dir.join(format!("{name}.toml")))
    }

    /// Profile names, sorted. Missing directory means none, not an error.
    pub fn list(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                // Ignore anything not loadable by name, so a stray file cannot
                // make `list` fail.
                if validate_name(stem).is_ok() {
                    out.push(stem.to_string());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn load(&self, name: &str) -> Result<Profile> {
        let path = self.path_for(name)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ProfileError::NotFound(name.to_string()))
            }
            Err(e) => return Err(e.into()),
        };
        let mut profile: Profile = toml::from_str(&text).map_err(|source| ProfileError::Parse {
            path: path.display().to_string(),
            source,
        })?;
        // The filename is authoritative: a hand-edited `name` that disagrees
        // would make `apply <name>` fail confusingly.
        profile.name = name.to_string();
        Ok(profile)
    }

    pub fn save(&self, profile: &Profile, force: bool) -> Result<PathBuf> {
        let path = self.path_for(&profile.name)?;
        if path.exists() && !force {
            return Err(ProfileError::Exists(profile.name.clone()));
        }
        std::fs::create_dir_all(&self.dir)?;
        let text = toml::to_string_pretty(profile)?;
        // Write via a temp file in the same directory: a crash mid-write must
        // not leave a truncated profile where a valid one was.
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        let path = self.path_for(name)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(ProfileError::NotFound(name.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ProfileStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(dir.path());
        (dir, store)
    }

    fn sample() -> Profile {
        Profile {
            name: "coding".into(),
            displays: vec![DisplayProfile {
                selector: "MB169CK".into(),
                settings: BTreeMap::from([("brightness".into(), 60), ("contrast".into(), 75)]),
            }],
        }
    }

    /// A profile name becomes a path. Traversal must be impossible.
    #[test]
    fn rejects_names_that_could_escape_the_directory() {
        for bad in [
            "../evil",
            "../../.ssh/authorized_keys",
            "a/b",
            "a\\b",
            ".",
            "..",
            "",
            "with space",
            "semi;colon",
            "null\0byte",
        ] {
            assert!(
                validate_name(bad).is_err(),
                "{bad:?} must be rejected as a profile name"
            );
        }
    }

    #[test]
    fn accepts_reasonable_names() {
        for ok in ["coding", "movie-night", "photo_edit", "p1", "A-1_b"] {
            assert!(validate_name(ok).is_ok(), "{ok:?} should be valid");
        }
    }

    #[test]
    fn traversal_name_cannot_reach_the_filesystem() {
        let (_d, s) = store();
        assert!(matches!(
            s.load("../../../etc/passwd"),
            Err(ProfileError::InvalidName(_))
        ));
        assert!(matches!(
            s.delete("../boom"),
            Err(ProfileError::InvalidName(_))
        ));
    }

    #[test]
    fn saves_and_loads_round_trip() {
        let (_d, s) = store();
        s.save(&sample(), false).unwrap();
        assert_eq!(s.load("coding").unwrap(), sample());
    }

    #[test]
    fn save_refuses_to_clobber_without_force() {
        let (_d, s) = store();
        s.save(&sample(), false).unwrap();
        assert!(matches!(
            s.save(&sample(), false),
            Err(ProfileError::Exists(_))
        ));
        s.save(&sample(), true).unwrap();
    }

    #[test]
    fn missing_profile_is_not_found_not_io_error() {
        let (_d, s) = store();
        assert!(matches!(s.load("nope"), Err(ProfileError::NotFound(_))));
        assert!(matches!(s.delete("nope"), Err(ProfileError::NotFound(_))));
    }

    #[test]
    fn listing_an_absent_directory_yields_nothing() {
        let s = ProfileStore::new("/nonexistent/path/for/tests");
        assert!(s.list().unwrap().is_empty());
    }

    #[test]
    fn list_is_sorted_and_ignores_non_toml() {
        let (d, s) = store();
        for n in ["zebra", "alpha", "middle"] {
            let mut p = sample();
            p.name = n.into();
            s.save(&p, false).unwrap();
        }
        std::fs::write(d.path().join("notes.txt"), "ignore me").unwrap();
        assert_eq!(s.list().unwrap(), vec!["alpha", "middle", "zebra"]);
    }

    /// Hand-editing is a supported workflow, so the on-disk shape is a contract.
    #[test]
    fn hand_written_toml_parses() {
        let (d, s) = store();
        std::fs::create_dir_all(d.path()).unwrap();
        std::fs::write(
            d.path().join("manual.toml"),
            r#"
name = "whatever"

[[display]]
match = "MB169CK"

[display.settings]
brightness = 42
"0x12" = 70
"#,
        )
        .unwrap();
        let p = s.load("manual").unwrap();
        // Filename wins over the `name` field.
        assert_eq!(p.name, "manual");
        assert_eq!(p.displays[0].selector, "MB169CK");
        assert_eq!(p.displays[0].settings["brightness"], 42);
        assert_eq!(p.displays[0].settings["0x12"], 70);
    }

    #[test]
    fn a_profile_with_no_displays_is_valid() {
        let (_d, s) = store();
        let p = Profile::new("empty").unwrap();
        s.save(&p, false).unwrap();
        assert!(s.load("empty").unwrap().displays.is_empty());
    }

    #[test]
    fn malformed_toml_reports_the_path() {
        let (d, s) = store();
        std::fs::write(d.path().join("broken.toml"), "this is not = = toml").unwrap();
        let err = s.load("broken").unwrap_err().to_string();
        assert!(err.contains("broken.toml"), "{err}");
    }

    /// A crash mid-save must not destroy the previous profile.
    #[test]
    fn save_leaves_no_temp_file_behind() {
        let (d, s) = store();
        s.save(&sample(), false).unwrap();
        // Match the file name, not the full path: the temp dir itself lives
        // under a path containing "tmp".
        let stray: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(stray.is_empty(), "temp file left behind: {stray:?}");
    }
}
