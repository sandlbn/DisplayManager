//! Keyboard brightness-key interception.
//!
//! Taps the system-defined media keys (the brightness keys) and, when the
//! cursor is over an external DDC display, adjusts that display's brightness and
//! swallows the event so macOS does not also move the built-in panel. Over the
//! built-in (or any display we do not control), the key passes straight through
//! to the OS.
//!
//! Requires the Accessibility permission — macOS only lets a *trusted* process
//! consume key events. Without it the tap cannot be created; we prompt once and
//! carry on without the feature.
//!
//! Brightness only for now. Volume/mute are left to the OS: audio output is
//! rarely the external monitor, so intercepting those usually does the wrong
//! thing.

use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::statusbar::Handler;

// ── CoreFoundation / CoreGraphics geometry ──────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

type CFTypeRef = *const c_void;
type CGEventRef = *mut c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFRunLoopRef = *mut c_void;

/// Opaque CGEvent carrying the Objective-C type encoding AppKit expects for
/// `eventWithCGEvent:` (`^{__CGEvent=}`). Passing a bare `*const c_void` fails
/// objc2's runtime type check; a pointer to this type encodes correctly.
#[repr(C)]
struct CGEventOpaque {
    _private: [u8; 0],
}
unsafe impl objc2::RefEncode for CGEventOpaque {
    const ENCODING_REF: objc2::Encoding =
        objc2::Encoding::Pointer(&objc2::Encoding::Struct("__CGEvent", &[]));
}

/// The tap callback: `(proxy, event type, event, user info) -> event or null`.
type TapCallback = extern "C" fn(*mut c_void, u32, CGEventRef, *mut c_void) -> CGEventRef;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: TapCallback,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventCreate(source: CFTypeRef) -> CGEventRef;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGGetDisplaysWithPoint(
        point: CGPoint,
        max_displays: u32,
        displays: *mut u32,
        count: *mut u32,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortCreateRunLoopSource(
        allocator: CFTypeRef,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetMain() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFTypeRef);
    fn CFRelease(cf: CFTypeRef);
    static kCFRunLoopCommonModes: CFTypeRef;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> bool;
    static kAXTrustedCheckOptionPrompt: CFTypeRef;
}

// System-defined event type and the tap-disabled sentinels.
const NX_SYSDEFINED: u32 = 14;
const TAP_DISABLED_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_USER_INPUT: u32 = 0xFFFF_FFFF;

// NX aux-button subtype and key codes carried in the NSEvent.
const NX_SUBTYPE_AUX_CONTROL: i16 = 8;
const NX_KEYTYPE_BRIGHTNESS_UP: i64 = 2;
const NX_KEYTYPE_BRIGHTNESS_DOWN: i64 = 3;
const NX_KEYDOWN: i64 = 0x0A;

/// The tap port, kept so the callback can re-enable it if macOS disables it
/// (which happens after a slow callback or on some user input).
static TAP: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Install the brightness-key tap. No-op (with a log line) if Accessibility is
/// not granted. `handler` must outlive the process — the caller leaks it.
pub fn install(handler: &Handler) {
    if !accessibility_trusted() {
        eprintln!(
            "display-gui: brightness keys need Accessibility permission. \
             Grant it in System Settings › Privacy & Security › Accessibility, then relaunch."
        );
        // Still fall through: on first run the prompt was shown; the tap create
        // will simply fail and we continue without the feature.
    }

    let user_info = handler as *const Handler as *mut c_void;
    let tap = unsafe {
        CGEventTapCreate(
            1, // kCGSessionEventTap
            0, // kCGHeadInsertEventTap
            0, // kCGEventTapOptionDefault (active — may consume)
            1u64 << NX_SYSDEFINED,
            tap_callback,
            user_info,
        )
    };
    if tap.is_null() {
        eprintln!(
            "display-gui: could not create the brightness-key tap (Accessibility not granted)."
        );
        return;
    }
    TAP.store(tap, Ordering::SeqCst);

    unsafe {
        let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
        CFRunLoopAddSource(CFRunLoopGetMain(), source, kCFRunLoopCommonModes);
        CGEventTapEnable(tap, true);
        CFRelease(source as CFTypeRef);
    }
}

/// Ask (and prompt) whether this process is trusted for Accessibility.
fn accessibility_trusted() -> bool {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;

    unsafe {
        // { kAXTrustedCheckOptionPrompt: true } shows the system prompt if not
        // yet granted.
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt as _);
        let options = CFDictionary::from_CFType_pairs(&[(
            key.as_CFType(),
            CFBoolean::true_value().as_CFType(),
        )]);
        AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef() as CFTypeRef)
    }
}

/// The `CGDirectDisplayID` under the mouse cursor, if any.
///
/// Uses the event system's own cursor position (top-left global coordinates),
/// which avoids the Cocoa/CoreGraphics Y-flip that `NSEvent.mouseLocation`
/// would introduce.
pub fn display_under_cursor() -> Option<u32> {
    unsafe {
        let ev = CGEventCreate(std::ptr::null());
        if ev.is_null() {
            return None;
        }
        let point = CGEventGetLocation(ev);
        CFRelease(ev as CFTypeRef);

        let mut ids = [0u32; 8];
        let mut count = 0u32;
        if CGGetDisplaysWithPoint(point, 8, ids.as_mut_ptr(), &mut count) != 0 || count == 0 {
            return None;
        }
        Some(ids[0])
    }
}

/// The event tap callback. Runs on the main run loop, so objc calls are safe.
///
/// A panic here would cross the C boundary and abort the process, so the body is
/// isolated in `catch_unwind`: on panic we pass the event through untouched.
extern "C" fn tap_callback(
    proxy: *mut c_void,
    etype: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_event(proxy, etype, event, user_info)
    }));
    out.unwrap_or(event)
}

fn handle_event(
    _proxy: *mut c_void,
    etype: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    // macOS disabled the tap (slow callback or user input) — re-enable and move on.
    if etype == TAP_DISABLED_TIMEOUT || etype == TAP_DISABLED_USER_INPUT {
        let tap = TAP.load(Ordering::SeqCst);
        if !tap.is_null() {
            unsafe { CGEventTapEnable(tap, true) };
        }
        return event;
    }
    if etype != NX_SYSDEFINED || user_info.is_null() {
        return event;
    }

    // Decode the system-defined event via NSEvent (it carries the key in data1).
    let Some((code, key_down)) = decode(event) else {
        return event;
    };
    if !key_down {
        return event; // act on press only; let key-up through
    }
    let up = match code {
        NX_KEYTYPE_BRIGHTNESS_UP => true,
        NX_KEYTYPE_BRIGHTNESS_DOWN => false,
        _ => return event, // not a brightness key
    };

    let handler = unsafe { &*(user_info as *const Handler) };
    if handler.media_brightness(up) {
        std::ptr::null_mut() // handled it — swallow the event
    } else {
        event // cursor is on the built-in / a display we don't drive — pass through
    }
}

/// Extract (key code, is-key-down) from a system-defined CGEvent, or None if it
/// is not an aux-control-button event.
fn decode(event: CGEventRef) -> Option<(i64, bool)> {
    unsafe {
        let ns: *mut AnyObject =
            msg_send![class!(NSEvent), eventWithCGEvent: event as *mut CGEventOpaque];
        if ns.is_null() {
            return None;
        }
        let subtype: i16 = msg_send![ns, subtype];
        if subtype != NX_SUBTYPE_AUX_CONTROL {
            return None;
        }
        let data1: i64 = msg_send![ns, data1];
        let code = (data1 & 0xFFFF_0000) >> 16;
        let state = (data1 & 0x0000_FF00) >> 8;
        Some((code, state == NX_KEYDOWN))
    }
}
