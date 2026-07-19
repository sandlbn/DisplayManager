//! Pairing `CGDirectDisplayID`s with IORegistry AV services.
//!
//! This is the correctness problem the plan calls out: with two identical
//! monitors, a mis-match means the user's input-source change lands on the wrong
//! panel and they lose picture with no way to undo it.
//!
//! Ported from `Arm64DDC.swift`'s `ioregMatchScore`. The weighting is the whole
//! point: physical location scores **10** while every identity signal scores
//! **1**, so the port a monitor is plugged into outranks anything it claims
//! about itself. The dev bench proves why — the ASUS MB169CK reports serial
//! `0x01010101`, a placeholder, so two of them would be identical on every
//! identity signal and separable only by location.

use crate::ioreg::IoregService;
use crate::sys::*;
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use std::ffi::c_void;

type RawDict = CFDictionary<*const c_void, *const c_void>;

/// Ceiling on `match_score`, used as the top of the greedy assignment sweep.
pub const MAX_MATCH_SCORE: i32 = 20;

/// Weight for a physical-location match. Deliberately larger than the sum of
/// all identity signals so location wins outright.
const LOCATION_WEIGHT: i32 = 10;

fn dict_get(d: &RawDict, key: &str) -> Option<CFType> {
    let k = CFString::new(key);
    d.find(k.as_CFTypeRef() as *const c_void)
        .map(|v| unsafe { CFType::wrap_under_get_rule(*v as _) })
}

fn dict_i64(d: &RawDict, key: &str) -> Option<i64> {
    dict_get(d, key)
        .and_then(|v| v.downcast::<CFNumber>())
        .and_then(|n| n.to_i64())
}

fn dict_string(d: &RawDict, key: &str) -> Option<String> {
    dict_get(d, key)
        .and_then(|v| v.downcast::<CFString>())
        .map(|s| s.to_string())
}

/// CoreDisplay's info dictionary for a display, or None if unavailable.
fn display_info(p: &Private, display_id: u32) -> Option<RawDict> {
    unsafe {
        let raw = (p.display_info)(display_id);
        if raw.is_null() {
            return None;
        }
        Some(RawDict::wrap_under_create_rule(raw))
    }
}

/// The localized product name, preferring en_US then any entry.
fn product_name(info: &RawDict) -> Option<String> {
    let names = dict_get(info, "DisplayProductName")?.downcast::<RawDict>()?;
    if let Some(n) = dict_string(&names, "en_US") {
        return Some(n);
    }
    let (_, values) = names.get_keys_and_values();
    values
        .first()
        .map(|v| unsafe { CFType::wrap_under_get_rule(*v as _) })
        .and_then(|v| v.downcast::<CFString>())
        .map(|s| s.to_string())
}

/// The 4 characters of `uuid` at `loc`, if present.
///
/// EDID UUIDs embed fields at fixed character offsets. Indexes by `char` to
/// mirror Swift's `prefix`/`suffix`, and returns None rather than panicking on a
/// short or absent UUID.
fn uuid_field(uuid: &str, loc: usize) -> Option<String> {
    let chars: Vec<char> = uuid.chars().collect();
    if chars.len() < loc + 4 {
        return None;
    }
    Some(chars[loc..loc + 4].iter().collect())
}

/// Score how well `service` matches `display_id`. Higher is better; 0 means no
/// evidence at all and must not be matched.
pub fn match_score(p: &Private, display_id: u32, service: &IoregService) -> i32 {
    let Some(info) = display_info(p, display_id) else {
        return 0;
    };
    let mut score = 0;

    // EDID UUID fields, +1 each.
    if let (Some(vendor), Some(product), Some(year), Some(week), Some(h), Some(v)) = (
        dict_i64(&info, "DisplayVendorID"),
        dict_i64(&info, "DisplayProductID"),
        dict_i64(&info, "YearOfManufacture"),
        dict_i64(&info, "WeekOfManufacture"),
        dict_i64(&info, "DisplayHorizontalImageSize"),
        dict_i64(&info, "DisplayVerticalImageSize"),
    ) {
        let vendor16 = vendor.clamp(0, 0xFFFF) as u16;
        let product16 = product.clamp(0, 0xFFFF) as u16;

        let keys: [(String, usize); 4] = [
            // Vendor ID, big-endian.
            (format!("{vendor16:04X}"), 0),
            // Product ID, byte-swapped relative to vendor. Not a typo: EDID
            // stores it little-endian.
            (
                format!(
                    "{:02X}{:02X}",
                    (product16 & 0xFF) as u8,
                    (product16 >> 8) as u8
                ),
                4,
            ),
            // Manufacture week/year, year offset from 1990.
            (
                format!(
                    "{:02X}{:02X}",
                    week.clamp(0, 255) as u8,
                    (year - 1990).clamp(0, 255) as u8
                ),
                19,
            ),
            // Physical image size in cm.
            (
                format!(
                    "{:02X}{:02X}",
                    (h / 10).clamp(0, 255) as u8,
                    (v / 10).clamp(0, 255) as u8
                ),
                30,
            ),
        ];

        for (key, loc) in keys {
            // "0000" is the absent-field sentinel and matches everything, so it
            // must never count as evidence.
            if key == "0000" {
                continue;
            }
            if uuid_field(&service.edid_uuid, loc).as_deref() == Some(key.as_str()) {
                score += 1;
            }
        }
    }

    // Physical port. Outranks identity by design.
    if !service.io_display_location.is_empty()
        && dict_string(&info, "IODisplayLocation").as_deref()
            == Some(service.io_display_location.as_str())
    {
        score += LOCATION_WEIGHT;
    }

    if !service.product_name.is_empty()
        && product_name(&info)
            .map(|n| n.eq_ignore_ascii_case(&service.product_name))
            .unwrap_or(false)
    {
        score += 1;
    }

    if service.serial_number != 0
        && dict_i64(&info, "DisplaySerialNumber") == Some(service.serial_number)
    {
        score += 1;
    }

    score
}

#[derive(Debug, Clone)]
pub struct Match {
    pub display_id: u32,
    pub service: IoregService,
    pub score: i32,
}

/// Assign displays to services, best scores first.
///
/// Greedy over descending score, and each display and each service may be taken
/// only once — so a confident pairing removes both from contention before a
/// weaker one is considered. Candidates scoring 0 are never matched: no evidence
/// is better than a wrong guess when the failure mode is a black screen.
pub fn assign(p: &Private, display_ids: &[u32], services: &[IoregService]) -> Vec<Match> {
    let mut candidates: Vec<Match> = Vec::new();
    for &display_id in display_ids {
        for service in services {
            candidates.push(Match {
                display_id,
                service: service.clone(),
                score: match_score(p, display_id, service),
            });
        }
    }

    let mut taken_displays: Vec<u32> = Vec::new();
    let mut taken_locations: Vec<usize> = Vec::new();
    let mut out = Vec::new();

    for score in (1..=MAX_MATCH_SCORE).rev() {
        for c in candidates.iter().filter(|c| c.score == score) {
            if taken_displays.contains(&c.display_id)
                || taken_locations.contains(&c.service.service_location)
            {
                continue;
            }
            taken_displays.push(c.display_id);
            taken_locations.push(c.service.service_location);
            out.push(c.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_field_extracts_by_character_offset() {
        let uuid = "0610AF00-0000-0000-1D1C-0104B53C2278";
        assert_eq!(uuid_field(uuid, 0).as_deref(), Some("0610"));
        assert_eq!(uuid_field(uuid, 4).as_deref(), Some("AF00"));
    }

    /// Must not panic on a short or missing UUID — plenty of displays have none.
    #[test]
    fn uuid_field_handles_short_input() {
        assert_eq!(uuid_field("", 0), None);
        assert_eq!(uuid_field("06", 0), None);
        assert_eq!(uuid_field("0610AF00", 30), None);
    }

    /// The core invariant: location must outweigh every identity signal
    /// combined, so identical panels on different ports stay distinguishable.
    #[test]
    fn location_outweighs_all_identity_signals_combined() {
        let identity_max = 4 /* uuid fields */ + 1 /* name */ + 1 /* serial */;
        assert!(
            LOCATION_WEIGHT > identity_max,
            "location weight {LOCATION_WEIGHT} must exceed identity total {identity_max}"
        );
        assert!(MAX_MATCH_SCORE >= LOCATION_WEIGHT + identity_max);
    }
}
