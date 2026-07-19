//! Runtime bindings to the private IOAVService / DisplayServices entry points.
//!
//! These symbols are not in any SDK header, so they are resolved with `dlsym` at
//! startup rather than linked. If a macOS update withdraws one, `Private::load`
//! returns an error instead of the process failing to launch.

use core_foundation_sys::base::{CFAllocatorRef, CFTypeRef};
use core_foundation_sys::dictionary::CFDictionaryRef;
use std::ffi::{c_char, c_void, CString};

pub type IOAVServiceRef = CFTypeRef;
pub type IoObject = u32;
pub type KernReturn = i32;

pub const KERN_SUCCESS: KernReturn = 0;
pub const IO_OBJECT_NULL: IoObject = 0;
pub const K_IO_REGISTRY_ITERATE_RECURSIVELY: u32 = 1;

/// `io_name_t` and `io_string_t` from <device/device_types.h>.
pub const IO_NAME_LEN: usize = 128;
pub const IO_STRING_LEN: usize = 512;

// IOKit proper is public API, so it links normally.
#[link(name = "IOKit", kind = "framework")]
extern "C" {
    pub fn IORegistryGetRootEntry(main_port: u32) -> IoObject;
    pub fn IORegistryEntryCreateIterator(
        entry: IoObject,
        plane: *const c_char,
        options: u32,
        iterator: *mut IoObject,
    ) -> KernReturn;
    pub fn IOIteratorNext(iterator: IoObject) -> IoObject;
    pub fn IORegistryEntryGetName(entry: IoObject, name: *mut c_char) -> KernReturn;
    pub fn IORegistryEntryGetPath(
        entry: IoObject,
        plane: *const c_char,
        path: *mut c_char,
    ) -> KernReturn;
    pub fn IORegistryEntryCreateCFProperty(
        entry: IoObject,
        key: CFTypeRef,
        allocator: CFAllocatorRef,
        options: u32,
    ) -> CFTypeRef;
    pub fn IOObjectRelease(object: IoObject) -> KernReturn;

    // Public API from IOPowerSources.h.
    pub fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    pub fn IOPSGetProvidingPowerSourceType(
        blob: CFTypeRef,
    ) -> core_foundation_sys::string::CFStringRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    pub fn CGGetOnlineDisplayList(
        max_displays: u32,
        online_displays: *mut u32,
        display_count: *mut u32,
    ) -> i32;
    pub fn CGDisplayIsBuiltin(display: u32) -> i32;
}

const CORE_DISPLAY: &str = "/System/Library/Frameworks/CoreDisplay.framework/CoreDisplay";
const DISPLAY_SERVICES: &str =
    "/System/Library/PrivateFrameworks/DisplayServices.framework/DisplayServices";

type FnCreateWithService = unsafe extern "C" fn(CFAllocatorRef, IoObject) -> IOAVServiceRef;
type FnReadI2c = unsafe extern "C" fn(IOAVServiceRef, u32, u32, *mut c_void, u32) -> KernReturn;
type FnWriteI2c = unsafe extern "C" fn(IOAVServiceRef, u32, u32, *const c_void, u32) -> KernReturn;
type FnDisplayInfo = unsafe extern "C" fn(u32) -> CFDictionaryRef;
type FnGetBrightness = unsafe extern "C" fn(u32, *mut f32) -> KernReturn;
type FnSetBrightness = unsafe extern "C" fn(u32, f32) -> KernReturn;

/// Private entry points resolved at runtime.
pub struct Private {
    pub av_create: FnCreateWithService,
    pub av_read: FnReadI2c,
    pub av_write: FnWriteI2c,
    pub display_info: FnDisplayInfo,
    /// Absent on some configurations; the built-in path degrades rather than fails.
    pub get_brightness: Option<FnGetBrightness>,
    pub set_brightness: Option<FnSetBrightness>,
}

unsafe fn dlopen(path: &str) -> Result<*mut c_void, String> {
    let c = CString::new(path).unwrap();
    let h = libc::dlopen(c.as_ptr(), libc::RTLD_LAZY);
    if h.is_null() {
        return Err(format!("dlopen failed: {path}"));
    }
    Ok(h)
}

unsafe fn sym(handle: *mut c_void, name: &str) -> Option<*mut c_void> {
    let c = CString::new(name).unwrap();
    let p = libc::dlsym(handle, c.as_ptr());
    if p.is_null() {
        None
    } else {
        Some(p)
    }
}

macro_rules! require {
    ($h:expr, $name:literal) => {
        match sym($h, $name) {
            Some(p) => std::mem::transmute(p),
            None => return Err(format!("missing symbol: {}", $name)),
        }
    };
}

impl Private {
    pub fn load() -> Result<Self, String> {
        unsafe {
            let cd = dlopen(CORE_DISPLAY)?;
            let ds = dlopen(DISPLAY_SERVICES).ok();

            Ok(Private {
                av_create: require!(cd, "IOAVServiceCreateWithService"),
                av_read: require!(cd, "IOAVServiceReadI2C"),
                av_write: require!(cd, "IOAVServiceWriteI2C"),
                display_info: require!(cd, "CoreDisplay_DisplayCreateInfoDictionary"),
                get_brightness: ds
                    .and_then(|h| sym(h, "DisplayServicesGetBrightness"))
                    .map(|p| std::mem::transmute(p)),
                set_brightness: ds
                    .and_then(|h| sym(h, "DisplayServicesSetBrightness"))
                    .map(|p| std::mem::transmute(p)),
            })
        }
    }
}
