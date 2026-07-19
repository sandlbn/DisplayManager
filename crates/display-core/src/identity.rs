//! Stable display identity, used to key persisted settings and profiles.

use std::fmt;

/// Serials that monitors report instead of an actual serial number.
///
/// The ASUS MB169CK on the dev bench reports `0x01010101`. A vendor that ships
/// one placeholder ships it on every unit, so two identical monitors would be
/// indistinguishable by serial — which is exactly the case the plan's
/// "two identical monitors" risk is about.
const PLACEHOLDER_SERIALS: &[i64] = &[
    0,
    0x0101_0101, // 16843009 — observed on ASUS MB169CK
    0x0000_0001,
    0xFFFF_FFFFu32 as i64,
];

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct DisplayIdentity {
    /// EDID manufacturer ID, e.g. "AUS".
    pub vendor: String,
    pub product_name: String,
    pub serial: i64,
    pub alphanumeric_serial: String,
    /// IOKit registry path of the framebuffer. Reflects the physical port.
    pub location: String,
}

impl DisplayIdentity {
    /// Whether `serial` distinguishes this unit from another of the same model.
    ///
    /// False for placeholders, so callers must fall back to physical location.
    pub fn has_trustworthy_serial(&self) -> bool {
        if PLACEHOLDER_SERIALS.contains(&self.serial) {
            return false;
        }
        // A serial whose bytes all repeat (0x02020202, ...) is a filler pattern,
        // not a real serial.
        let b = (self.serial as u32).to_le_bytes();
        if b[0] == b[1] && b[1] == b[2] && b[2] == b[3] {
            return false;
        }
        true
    }

    /// Key for persisting per-monitor settings.
    ///
    /// Prefers the serial, but falls back to physical location when the serial
    /// is a placeholder — settings then follow the *port* rather than the panel,
    /// which is the best available behaviour for such hardware.
    pub fn settings_key(&self) -> String {
        if self.has_trustworthy_serial() {
            format!("{}:{}:{}", self.vendor, self.product_name, self.serial)
        } else if !self.alphanumeric_serial.is_empty() {
            format!(
                "{}:{}:alnum:{}",
                self.vendor, self.product_name, self.alphanumeric_serial
            )
        } else {
            format!(
                "{}:{}:loc:{}",
                self.vendor, self.product_name, self.location
            )
        }
    }
}

impl fmt::Display for DisplayIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.product_name.is_empty() {
            write!(f, "{} (unnamed)", self.vendor)
        } else {
            write!(f, "{} {}", self.vendor, self.product_name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(serial: i64) -> DisplayIdentity {
        DisplayIdentity {
            vendor: "AUS".into(),
            product_name: "MB169CK".into(),
            serial,
            location: "/IOService/foo@1".into(),
            ..Default::default()
        }
    }

    #[test]
    fn observed_placeholder_serial_is_not_trusted() {
        // Value read from real hardware during the Phase 0 spike.
        assert_eq!(16843009, 0x0101_0101);
        assert!(!ident(16843009).has_trustworthy_serial());
    }

    #[test]
    fn zero_and_repeated_byte_serials_are_not_trusted() {
        assert!(!ident(0).has_trustworthy_serial());
        assert!(!ident(0x0202_0202).has_trustworthy_serial());
        assert!(!ident(0xFFFF_FFFFu32 as i64).has_trustworthy_serial());
    }

    #[test]
    fn genuine_serial_is_trusted() {
        assert!(ident(0x1A2B_3C4D).has_trustworthy_serial());
    }

    #[test]
    fn untrustworthy_serial_falls_back_to_location_not_serial() {
        let key = ident(16843009).settings_key();
        assert!(
            key.contains("loc:"),
            "expected location fallback, got {key}"
        );
        assert!(!key.contains("16843009"));
    }

    /// Two identical units on different ports must not collide.
    #[test]
    fn placeholder_serial_units_are_distinguished_by_port() {
        let mut a = ident(16843009);
        let mut b = ident(16843009);
        a.location = "/IOService/port@1".into();
        b.location = "/IOService/port@2".into();
        assert_ne!(a.settings_key(), b.settings_key());
    }

    #[test]
    fn alphanumeric_serial_preferred_over_location() {
        let mut d = ident(16843009);
        d.alphanumeric_serial = "R3LMTF001234".into();
        assert!(d.settings_key().contains("alnum:R3LMTF001234"));
    }
}
