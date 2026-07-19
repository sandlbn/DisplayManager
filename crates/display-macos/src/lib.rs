//! macOS backend: IOKit/IOAVService DDC, CoreGraphics enumeration, and the
//! built-in panel via DisplayServices.

pub mod ioreg;
pub mod matching;
pub mod sys;
pub mod transport;

use display_core::rules::PowerState;
use display_core::{
    ControlPath, DisplayBackend, DisplayId, DisplayIdentity, Error, Monitor, Result,
};
use display_ddc::protocol::DdcDevice;
use display_ddc::vcp::VcpCode;
use display_ddc::Timings;
use std::collections::HashMap;
use std::sync::Arc;
use sys::Private;
use transport::AvServiceTransport;

/// Full scale for built-in brightness, which DisplayServices reports as 0.0–1.0.
///
/// Presented on the same 0–100 scale DDC monitors use, so callers can drive any
/// display's brightness without branching on its control path.
const NATIVE_BRIGHTNESS_MAX: u16 = 100;

pub struct MacosBackend {
    private: Arc<Private>,
    /// Rebuilt on every `list()`; AV services do not survive hot-plug.
    devices: HashMap<DisplayId, DdcDevice<AvServiceTransport>>,
    /// How each known display is driven. Kept so `get_vcp`/`set_vcp` can route
    /// the built-in panel to DisplayServices transparently.
    paths: HashMap<DisplayId, ControlPath>,
    timings: Timings,
}

impl MacosBackend {
    pub fn new() -> Result<Self> {
        let private = Private::load().map_err(Error::Transport)?;
        Ok(MacosBackend {
            private: Arc::new(private),
            devices: HashMap::new(),
            paths: HashMap::new(),
            timings: Timings::default(),
        })
    }

    /// Built-in panel brightness as 0.0–1.0, if DisplayServices is available.
    pub fn builtin_brightness(&self, id: DisplayId) -> Option<f32> {
        let get = self.private.get_brightness?;
        let mut b = 0f32;
        (unsafe { get(id.0, &mut b) } == sys::KERN_SUCCESS).then_some(b)
    }

    pub fn set_builtin_brightness(&self, id: DisplayId, value: f32) -> Result<()> {
        let set = self
            .private
            .set_brightness
            .ok_or_else(|| Error::Transport("DisplayServices unavailable".into()))?;
        let rc = unsafe { set(id.0, value.clamp(0.0, 1.0)) };
        if rc == sys::KERN_SUCCESS {
            Ok(())
        } else {
            Err(Error::Transport(format!(
                "DisplayServicesSetBrightness: {rc:#x}"
            )))
        }
    }

    fn device(&mut self, id: DisplayId) -> Result<&mut DdcDevice<AvServiceTransport>> {
        self.devices.get_mut(&id).ok_or(Error::NotFound(id))
    }
}

impl DisplayBackend for MacosBackend {
    /// CoreGraphics only — no IORegistry walk, no I2C.
    fn online_ids(&mut self) -> Result<Vec<DisplayId>> {
        Ok(ioreg::online_displays()
            .into_iter()
            .map(DisplayId)
            .collect())
    }

    fn power_source(&mut self) -> Result<PowerState> {
        use core_foundation::base::TCFType;
        use core_foundation::string::CFString;
        unsafe {
            let blob = sys::IOPSCopyPowerSourcesInfo();
            if blob.is_null() {
                return Ok(PowerState::Unknown);
            }
            // Take ownership of the Copy result so it is released on drop.
            let _blob = core_foundation::base::CFType::wrap_under_create_rule(blob);
            let kind = sys::IOPSGetProvidingPowerSourceType(blob);
            if kind.is_null() {
                return Ok(PowerState::Unknown);
            }
            // Get rule: IOPSGetProvidingPowerSourceType does not transfer ownership.
            let s = CFString::wrap_under_get_rule(kind).to_string();
            Ok(match s.as_str() {
                "AC Power" => PowerState::Ac,
                "Battery Power" => PowerState::Battery,
                // UPS Power, or something new. Unknown never fires a rule, which
                // is the right call for a state we do not understand.
                _ => PowerState::Unknown,
            })
        }
    }

    fn list(&mut self) -> Result<Vec<Monitor>> {
        let display_ids = ioreg::online_displays();
        let services = ioreg::services_for_matching(&self.private);
        let matches = matching::assign(&self.private, &display_ids, &services);

        self.devices.clear();
        self.paths.clear();
        let mut monitors = Vec::new();

        for id in display_ids {
            let did = DisplayId(id);
            let m = matches.iter().find(|m| m.display_id == id);

            let identity = m
                .map(|m| DisplayIdentity {
                    vendor: m.service.manufacturer_id.clone(),
                    product_name: m.service.product_name.clone(),
                    serial: m.service.serial_number,
                    alphanumeric_serial: m.service.alphanumeric_serial.clone(),
                    location: m.service.io_display_location.clone(),
                })
                .unwrap_or_default();

            let builtin = unsafe { sys::CGDisplayIsBuiltin(id) } != 0;
            let control = match m {
                Some(m) if !m.service.service.is_null() => {
                    // SAFETY: service is non-null and came from this walk.
                    let t = unsafe {
                        AvServiceTransport::new(Arc::clone(&self.private), m.service.service)
                    };
                    self.devices.insert(did, DdcDevice::new(t, self.timings));
                    ControlPath::Ddc
                }
                _ if builtin && self.private.get_brightness.is_some() => ControlPath::Native,
                _ => ControlPath::None,
            };
            self.paths.insert(did, control);

            monitors.push(Monitor {
                id: did,
                identity,
                control,
                capabilities: None,
            });
        }
        Ok(monitors)
    }

    fn get_vcp(&mut self, id: DisplayId, code: VcpCode) -> Result<(u16, u16)> {
        // The built-in panel has no I2C channel, but it does have brightness.
        // Routing it here means callers drive every display the same way.
        if self.paths.get(&id) == Some(&ControlPath::Native) {
            return match code {
                VcpCode::Brightness => {
                    let b = self
                        .builtin_brightness(id)
                        .ok_or_else(|| Error::Transport("DisplayServices read failed".into()))?;
                    let scaled = (b * NATIVE_BRIGHTNESS_MAX as f32).round() as u16;
                    Ok((scaled.min(NATIVE_BRIGHTNESS_MAX), NATIVE_BRIGHTNESS_MAX))
                }
                // Contrast, input source and friends are meaningless for the
                // built-in panel; say so rather than reporting a bogus value.
                _ => Err(Error::Unsupported(code)),
            };
        }
        let r = self.device(id)?.get_vcp(code.code())?;
        Ok((r.current, r.max))
    }

    fn set_vcp(&mut self, id: DisplayId, code: VcpCode, value: u16) -> Result<()> {
        if self.paths.get(&id) == Some(&ControlPath::Native) {
            return match code {
                VcpCode::Brightness => {
                    if value > NATIVE_BRIGHTNESS_MAX {
                        return Err(Error::OutOfRange {
                            code,
                            value,
                            max: NATIVE_BRIGHTNESS_MAX,
                        });
                    }
                    self.set_builtin_brightness(id, value as f32 / NATIVE_BRIGHTNESS_MAX as f32)
                }
                _ => Err(Error::Unsupported(code)),
            };
        }
        self.device(id)?.set_vcp(code.code(), value)?;
        Ok(())
    }

    fn capability_string(&mut self, id: DisplayId) -> Result<Option<String>> {
        if !self.devices.contains_key(&id) {
            return Ok(None);
        }
        Ok(Some(self.device(id)?.capability_string()?))
    }
}

/// Tests that need real hardware. Run with `cargo test -- --ignored`.
///
/// Kept out of CI, where there is no display and no battery, but kept in the
/// tree so they are runnable on a bench machine rather than re-improvised.
#[cfg(test)]
mod hardware_tests {
    use super::*;

    #[test]
    #[ignore = "requires hardware"]
    fn power_source_matches_pmset() {
        let mut b = MacosBackend::new().expect("backend");
        let ours = b.power_source().expect("power source");

        let out = std::process::Command::new("pmset")
            .args(["-g", "batt"])
            .output()
            .expect("pmset");
        let text = String::from_utf8_lossy(&out.stdout);
        let expected = if text.contains("'AC Power'") {
            PowerState::Ac
        } else if text.contains("'Battery Power'") {
            PowerState::Battery
        } else {
            PowerState::Unknown
        };

        println!("displayd reads: {ours}\npmset says:     {expected}");
        assert_eq!(ours, expected, "power source disagrees with pmset");
        assert_ne!(
            ours,
            PowerState::Unknown,
            "Unknown never fires a rule, so it would silently disable power automation"
        );
    }

    #[test]
    #[ignore = "requires hardware"]
    fn enumerates_at_least_the_builtin_display() {
        let mut b = MacosBackend::new().expect("backend");
        let monitors = b.list().expect("list");
        println!("{} display(s):", monitors.len());
        for m in &monitors {
            println!("  [{}] {} — {:?}", m.id, m.identity, m.control);
        }
        assert!(!monitors.is_empty());
        assert_eq!(
            b.online_ids().unwrap().len(),
            monitors.len(),
            "online_ids and list must agree, or hot-plug detection misfires"
        );
    }
}
