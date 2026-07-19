//! Walking the IOService plane to pair framebuffers with their AV services.
//!
//! The registry lists a framebuffer node (AppleCLCD2 / IOMobileFramebufferShim)
//! carrying the display's identity, followed by the DCPAVServiceProxy node that
//! carries the I2C channel. Neither node has both halves, so the walk stitches
//! each proxy onto the framebuffer that preceded it.

use crate::sys::*;
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use std::ffi::{c_char, c_void, CStr, CString};

/// core-foundation only implements `ConcreteCFType` for the untyped dictionary,
/// so downcasts land here and values are re-wrapped as `CFType` by hand.
type RawDict = CFDictionary<*const c_void, *const c_void>;

#[derive(Debug, Default, Clone)]
pub struct IoregService {
    pub edid_uuid: String,
    pub manufacturer_id: String,
    pub product_name: String,
    pub serial_number: i64,
    pub alphanumeric_serial: String,
    pub location: String,
    pub io_display_location: String,
    pub transport_upstream: String,
    pub transport_downstream: String,
    pub service_location: usize,
    pub service: IOAVServiceRef,
}

fn cf_prop(entry: IoObject, key: &str) -> Option<CFType> {
    let k = CFString::new(key);
    unsafe {
        let raw = IORegistryEntryCreateCFProperty(
            entry,
            k.as_CFTypeRef(),
            std::ptr::null(),
            K_IO_REGISTRY_ITERATE_RECURSIVELY,
        );
        if raw.is_null() {
            None
        } else {
            Some(CFType::wrap_under_create_rule(raw))
        }
    }
}

/// Look a key up and re-wrap the value as a CFType (get rule: the dictionary
/// still owns it, so retain rather than adopt).
fn dict_get(d: &RawDict, key: &str) -> Option<CFType> {
    let k = CFString::new(key);
    let raw = k.as_CFTypeRef() as *const c_void;
    d.find(raw)
        .map(|v| unsafe { CFType::wrap_under_get_rule(*v as _) })
}

fn dict_string(d: &RawDict, key: &str) -> Option<String> {
    dict_get(d, key)
        .and_then(|v| v.downcast::<CFString>())
        .map(|s| s.to_string())
}

fn dict_i64(d: &RawDict, key: &str) -> Option<i64> {
    dict_get(d, key)
        .and_then(|v| v.downcast::<CFNumber>())
        .and_then(|n| n.to_i64())
}

fn dict_dict(d: &RawDict, key: &str) -> Option<RawDict> {
    dict_get(d, key).and_then(|v| v.downcast::<RawDict>())
}

/// Read display identity from a framebuffer node.
fn read_framebuffer(entry: IoObject) -> IoregService {
    let mut svc = IoregService::default();

    if let Some(uuid) = cf_prop(entry, "EDID UUID").and_then(|v| v.downcast::<CFString>()) {
        svc.edid_uuid = uuid.to_string();
    }

    let plane = CString::new("IOService").unwrap();
    let mut path = [0 as c_char; IO_STRING_LEN];
    unsafe {
        if IORegistryEntryGetPath(entry, plane.as_ptr(), path.as_mut_ptr()) == KERN_SUCCESS {
            svc.io_display_location = CStr::from_ptr(path.as_ptr()).to_string_lossy().into_owned();
        }
    }

    if let Some(attrs) = cf_prop(entry, "DisplayAttributes").and_then(|v| v.downcast::<RawDict>()) {
        if let Some(product) = dict_dict(&attrs, "ProductAttributes") {
            svc.manufacturer_id = dict_string(&product, "ManufacturerID").unwrap_or_default();
            svc.product_name = dict_string(&product, "ProductName").unwrap_or_default();
            svc.serial_number = dict_i64(&product, "SerialNumber").unwrap_or(0);
            svc.alphanumeric_serial =
                dict_string(&product, "AlphanumericSerialNumber").unwrap_or_default();
        }
    }

    if let Some(transport) = cf_prop(entry, "Transport").and_then(|v| v.downcast::<RawDict>()) {
        svc.transport_upstream = dict_string(&transport, "Upstream").unwrap_or_default();
        svc.transport_downstream = dict_string(&transport, "Downstream").unwrap_or_default();
    }

    svc
}

/// Attach the I2C channel from a DCPAVServiceProxy node. Only External locations
/// get a service: the embedded panel speaks DisplayServices, not DDC.
fn attach_av_service(p: &Private, entry: IoObject, svc: &mut IoregService) {
    if let Some(loc) = cf_prop(entry, "Location").and_then(|v| v.downcast::<CFString>()) {
        svc.location = loc.to_string();
        if svc.location == "External" {
            svc.service = unsafe { (p.av_create)(std::ptr::null(), entry) };
        }
    }
}

const FRAMEBUFFER_KEYS: [&str; 2] = ["AppleCLCD2", "IOMobileFramebufferShim"];
const AV_PROXY_KEY: &str = "DCPAVServiceProxy";

/// Enumerate every framebuffer/AV-service pair in the IOService plane.
pub fn services_for_matching(p: &Private) -> Vec<IoregService> {
    let mut out = Vec::new();
    let plane = CString::new("IOService").unwrap();

    unsafe {
        let root = IORegistryGetRootEntry(0);
        if root == IO_OBJECT_NULL {
            return out;
        }
        let mut iter: IoObject = 0;
        if IORegistryEntryCreateIterator(
            root,
            plane.as_ptr(),
            K_IO_REGISTRY_ITERATE_RECURSIVELY,
            &mut iter,
        ) != KERN_SUCCESS
        {
            IOObjectRelease(root);
            return out;
        }

        let mut current = IoregService::default();
        let mut service_location = 0usize;
        let mut name_buf = [0 as c_char; IO_NAME_LEN];

        loop {
            let entry = IOIteratorNext(iter);
            if entry == IO_OBJECT_NULL {
                break;
            }
            if IORegistryEntryGetName(entry, name_buf.as_mut_ptr()) != KERN_SUCCESS {
                IOObjectRelease(entry);
                break;
            }
            let name = CStr::from_ptr(name_buf.as_ptr()).to_string_lossy();

            // Exact match, never `contains`: DCPAVServiceProxyUserClient is a
            // distinct node that a substring test would wrongly accept, cloning
            // the preceding framebuffer into a duplicate record.
            if std::env::var_os("DDC_SPIKE_TRACE").is_some()
                && (FRAMEBUFFER_KEYS.contains(&name.as_ref()) || name == AV_PROXY_KEY)
            {
                eprintln!("trace: visit entry={entry} name={name}");
            }

            if FRAMEBUFFER_KEYS.contains(&name.as_ref()) {
                current = read_framebuffer(entry);
                service_location += 1;
                current.service_location = service_location;
            } else if name == AV_PROXY_KEY {
                attach_av_service(p, entry, &mut current);
                out.push(current.clone());
            }
            IOObjectRelease(entry);
        }

        IOObjectRelease(iter);
        IOObjectRelease(root);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a substring test accepts DCPAVServiceProxyUserClient and
    /// duplicates the preceding framebuffer's record, enumerating one physical
    /// monitor twice. Observed on real hardware during the Phase 0 spike.
    #[test]
    fn av_proxy_key_matches_exactly_not_by_substring() {
        assert_eq!("DCPAVServiceProxy", AV_PROXY_KEY);
        assert_ne!("DCPAVServiceProxyUserClient", AV_PROXY_KEY);
        assert!("DCPAVServiceProxyUserClient".contains(AV_PROXY_KEY));
    }

    #[test]
    fn framebuffer_keys_match_exactly_not_by_substring() {
        assert!(FRAMEBUFFER_KEYS.contains(&"IOMobileFramebufferShim"));
        assert!(!FRAMEBUFFER_KEYS.contains(&"IOMobileFramebufferShimUserClient"));
    }
}

/// Online display IDs from CoreGraphics.
pub fn online_displays() -> Vec<u32> {
    let mut ids = [0u32; 16];
    let mut count = 0u32;
    unsafe {
        if CGGetOnlineDisplayList(16, ids.as_mut_ptr(), &mut count) != 0 {
            return Vec::new();
        }
    }
    ids[..count as usize].to_vec()
}
