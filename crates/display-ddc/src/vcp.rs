//! MCCS VCP code table.
//!
//! Not exhaustive — MCCS 2.2 defines well over a hundred codes and most are
//! never implemented by real monitors. This covers what the product actually
//! drives plus the codes worth surfacing in the capability explorer. Unknown
//! codes stay addressable via [`VcpCode::Raw`] so the CLI's `vcp get 0xE1`
//! escape hatch works against vendor-specific features.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VcpCode {
    Brightness,
    Contrast,
    Volume,
    Mute,
    InputSource,
    PowerMode,
    RedGain,
    GreenGain,
    BlueGain,
    ColorPreset,
    OsdLanguage,
    SpeakerSelect,
    Sharpness,
    /// Write-only: signals that a control value changed.
    NewControlValue,
    /// Write-only: **resets the monitor to factory settings**.
    RestoreFactoryDefaults,
    /// Write-only: resets brightness/contrast to factory settings.
    RestoreFactoryLuminance,
    /// Write-only: resets colour settings to factory settings.
    RestoreFactoryColor,
    /// Any code not in the table above, including vendor-specific ones.
    Raw(u8),
}

/// Whether a code carries a continuous range or an enumerated set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Continuous,
    NonContinuous,
    Unknown,
}

impl VcpCode {
    pub fn code(self) -> u8 {
        match self {
            VcpCode::NewControlValue => 0x02,
            VcpCode::RestoreFactoryDefaults => 0x04,
            VcpCode::RestoreFactoryLuminance => 0x05,
            VcpCode::RestoreFactoryColor => 0x08,
            VcpCode::Brightness => 0x10,
            VcpCode::Contrast => 0x12,
            VcpCode::RedGain => 0x16,
            VcpCode::GreenGain => 0x18,
            VcpCode::BlueGain => 0x1A,
            VcpCode::Sharpness => 0x87,
            VcpCode::ColorPreset => 0x14,
            VcpCode::SpeakerSelect => 0x63,
            VcpCode::Volume => 0x62,
            VcpCode::Mute => 0x8D,
            VcpCode::InputSource => 0x60,
            VcpCode::OsdLanguage => 0xCC,
            VcpCode::PowerMode => 0xD6,
            VcpCode::Raw(c) => c,
        }
    }

    pub fn from_code(c: u8) -> VcpCode {
        match c {
            0x02 => VcpCode::NewControlValue,
            0x04 => VcpCode::RestoreFactoryDefaults,
            0x05 => VcpCode::RestoreFactoryLuminance,
            0x08 => VcpCode::RestoreFactoryColor,
            0x10 => VcpCode::Brightness,
            0x12 => VcpCode::Contrast,
            0x14 => VcpCode::ColorPreset,
            0x16 => VcpCode::RedGain,
            0x18 => VcpCode::GreenGain,
            0x1A => VcpCode::BlueGain,
            0x60 => VcpCode::InputSource,
            0x62 => VcpCode::Volume,
            0x63 => VcpCode::SpeakerSelect,
            0x87 => VcpCode::Sharpness,
            0x8D => VcpCode::Mute,
            0xCC => VcpCode::OsdLanguage,
            0xD6 => VcpCode::PowerMode,
            other => VcpCode::Raw(other),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            VcpCode::NewControlValue => "New Control Value",
            VcpCode::RestoreFactoryDefaults => "Restore Factory Defaults",
            VcpCode::RestoreFactoryLuminance => "Restore Factory Luminance",
            VcpCode::RestoreFactoryColor => "Restore Factory Color",
            VcpCode::Brightness => "Brightness",
            VcpCode::Contrast => "Contrast",
            VcpCode::Volume => "Volume",
            VcpCode::Mute => "Mute",
            VcpCode::InputSource => "Input Source",
            VcpCode::PowerMode => "Power Mode",
            VcpCode::RedGain => "Red Gain",
            VcpCode::GreenGain => "Green Gain",
            VcpCode::BlueGain => "Blue Gain",
            VcpCode::ColorPreset => "Color Preset",
            VcpCode::OsdLanguage => "OSD Language",
            VcpCode::SpeakerSelect => "Speaker Select",
            VcpCode::Sharpness => "Sharpness",
            VcpCode::Raw(_) => "Unknown",
        }
    }

    /// Human-readable label. For codes not modelled as a variant, consults the
    /// standard MCCS table before giving up — so `0x52`, the 6-axis saturation
    /// codes, and friends get real names rather than `VCP 0x52`.
    pub fn display_name(self) -> String {
        match self {
            VcpCode::Raw(c) => standard_name(c)
                .map(|n| n.to_string())
                .unwrap_or_else(|| format!("VCP {c:#04x}")),
            _ => self.name().to_string(),
        }
    }

    pub fn kind(self) -> ValueKind {
        match self {
            VcpCode::Brightness
            | VcpCode::Contrast
            | VcpCode::Volume
            | VcpCode::RedGain
            | VcpCode::GreenGain
            | VcpCode::BlueGain
            | VcpCode::Sharpness => ValueKind::Continuous,
            VcpCode::Mute
            | VcpCode::InputSource
            | VcpCode::PowerMode
            | VcpCode::ColorPreset
            | VcpCode::OsdLanguage
            | VcpCode::SpeakerSelect
            | VcpCode::NewControlValue
            | VcpCode::RestoreFactoryDefaults
            | VcpCode::RestoreFactoryLuminance
            | VcpCode::RestoreFactoryColor => ValueKind::NonContinuous,
            VcpCode::Raw(_) => ValueKind::Unknown,
        }
    }

    /// Whether writing this code does something the user cannot casually undo.
    ///
    /// Two distinct hazards:
    ///
    /// - **Input source / power** — the plan's named footgun. Send a display to
    ///   an input with no signal and the user has no picture with which to undo
    ///   it; their only recovery is the monitor's own OSD.
    /// - **Restore-factory codes** — writing `0x04` silently wipes every setting
    ///   on the monitor, including ones this app never touched. Found by
    ///   sweeping the dev-bench monitor, which advertises both `0x04` and `0x08`.
    ///
    /// Callers must require explicit confirmation before writing these. The GUI
    /// additionally wants confirm-with-timeout for input changes.
    pub fn is_destructive(self) -> bool {
        matches!(
            self,
            VcpCode::InputSource
                | VcpCode::PowerMode
                | VcpCode::RestoreFactoryDefaults
                | VcpCode::RestoreFactoryLuminance
                | VcpCode::RestoreFactoryColor
        )
    }
}

impl fmt::Display for VcpCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VcpCode::Raw(c) => write!(f, "VCP {c:#04x}"),
            _ => write!(f, "{} (VCP {:#04x})", self.name(), self.code()),
        }
    }
}

/// Name for a standard MCCS 2.2 VCP code.
///
/// Covers the spec-defined codes, which are identical across every conforming
/// vendor — so a monitor advertising `0x59` really does mean 6-axis saturation
/// red, not something proprietary. Codes `0xE0`–`0xFF` are the manufacturer-
/// specific range and deliberately return `None`: their meaning varies per
/// vendor and can only come from a per-model database, never a guess.
///
/// Not exhaustive on the CRT-geometry codes (pincushion, convergence, …), which
/// no flat panel implements; the flat-panel-relevant set is complete.
pub fn standard_name(code: u8) -> Option<&'static str> {
    Some(match code {
        0x01 => "Degauss",
        0x02 => "New Control Value",
        0x03 => "Soft Controls",
        0x04 => "Restore Factory Defaults",
        0x05 => "Restore Factory Luminance/Contrast",
        0x06 => "Restore Factory Geometry",
        0x08 => "Restore Factory Color",
        0x0A => "Restore Factory TV Defaults",
        0x0B => "Color Temperature Increment",
        0x0C => "Color Temperature Request",
        0x0E => "Clock",
        0x10 => "Brightness",
        0x11 => "Flesh Tone Enhancement",
        0x12 => "Contrast",
        0x13 => "Backlight Control",
        0x14 => "Color Preset",
        0x16 => "Red Gain",
        0x17 => "User Color Vision Compensation",
        0x18 => "Green Gain",
        0x1A => "Blue Gain",
        0x1C => "Focus",
        0x1E => "Auto Setup",
        0x1F => "Auto Color Setup",
        0x20 => "Horizontal Position",
        0x22 => "Horizontal Size",
        0x30 => "Vertical Position",
        0x32 => "Vertical Size",
        0x3E => "Clock Phase",
        0x52 => "Active Control",
        0x54 => "Performance Preservation",
        0x56 => "Horizontal Moiré",
        0x58 => "Vertical Moiré",
        0x59 => "6-Axis Saturation: Red",
        0x5A => "6-Axis Saturation: Yellow",
        0x5B => "6-Axis Saturation: Green",
        0x5C => "6-Axis Saturation: Cyan",
        0x5D => "6-Axis Saturation: Blue",
        0x5E => "6-Axis Saturation: Magenta",
        0x60 => "Input Source",
        0x62 => "Volume",
        0x63 => "Speaker Select",
        0x64 => "Microphone Volume",
        0x66 => "Ambient Light Sensor",
        0x6C => "Red Black Level",
        0x6E => "Green Black Level",
        0x70 => "Blue Black Level",
        0x72 => "Gamma",
        0x7C => "Adjust Zoom",
        0x82 => "Horizontal Mirror",
        0x84 => "Vertical Mirror",
        0x86 => "Display Scaling",
        0x87 => "Sharpness",
        0x88 => "Velocity Scan Modulation",
        0x8A => "Color Saturation",
        0x8B => "TV Channel Up/Down",
        0x8C => "TV Sharpness",
        0x8D => "Mute / Screen Blank",
        0x8E => "TV Contrast",
        0x8F => "Audio Treble",
        0x90 => "Hue",
        0x91 => "Audio Bass",
        0x92 => "TV Black Level",
        0x94 => "Audio Balance",
        0x9B => "6-Axis Hue: Red",
        0x9C => "6-Axis Hue: Yellow",
        0x9D => "6-Axis Hue: Green",
        0x9E => "6-Axis Hue: Cyan",
        0x9F => "6-Axis Hue: Blue",
        0xA0 => "6-Axis Hue: Magenta",
        0xA2 => "Auto Setup On/Off",
        0xA4 => "Window Mask Control",
        0xA5 => "Change Selected Window",
        0xAA => "Screen Orientation",
        0xAC => "Horizontal Frequency",
        0xAE => "Vertical Frequency",
        0xB0 => "Settings (Store/Restore)",
        0xB2 => "Flat Panel Sub-Pixel Layout",
        0xB4 => "Source Timing Mode",
        0xB6 => "Display Technology Type",
        0xC0 => "Display Usage Time",
        0xC6 => "Application Enable Key",
        0xC8 => "Display Controller Type",
        0xC9 => "Display Firmware Level",
        0xCA => "OSD / Button Control",
        0xCC => "OSD Language",
        0xCD => "Status Indicators",
        0xCE => "Auxiliary Display Size",
        0xD0 => "Output Select",
        0xD4 => "Stereo Video Mode",
        0xD6 => "Power Mode",
        0xDA => "Scan Mode",
        0xDB => "Image Mode",
        0xDC => "Display Mode / Preset",
        0xDF => "VCP Version",
        // 0xE0–0xFF: manufacturer-specific — meaning varies by vendor.
        _ => return None,
    })
}

/// Whether a code is a continuous 0–max control that makes sense as a slider.
///
/// Used to decide which advertised codes the menu can offer as sliders. Excludes
/// non-continuous codes (input source, presets, power — those are pickers or
/// actions) and read-only/informational codes. A monitor may still ignore writes
/// to any of these (advertised ≠ implemented), which read-after-write detects.
pub fn is_adjustable(code: u8) -> bool {
    matches!(
        code,
        0x10 // brightness
        | 0x12 // contrast
        | 0x16 | 0x18 | 0x1A // RGB gain
        | 0x59..=0x5E // 6-axis saturation
        | 0x62 // volume
        | 0x64 // mic volume
        | 0x6C | 0x6E | 0x70 // RGB black level
        | 0x72 // gamma
        | 0x87 // sharpness
        | 0x8A // color saturation
        | 0x8F // treble
        | 0x90 // hue
        | 0x91 // bass
        | 0x9B..=0xA0 // 6-axis hue
    )
}

/// Names for VCP 0x60 (Input Source) values **defined by MCCS**.
///
/// MCCS 2.2 defines this enumeration only through `0x12`; every higher value is
/// vendor-specific and means whatever the vendor decided. Returning `None` for
/// those is a safety property, not an omission: input switching is destructive,
/// and a confidently wrong label ("USB-C") invites the user to select an input
/// that shows nothing, leaving them no picture with which to undo it. Report
/// unknown values as raw numbers and let the capability DB name them per model.
///
/// The dev-bench ASUS MB169CK advertises `0x11 0x1A 0x1B` and sits on `0x1A` —
/// two of its three inputs are outside the standard.
pub fn input_source_name(value: u8) -> Option<&'static str> {
    Some(match value {
        0x01 => "VGA-1",
        0x02 => "VGA-2",
        0x03 => "DVI-1",
        0x04 => "DVI-2",
        0x05 => "Composite-1",
        0x06 => "Composite-2",
        0x07 => "S-Video-1",
        0x08 => "S-Video-2",
        0x09 => "Tuner-1",
        0x0A => "Tuner-2",
        0x0B => "Tuner-3",
        0x0C => "Component-1",
        0x0D => "Component-2",
        0x0E => "Component-3",
        0x0F => "DisplayPort-1",
        0x10 => "DisplayPort-2",
        0x11 => "HDMI-1",
        0x12 => "HDMI-2",
        _ => return None,
    })
}

/// Standard names for VCP 0x14 (Color Preset) values.
pub fn color_preset_name(value: u8) -> Option<&'static str> {
    Some(match value {
        0x01 => "sRGB",
        0x02 => "Display Native",
        0x03 => "4000 K",
        0x04 => "5000 K",
        0x05 => "6500 K",
        0x06 => "7500 K",
        0x07 => "8200 K",
        0x08 => "9300 K",
        0x09 => "10000 K",
        0x0A => "11500 K",
        0x0B => "User 1",
        0x0C => "User 2",
        0x0D => "User 3",
        _ => return None,
    })
}

/// Standard names for VCP 0xAA (Screen Orientation) values.
pub fn orientation_name(value: u8) -> Option<&'static str> {
    Some(match value {
        0x01 => "0° (Landscape)",
        0x02 => "90°",
        0x03 => "180°",
        0x04 => "270°",
        0xFF => "Not Applicable",
        _ => return None,
    })
}

/// Standard names for VCP 0xCC (OSD Language) values.
pub fn osd_language_name(value: u8) -> Option<&'static str> {
    Some(match value {
        0x01 => "Chinese (Traditional)",
        0x02 => "English",
        0x03 => "French",
        0x04 => "German",
        0x05 => "Italian",
        0x06 => "Japanese",
        0x07 => "Korean",
        0x08 => "Portuguese (Portugal)",
        0x09 => "Russian",
        0x0A => "Spanish",
        0x0B => "Swedish",
        0x0C => "Turkish",
        0x0D => "Chinese (Simplified)",
        0x0E => "Portuguese (Brazil)",
        0x0F => "Arabic",
        0x10 => "Bulgarian",
        0x11 => "Croatian",
        0x12 => "Czech",
        0x13 => "Danish",
        0x14 => "Dutch",
        0x15 => "Finnish",
        0x16 => "Greek",
        0x17 => "Hebrew",
        0x18 => "Hindi",
        0x19 => "Hungarian",
        0x1A => "Latvian",
        0x1B => "Lithuanian",
        0x1C => "Norwegian",
        0x1D => "Polish",
        0x1E => "Romanian",
        0x1F => "Serbian",
        0x20 => "Slovak",
        0x21 => "Slovenian",
        0x22 => "Thai",
        0x23 => "Ukrainian",
        0x24 => "Vietnamese",
        _ => return None,
    })
}

/// Codes that should be offered as an enumerated **picker** (a submenu of
/// choices), with the submenu title. `None` for codes that are not pickers.
///
/// Deliberately curated: only enumerated codes whose values have standard names
/// and that a user would actually want to switch. Power mode is excluded (it can
/// blank the screen and is handled by the explicit Off/On items instead), as are
/// the restore-factory codes.
pub fn picker_title(code: u8) -> Option<&'static str> {
    Some(match code {
        0x60 => "Input",
        0x14 => "Color Preset",
        0xAA => "Orientation",
        0xCC => "OSD Language",
        _ => return None,
    })
}

/// Whether a code is offered as an enumerated picker.
pub fn is_picker(code: u8) -> bool {
    picker_title(code).is_some()
}

/// Friendly name for one value of an enumerated picker code. Falls back to the
/// raw value for vendor-specific values (common above the MCCS-standard range).
pub fn enum_value_name(code: u8, value: u8) -> String {
    let named = match code {
        0x60 => input_source_name(value),
        0x14 => color_preset_name(value),
        0xAA => orientation_name(value),
        0xCC => osd_language_name(value),
        _ => None,
    };
    named
        .map(|n| n.to_string())
        .unwrap_or_else(|| format!("0x{value:02X}"))
}

/// Parse a code from CLI input: a name ("brightness") or a number ("0x10", "16").
pub fn parse_code(s: &str) -> Option<VcpCode> {
    let t = s.trim();
    let normalized = t.to_ascii_lowercase().replace(['-', '_', ' '], "");
    let by_name = match normalized.as_str() {
        "brightness" => Some(VcpCode::Brightness),
        "contrast" => Some(VcpCode::Contrast),
        "volume" => Some(VcpCode::Volume),
        "mute" => Some(VcpCode::Mute),
        "input" | "inputsource" => Some(VcpCode::InputSource),
        "power" | "powermode" => Some(VcpCode::PowerMode),
        "red" | "redgain" => Some(VcpCode::RedGain),
        "green" | "greengain" => Some(VcpCode::GreenGain),
        "blue" | "bluegain" => Some(VcpCode::BlueGain),
        "sharpness" => Some(VcpCode::Sharpness),
        _ => None,
    };
    if by_name.is_some() {
        return by_name;
    }
    let n = if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u8::from_str_radix(hex, 16).ok()?
    } else {
        t.parse::<u8>().ok()?
    };
    Some(VcpCode::from_code(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_roundtrips_through_from_code() {
        for c in 0u8..=255 {
            assert_eq!(
                VcpCode::from_code(c).code(),
                c,
                "roundtrip failed for {c:#04x}"
            );
        }
    }

    /// A known code must never fall through to Raw, or it would print as
    /// "Unknown" and lose its value kind.
    #[test]
    fn known_codes_do_not_decode_as_raw() {
        for c in [
            0x10, 0x12, 0x14, 0x16, 0x18, 0x1A, 0x60, 0x62, 0x63, 0x87, 0x8D, 0xCC, 0xD6,
        ] {
            assert!(
                !matches!(VcpCode::from_code(c), VcpCode::Raw(_)),
                "{c:#04x} decoded as Raw"
            );
        }
    }

    /// Unknown codes must render as their number, not "Unknown".
    #[test]
    fn raw_codes_display_as_their_number() {
        assert_eq!(VcpCode::Raw(0xE1).display_name(), "VCP 0xe1");
        assert_eq!(VcpCode::Brightness.display_name(), "Brightness");
    }

    #[test]
    fn input_and_power_are_flagged_destructive() {
        assert!(VcpCode::InputSource.is_destructive());
        assert!(VcpCode::PowerMode.is_destructive());
        assert!(!VcpCode::Brightness.is_destructive());
    }

    /// Vendor-specific inputs must stay unnamed rather than be guessed at.
    /// Observed on the dev bench: MB169CK advertises 0x1A and 0x1B.
    #[test]
    fn input_sources_above_the_standard_range_are_not_named() {
        assert_eq!(input_source_name(0x11), Some("HDMI-1"));
        assert_eq!(input_source_name(0x12), Some("HDMI-2"));
        for v in 0x13..=0xFF {
            assert_eq!(input_source_name(v), None, "{v:#04x} must not be named");
        }
    }

    #[test]
    fn parses_names_and_numbers() {
        assert_eq!(parse_code("brightness"), Some(VcpCode::Brightness));
        assert_eq!(parse_code("Input-Source"), Some(VcpCode::InputSource));
        assert_eq!(parse_code("0x10"), Some(VcpCode::Brightness));
        assert_eq!(parse_code("16"), Some(VcpCode::Brightness));
        assert_eq!(parse_code("0xE1"), Some(VcpCode::Raw(0xE1)));
        assert_eq!(parse_code("nonsense"), None);
        assert_eq!(parse_code("999"), None);
    }
}
