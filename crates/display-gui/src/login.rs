//! Launch-at-login for the GUI.
//!
//! For a packaged `.app` the right API is `SMAppService.mainApp`, which needs a
//! real bundle. This dev binary has no bundle, so it manages a LaunchAgent plist
//! pointing at the current executable — the same mechanism, one layer lower. The
//! settings toggle calls [`set_enabled`]; the packaging step will swap this for
//! SMAppService once there is an `.app`.

use std::path::PathBuf;

const LABEL: &str = "io.github.displaymanager.gui";

fn plist_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// Enable or disable launch-at-login. Best-effort: a failure here must not take
/// down the settings window, so errors are swallowed (the toggle simply won't
/// stick, which the checkbox state reflects on next open).
pub fn set_enabled(on: bool) {
    let path = plist_path();
    if on {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key><array><string>{}</string></array>
    <key>RunAtLoad</key><true/>
</dict>
</plist>"#,
            exe.display()
        );
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, plist);
    } else {
        let _ = std::fs::remove_file(&path);
    }
}
