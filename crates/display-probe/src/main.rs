//! Phase 1 deliverable: list every display with a full capability report.
//!
//! Becomes the diagnostics page later, so it prints what a bug report needs.

use display_core::{ControlPath, DisplayBackend};
use display_ddc::caps;
use display_ddc::vcp::{input_source_name, ValueKind, VcpCode};

/// Codes worth reading on every display, even when unadvertised.
const SURVEY: &[VcpCode] = &[
    VcpCode::Brightness,
    VcpCode::Contrast,
    VcpCode::Volume,
    VcpCode::InputSource,
    VcpCode::PowerMode,
    VcpCode::RedGain,
    VcpCode::GreenGain,
    VcpCode::BlueGain,
];

fn main() {
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("display-probe requires macOS");
        std::process::exit(1);
    }

    #[cfg(target_os = "macos")]
    run();
}

#[cfg(target_os = "macos")]
fn run() {
    let mut backend = match display_macos::MacosBackend::new() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("backend unavailable: {e}");
            std::process::exit(1);
        }
    };

    let monitors = match backend.list() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("enumeration failed: {e}");
            std::process::exit(1);
        }
    };

    println!("{} display(s)\n", monitors.len());

    for m in &monitors {
        println!("── {} ── displayID {}", m.identity, m.id);
        println!(
            "   control:  {}",
            match m.control {
                ControlPath::Ddc => "DDC/CI",
                ControlPath::Native => "built-in (DisplayServices)",
                ControlPath::None => "none",
            }
        );

        if !m.identity.location.is_empty() {
            println!("   location: {}", m.identity.location);
        }
        // Surfaced prominently: a placeholder serial means settings key off the
        // port instead, which is what the user sees if they swap cables.
        println!(
            "   serial:   {} ({})",
            m.identity.serial,
            if m.identity.has_trustworthy_serial() {
                "trustworthy"
            } else {
                "PLACEHOLDER — identity falls back to physical port"
            }
        );
        println!("   key:      {}", m.identity.settings_key());

        match m.control {
            ControlPath::Native => {
                if let Some(b) = backend.builtin_brightness(m.id) {
                    println!("   brightness: {:.0}%", b * 100.0);
                }
            }
            ControlPath::Ddc => probe_ddc(&mut backend, m),
            ControlPath::None => {}
        }
        println!();
    }
}

#[cfg(target_os = "macos")]
fn probe_ddc(backend: &mut display_macos::MacosBackend, m: &display_core::Monitor) {
    println!("\n   Capability string:");
    match backend.capability_string(m.id) {
        Ok(Some(raw)) => {
            println!("     raw: {raw}");
            match caps::parse(&raw) {
                Ok(c) => {
                    if let Some(v) = &c.mccs_version {
                        println!("     MCCS version: {v}");
                    }
                    if let Some(t) = &c.display_type {
                        println!("     type: {t}");
                    }
                    println!("     advertises {} VCP code(s):", c.vcp.len());
                    for f in &c.vcp {
                        let vals = if f.values.is_empty() {
                            String::new()
                        } else {
                            let named: Vec<String> = f
                                .values
                                .iter()
                                .map(|v| match input_source_name(*v) {
                                    Some(n) if f.code == VcpCode::InputSource => {
                                        format!("{v:#04x}={n}")
                                    }
                                    _ => format!("{v:#04x}"),
                                })
                                .collect();
                            format!("  [{}]", named.join(", "))
                        };
                        println!("       {}{}", f.code, vals);
                    }
                    if !c.unknown_sections.is_empty() {
                        println!("     unknown sections: {}", c.unknown_sections.join(", "));
                    }
                }
                Err(e) => println!("     parse failed: {e}"),
            }
        }
        Ok(None) => println!("     (no DDC path)"),
        Err(e) => println!("     read failed: {e}"),
    }

    println!("\n   Live VCP values:");
    for &code in SURVEY {
        match backend.get_vcp(m.id, code) {
            Ok((cur, max)) => {
                // For non-continuous codes the "max" byte is not a range bound,
                // so printing "26 / 3" would invent a scale that doesn't exist.
                let rendered = match code.kind() {
                    ValueKind::NonContinuous => {
                        let name = if code == VcpCode::InputSource {
                            input_source_name(cur as u8)
                                .map(|n| format!(" ({n})"))
                                .unwrap_or_else(|| " (vendor-specific)".into())
                        } else {
                            String::new()
                        };
                        format!("{cur:#04x}{name}")
                    }
                    _ => format!("{cur} / {max}"),
                };
                println!("     {:<28} {rendered}", code.to_string());
            }
            Err(e) => println!("     {:<28} —  ({e})", code.to_string()),
        }
    }
}
