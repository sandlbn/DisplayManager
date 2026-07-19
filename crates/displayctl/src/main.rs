//! `displayctl` — the headless product.
//!
//! Talks to `displayd` when it is running and falls back to driving the hardware
//! directly when it is not, so the CLI is useful before the daemon is installed.
//! Both paths run the same `Engine`, so behaviour cannot drift between them.

use clap::{Parser, Subcommand};
use display_api::protocol as api;
use display_ddc::vcp::{input_source_name, VcpCode};

mod backend;
use backend::Access;

#[derive(Parser)]
#[command(
    name = "displayctl",
    version,
    about = "Control external displays over DDC/CI"
)]
struct Cli {
    /// Emit JSON instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,

    /// After writing, read the value back and report if the display ignored it.
    ///
    /// DDC writes are unacknowledged, and monitors commonly advertise codes they
    /// do not implement, so a plain `set` cannot tell you whether anything
    /// happened. Costs one extra read per display.
    #[arg(long, global = true)]
    verify: bool,

    /// Bypass the daemon and drive the hardware directly.
    #[arg(long, global = true)]
    direct: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List displays.
    List,
    /// Show daemon and protocol versions.
    Version,
    /// Read a VCP value.
    Get {
        /// Code name ("brightness") or number ("0x10").
        code: String,
        #[arg(short, long, default_value = "")]
        display: String,
    },
    /// Write a VCP value.
    Set {
        code: String,
        /// Decimal ("75") or hex ("0x4B"). VCP values are conventionally hex.
        value: String,
        #[arg(short, long, default_value = "")]
        display: String,
        /// Required for codes that cannot be casually undone (input source,
        /// power, restore-factory-defaults).
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Get or set brightness (shorthand for `get`/`set brightness`).
    Brightness {
        /// Omit to read the current value.
        value: Option<u16>,
        #[arg(short, long, default_value = "")]
        display: String,
    },
    /// Get or set volume.
    Volume {
        value: Option<u16>,
        #[arg(short, long, default_value = "")]
        display: String,
    },
    /// Switch input source.
    Input {
        /// Input value, e.g. "0x11". Most inputs are vendor-specific; see `caps`.
        value: String,
        #[arg(short, long, default_value = "")]
        display: String,
        /// Required: switching to an input with no signal leaves no picture to
        /// undo it with.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Report a display's capabilities.
    Caps {
        #[arg(short, long, default_value = "")]
        display: String,
    },
    /// Manage profiles: named snapshots of display settings.
    #[command(subcommand)]
    Profile(ProfileCmd),
    /// Inspect automation rules.
    #[command(subcommand)]
    Rules(RulesCmd),
}

#[derive(Subcommand)]
enum RulesCmd {
    /// List loaded rules.
    List,
    /// Re-read the rules file without restarting the daemon.
    Reload,
    /// Evaluate rules right now instead of waiting for the next poll.
    Tick,
    /// Print the path of the rules file.
    Path,
}

#[derive(Subcommand)]
enum ProfileCmd {
    /// List saved profiles.
    List,
    /// Apply a profile.
    Apply {
        name: String,
        /// Required if the profile writes destructive codes.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Snapshot current settings into a profile.
    Save {
        name: String,
        /// Codes to capture. Defaults to brightness, contrast, volume, gains.
        #[arg(short, long)]
        code: Vec<String>,
        #[arg(short, long)]
        force: bool,
    },
    /// Print a profile's contents.
    Show { name: String },
    /// Delete a profile.
    Delete { name: String },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(&cli) {
        if cli.json {
            let out = serde_json::json!({ "error": e });
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
        } else {
            eprintln!("error: {e}");
        }
        std::process::exit(1);
    }
}

fn run(cli: &Cli) -> Result<(), String> {
    let mut access = Access::open(cli.direct)?;

    match &cli.command {
        Command::Version => {
            let v = access.version()?;
            if cli.json {
                print_json(&v);
            } else {
                println!("displayctl   {}", env!("CARGO_PKG_VERSION"));
                println!("daemon       {}", v.daemon);
                println!("protocol     {}", v.protocol);
                println!("source       {}", access.describe());
            }
        }

        Command::List => {
            let monitors = access.list()?;
            if cli.json {
                print_json(&monitors);
            } else {
                print_list(&monitors);
            }
        }

        Command::Get { code, display } => {
            let v = access.get(display, code)?;
            if cli.json {
                print_json(&v);
            } else {
                println!("{}", render_value(&v));
            }
        }

        Command::Set {
            code,
            value,
            display,
            yes,
        } => {
            // The raw escape hatch must not be a way around the confirmation
            // that the `input` sugar enforces.
            guard_destructive(code, *yes)?;
            let v = parse_value(value)?;
            let r = access.set(display, code, v, cli.verify)?;
            report_set(cli.json, code, v, &r);
        }

        Command::Brightness { value, display } => match value {
            Some(v) => {
                let r = access.set(display, "brightness", *v, cli.verify)?;
                report_set(cli.json, "brightness", *v, &r);
            }
            None => {
                let v = access.get(display, "brightness")?;
                if cli.json {
                    print_json(&v);
                } else {
                    println!("{}", render_value(&v));
                }
            }
        },

        Command::Volume { value, display } => match value {
            Some(v) => {
                let r = access.set(display, "volume", *v, cli.verify)?;
                report_set(cli.json, "volume", *v, &r);
            }
            None => {
                let v = access.get(display, "volume")?;
                if cli.json {
                    print_json(&v);
                } else {
                    println!("{}", render_value(&v));
                }
            }
        },

        Command::Input {
            value,
            display,
            yes,
        } => {
            let parsed = parse_input_value(value)?;
            if !yes {
                return Err(format!(
                    "refusing to switch input without --yes.\n\
                     If {parsed:#04x} has no signal the display goes blank, and you would need \
                     the monitor's own OSD to recover.\n\
                     Run `displayctl caps` to see which inputs this display advertises."
                ));
            }
            let r = access.set(display, "0x60", parsed, cli.verify)?;
            report_set(cli.json, "input", parsed, &r);
        }

        Command::Caps { display } => {
            let c = access.caps(display)?;
            if cli.json {
                print_json(&c);
            } else {
                print_caps(&c);
            }
        }

        Command::Profile(cmd) => run_profile(cli, &mut access, cmd)?,
        Command::Rules(cmd) => run_rules(cli, &mut access, cmd)?,
    }
    Ok(())
}

fn run_rules(cli: &Cli, access: &mut Access, cmd: &RulesCmd) -> Result<(), String> {
    match cmd {
        RulesCmd::Path => {
            let store = display_core::RulesStore::default_location();
            if cli.json {
                print_json(&serde_json::json!({ "path": store.path() }));
            } else {
                println!("{}", store.path().display());
                if store.is_insecure() {
                    eprintln!(
                        "warning: this file is writable by other users and can run shell \
                         commands — consider `chmod 600`"
                    );
                }
            }
        }

        RulesCmd::List => {
            let rules = access.rules_list()?;
            if cli.json {
                print_json(&rules);
            } else if rules.is_empty() {
                let store = display_core::RulesStore::default_location();
                println!("no rules loaded — create {}", store.path().display());
            } else {
                for r in &rules {
                    let state = if r.enabled { "" } else { "  (disabled)" };
                    let force = if r.force { "  [force]" } else { "" };
                    println!("{}{state}{force}", r.name);
                    println!("    when: {}", r.trigger);
                    println!("    do:   {}", r.action);
                }
            }
        }

        RulesCmd::Reload => {
            let rules = access.rules_reload()?;
            if cli.json {
                print_json(&rules);
            } else {
                println!("reloaded {} rule(s)", rules.len());
            }
        }

        RulesCmd::Tick => {
            let fired = access.rules_tick()?;
            if cli.json {
                print_json(&fired);
            } else if fired.is_empty() {
                println!("no rules fired");
            } else {
                for f in &fired {
                    let mark = if f.ok { "✓" } else { "✗" };
                    println!("{mark} {}: {}", f.name, f.detail);
                }
            }
        }
    }
    Ok(())
}

fn run_profile(cli: &Cli, access: &mut Access, cmd: &ProfileCmd) -> Result<(), String> {
    match cmd {
        ProfileCmd::List => {
            let profiles = access.profile_list()?;
            if cli.json {
                print_json(&profiles);
            } else if profiles.is_empty() {
                println!("no profiles yet — create one with `displayctl profile save <name>`");
            } else {
                for p in &profiles {
                    match p.displays {
                        Some(n) => println!("{:<20} {n} display(s)", p.name),
                        None => println!("{:<20} (unreadable — check the TOML)", p.name),
                    }
                }
            }
        }

        ProfileCmd::Apply { name, yes } => {
            let r = access.profile_apply(name, cli.verify, *yes)?;
            if cli.json {
                print_json(&r);
            } else {
                print_apply(&r, cli.verify);
            }
        }

        ProfileCmd::Save { name, code, force } => {
            let s = access.profile_save(name, code, *force)?;
            if cli.json {
                print_json(&s);
            } else {
                println!(
                    "saved profile {:?} with {} display(s)",
                    s.name,
                    s.displays.unwrap_or(0)
                );
            }
        }

        ProfileCmd::Show { name } => {
            let p = access.profile_show(name)?;
            if cli.json {
                print_json(&p);
            } else {
                // Show the TOML: it is the editable artefact, so print what the
                // user would actually change.
                match toml::to_string_pretty(&p) {
                    Ok(t) => println!("{t}"),
                    Err(e) => return Err(format!("cannot render profile: {e}")),
                }
            }
        }

        ProfileCmd::Delete { name } => {
            access.profile_delete(name)?;
            if cli.json {
                print_json(&serde_json::json!({ "ok": true, "deleted": name }));
            } else {
                println!("deleted profile {name:?}");
            }
        }
    }
    Ok(())
}

/// Report an apply, distinguishing "did something" from "claimed to".
fn print_apply(r: &api::ApplyResult, verified: bool) {
    use api::ApplyStatus::*;
    for o in &r.outcomes {
        match o.status {
            NotConnected => println!("  {:<24} — not connected", o.selector),
            Applied => println!(
                "  [{}] {:<22} = {} ✓",
                o.display.unwrap_or(0),
                o.name,
                o.value
            ),
            Ignored => println!(
                "  [{}] {:<22} = {} — IGNORED by display",
                o.display.unwrap_or(0),
                o.name,
                o.value
            ),
            Unverified => println!(
                "  [{}] {:<22} = {}",
                o.display.unwrap_or(0),
                o.name,
                o.value
            ),
        }
    }

    let applied = r.count(Applied);
    let ignored = r.count(Ignored);
    let absent = r.count(NotConnected);
    println!("\napplied profile {:?}", r.profile);
    if ignored > 0 {
        println!("  {ignored} setting(s) were IGNORED — the display does not implement them");
    }
    if absent > 0 {
        println!("  {absent} display(s) in this profile are not connected");
    }
    if !verified {
        // Without a read-back we genuinely do not know, and saying "applied"
        // unqualified would be a claim we cannot support.
        println!(
            "  note: writes were not verified. DDC does not acknowledge writes, so this is \
             not proof anything changed — re-run with --verify to check."
        );
    } else if ignored == 0 && applied > 0 {
        println!("  all {applied} setting(s) confirmed");
    }
}

/// Refuse a destructive write unless the caller explicitly opted in.
///
/// The message says what specifically goes wrong, because "are you sure?" does
/// not help someone who does not know that `0x04` is a factory reset.
fn guard_destructive(code_str: &str, yes: bool) -> Result<(), String> {
    if yes {
        return Ok(());
    }
    let Some(code) = display_ddc::vcp::parse_code(code_str) else {
        return Ok(()); // Unparseable codes fail later, with a better message.
    };
    if !code.is_destructive() {
        return Ok(());
    }
    let consequence = match code {
        VcpCode::InputSource => {
            "If the target input has no signal the display goes blank, and you would need \
             the monitor's own OSD to get back."
        }
        VcpCode::PowerMode => {
            "This can put the display into standby, which may need a physical button to undo."
        }
        VcpCode::RestoreFactoryDefaults
        | VcpCode::RestoreFactoryLuminance
        | VcpCode::RestoreFactoryColor => {
            "This wipes the monitor's saved settings — including ones this tool never set. \
             There is no undo."
        }
        _ => "This cannot be easily undone.",
    };
    Err(format!(
        "refusing to write {} without --yes.\n{consequence}",
        code.display_name()
    ))
}

/// Parse a VCP value: decimal, or hex with an `0x` prefix.
fn parse_value(s: &str) -> Result<u16, String> {
    let t = s.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u16::from_str_radix(hex, 16).map_err(|_| format!("bad hex value {s:?}"));
    }
    t.parse::<u16>()
        .map_err(|_| format!("bad value {s:?} — use a decimal like 75 or hex like 0x4B"))
}

fn parse_input_value(s: &str) -> Result<u16, String> {
    parse_value(s).map_err(|e| {
        format!(
            "{e}\nInput names are vendor-specific, so `displayctl caps` is the way to find yours."
        )
    })
}

fn report_set(json: bool, what: &str, value: u16, r: &api::SetResult) {
    if json {
        print_json(&serde_json::json!({
            "ok": r.ignored.is_empty(),
            "code": what,
            "value": value,
            "displays": r.displays,
            "ignored": r.ignored,
        }));
        return;
    }
    let plural = if r.displays == 1 {
        "display"
    } else {
        "displays"
    };
    println!("set {what} = {value} on {} {plural}", r.displays);
    if !r.ignored.is_empty() {
        // The write "succeeded" at the protocol level and did nothing. Say so
        // plainly rather than letting the success line stand unqualified.
        let ids: Vec<String> = r.ignored.iter().map(|i| i.to_string()).collect();
        eprintln!(
            "warning: display {} ignored the write — the value did not change.\n\
             The monitor may advertise {what} without implementing it.",
            ids.join(", ")
        );
    }
}

fn print_json<T: serde::Serialize>(v: &T) {
    println!("{}", serde_json::to_string_pretty(v).unwrap());
}

fn render_value(v: &api::VcpValue) -> String {
    match v.kind {
        // The max byte is not a range bound for these; rendering "26 / 3" would
        // invent a scale that does not exist.
        api::ValueKind::NonContinuous => {
            let name = if v.code == VcpCode::InputSource.code() {
                input_source_name(v.current as u8)
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_else(|| " (vendor-specific)".into())
            } else {
                String::new()
            };
            format!("{}: {:#04x}{}", v.name, v.current, name)
        }
        _ => format!("{}: {} / {}", v.name, v.current, v.max),
    }
}

fn print_list(monitors: &[api::MonitorInfo]) {
    if monitors.is_empty() {
        println!("no displays found");
        return;
    }
    for m in monitors {
        let name = if m.product.is_empty() {
            format!("{} (unnamed)", m.vendor)
        } else {
            format!("{} {}", m.vendor, m.product)
        };
        let control = match m.control {
            api::ControlPath::Ddc => "DDC/CI",
            api::ControlPath::Native => "built-in",
            api::ControlPath::None => "not controllable",
        };
        println!("[{}] {name}  —  {control}", m.id);
        if !m.serial_trustworthy && !m.alphanumeric_serial.is_empty() {
            println!(
                "     serial: {} (numeric serial is a placeholder)",
                m.alphanumeric_serial
            );
        } else if m.serial_trustworthy {
            println!("     serial: {}", m.serial);
        }
    }
}

fn print_caps(c: &api::CapsResult) {
    match &c.raw {
        None => println!("no capability string (display has no DDC path)"),
        Some(raw) => {
            if let Some(v) = &c.mccs_version {
                println!("MCCS version: {v}");
            }
            println!("advertises {} VCP code(s):", c.vcp_codes.len());
            for code in &c.vcp_codes {
                let vc = VcpCode::from_code(*code);
                println!("  {vc}");
            }
            if !c.unknown_sections.is_empty() {
                println!("unknown sections: {}", c.unknown_sections.join(", "));
            }
            println!("\nraw: {raw}");
        }
    }
}
