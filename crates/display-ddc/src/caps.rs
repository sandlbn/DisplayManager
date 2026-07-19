//! Capability string parsing.
//!
//! Format: `(prot(monitor)type(lcd)model(X)cmds(01 02)vcp(10 12 60(0F 11))…)`.
//!
//! Real monitors emit malformed strings — unbalanced parens, stray whitespace,
//! junk sections, truncated tails. A capability string is *informational*, so
//! the parser is deliberately tolerant: it salvages what it can and never fails
//! on structure alone. Refusing to parse would mean refusing to control a
//! monitor that otherwise works fine.

use crate::vcp::VcpCode;
use crate::{Error, Result};

/// One entry from the `vcp(...)` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcpFeature {
    pub code: VcpCode,
    /// Permitted values for non-continuous codes; empty for continuous ones.
    pub values: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub raw: String,
    pub protocol: Option<String>,
    pub display_type: Option<String>,
    pub model: Option<String>,
    pub mccs_version: Option<String>,
    pub commands: Vec<u8>,
    pub vcp: Vec<VcpFeature>,
    /// Sections that were present but not understood. Surfaced rather than
    /// dropped: unknown sections are exactly what the capability explorer and
    /// the crowdsourced DB want to collect.
    pub unknown_sections: Vec<String>,
}

impl Capabilities {
    pub fn supports(&self, code: VcpCode) -> bool {
        self.vcp.iter().any(|f| f.code == code)
    }

    pub fn feature(&self, code: VcpCode) -> Option<&VcpFeature> {
        self.vcp.iter().find(|f| f.code == code)
    }
}

/// Split `name(body)name2(body2)` into pairs, tracking nesting depth.
///
/// Unclosed sections are returned with whatever body was captured, so a
/// truncated string still yields its earlier sections.
fn split_sections(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut name = String::new();

    while i < b.len() {
        match b[i] {
            '(' => {
                let mut depth = 1;
                let start = i + 1;
                let mut j = start;
                while j < b.len() && depth > 0 {
                    match b[j] {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    if depth > 0 {
                        j += 1;
                    }
                }
                let body: String = b[start..j.min(b.len())].iter().collect();
                let key = name.trim().to_string();
                if !key.is_empty() {
                    out.push((key, body));
                }
                name.clear();
                i = j + 1;
            }
            ')' => {
                name.clear();
                i += 1;
            }
            c => {
                name.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Parse a hex byte list like `01 02 0C E3`, skipping unparseable tokens.
fn parse_hex_list(s: &str) -> Vec<u8> {
    s.split_whitespace()
        .filter_map(|t| u8::from_str_radix(t, 16).ok())
        .collect()
}

/// Parse the `vcp(...)` body: codes, each optionally followed by a value list.
fn parse_vcp_list(s: &str) -> Vec<VcpFeature> {
    let mut out: Vec<VcpFeature> = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut token = String::new();

    // A code is committed when whitespace or '(' ends its token; '(' additionally
    // attaches a value list to the code just committed.
    let flush = |token: &mut String, out: &mut Vec<VcpFeature>| {
        let t = token.trim();
        if !t.is_empty() {
            if let Ok(c) = u8::from_str_radix(t, 16) {
                out.push(VcpFeature {
                    code: VcpCode::from_code(c),
                    values: Vec::new(),
                });
            }
        }
        token.clear();
    };

    while i < chars.len() {
        match chars[i] {
            '(' => {
                flush(&mut token, &mut out);
                let mut depth = 1;
                let start = i + 1;
                let mut j = start;
                while j < chars.len() && depth > 0 {
                    match chars[j] {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    if depth > 0 {
                        j += 1;
                    }
                }
                let body: String = chars[start..j.min(chars.len())].iter().collect();
                if let Some(last) = out.last_mut() {
                    last.values = parse_hex_list(&body);
                }
                i = j + 1;
            }
            c if c.is_whitespace() => {
                flush(&mut token, &mut out);
                i += 1;
            }
            c => {
                token.push(c);
                i += 1;
            }
        }
    }
    flush(&mut token, &mut out);
    out
}

/// Parse a capability string.
///
/// Fails only if nothing meaningful could be recovered.
pub fn parse(raw: &str) -> Result<Capabilities> {
    let trimmed = raw.trim();
    // Strip the outer wrapper if present; tolerate its absence.
    let inner = trimmed
        .strip_prefix('(')
        .map(|s| s.strip_suffix(')').unwrap_or(s))
        .unwrap_or(trimmed);

    let mut caps = Capabilities {
        raw: raw.to_string(),
        ..Default::default()
    };

    for (name, body) in split_sections(inner) {
        match name.to_ascii_lowercase().as_str() {
            "prot" => caps.protocol = Some(body.trim().to_string()),
            "type" => caps.display_type = Some(body.trim().to_string()),
            "model" => caps.model = Some(body.trim().to_string()),
            "mccs_ver" => caps.mccs_version = Some(body.trim().to_string()),
            "cmds" => caps.commands = parse_hex_list(&body),
            "vcp" => caps.vcp = parse_vcp_list(&body),
            _ => caps.unknown_sections.push(name),
        }
    }

    if caps.vcp.is_empty() && caps.protocol.is_none() && caps.model.is_none() {
        return Err(Error::CapabilityParse(format!(
            "no recognisable sections in {raw:?}"
        )));
    }
    Ok(caps)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = "(prot(monitor)type(lcd)model(MB169CK)cmds(01 02 03 07 0C E3 F3)vcp(02 04 08 10 12 14(05 08 0B) 16 18 1A 60(0F 11 1B) AC AE B6 C6 C8 CC(02 03) DF)mccs_ver(2.1))";

    #[test]
    fn parses_a_realistic_string() {
        let c = parse(REAL).unwrap();
        assert_eq!(c.protocol.as_deref(), Some("monitor"));
        assert_eq!(c.display_type.as_deref(), Some("lcd"));
        assert_eq!(c.model.as_deref(), Some("MB169CK"));
        assert_eq!(c.mccs_version.as_deref(), Some("2.1"));
        assert_eq!(c.commands, vec![0x01, 0x02, 0x03, 0x07, 0x0C, 0xE3, 0xF3]);
        assert!(c.supports(VcpCode::Brightness));
        assert!(c.supports(VcpCode::Contrast));
        assert!(c.supports(VcpCode::InputSource));
        assert!(!c.supports(VcpCode::Volume));
    }

    #[test]
    fn attaches_value_lists_to_the_right_code() {
        let c = parse(REAL).unwrap();
        let input = c.feature(VcpCode::InputSource).unwrap();
        assert_eq!(input.values, vec![0x0F, 0x11, 0x1B]);
        // Brightness is continuous: no value list, and it must not inherit the
        // list belonging to a neighbouring code.
        assert!(c.feature(VcpCode::Brightness).unwrap().values.is_empty());
    }

    #[test]
    fn value_list_does_not_leak_to_following_code() {
        let c = parse("(vcp(14(05 08) 16))").unwrap();
        assert_eq!(c.feature(VcpCode::ColorPreset).unwrap().values, vec![5, 8]);
        assert!(c.feature(VcpCode::RedGain).unwrap().values.is_empty());
    }

    #[test]
    fn tolerates_missing_outer_parens() {
        let c = parse("prot(monitor)vcp(10 12)").unwrap();
        assert!(c.supports(VcpCode::Brightness));
    }

    /// Truncated mid-string: earlier sections must survive.
    #[test]
    fn tolerates_unbalanced_parens() {
        let c = parse("(prot(monitor)type(lcd)vcp(10 12").unwrap();
        assert_eq!(c.protocol.as_deref(), Some("monitor"));
        assert!(c.supports(VcpCode::Brightness));
    }

    #[test]
    fn tolerates_junk_tokens_in_vcp_list() {
        let c = parse("(vcp(10 ZZ 12 !! 60))").unwrap();
        assert!(c.supports(VcpCode::Brightness));
        assert!(c.supports(VcpCode::Contrast));
        assert!(c.supports(VcpCode::InputSource));
    }

    #[test]
    fn collects_unknown_sections_rather_than_dropping_them() {
        let c = parse("(prot(monitor)vcp(10)asciicode(42)vendorthing(x))").unwrap();
        assert!(c.unknown_sections.contains(&"asciicode".to_string()));
        assert!(c.unknown_sections.contains(&"vendorthing".to_string()));
    }

    #[test]
    fn extra_whitespace_is_harmless() {
        let c = parse("(  vcp( 10   12  )  )").unwrap();
        assert_eq!(c.vcp.len(), 2);
    }

    #[test]
    fn rejects_a_string_with_nothing_recoverable() {
        assert!(parse("garbage without sections").is_err());
        assert!(parse("").is_err());
    }

    /// Must terminate and not panic on adversarial input.
    #[test]
    fn does_not_hang_or_panic_on_pathological_input() {
        assert!(parse(&"(".repeat(500)).is_err());
        let _ = parse(&")".repeat(500));
        let _ = parse(&"(vcp(".repeat(200));
        let _ = parse("(vcp(10(((((");
    }
}
