//! `I2cTransport` over IOAVService.

use crate::sys::{IOAVServiceRef, Private, KERN_SUCCESS};
use display_ddc::{Error, I2cTransport, Result};
use std::ffi::c_void;
use std::sync::Arc;

/// An I2C channel to one external display.
///
/// DDC is fragile under concurrent access, so this is deliberately not `Sync`:
/// serialization is the daemon's job (one worker per display), and the type
/// system should not quietly permit otherwise.
pub struct AvServiceTransport {
    private: Arc<Private>,
    service: IOAVServiceRef,
}

// SAFETY: the underlying IOAVService may be moved between threads, but the lack
// of a Sync impl means only one thread can use it at a time.
unsafe impl Send for AvServiceTransport {}

impl AvServiceTransport {
    /// # Safety
    /// `service` must be a live IOAVService from `IOAVServiceCreateWithService`.
    pub unsafe fn new(private: Arc<Private>, service: IOAVServiceRef) -> Self {
        AvServiceTransport { private, service }
    }
}

impl I2cTransport for AvServiceTransport {
    fn write(&mut self, chip_address: u32, data_address: u32, data: &[u8]) -> Result<()> {
        if self.service.is_null() {
            return Err(Error::Transport("no AV service".into()));
        }
        let rc = unsafe {
            (self.private.av_write)(
                self.service,
                chip_address,
                data_address,
                data.as_ptr() as *const c_void,
                data.len() as u32,
            )
        };
        if rc == KERN_SUCCESS {
            Ok(())
        } else {
            Err(Error::Transport(format!(
                "IOAVServiceWriteI2C failed: {rc:#x}"
            )))
        }
    }

    fn read(&mut self, chip_address: u32, offset: u32, buf: &mut [u8]) -> Result<()> {
        if self.service.is_null() {
            return Err(Error::Transport("no AV service".into()));
        }
        let rc = unsafe {
            (self.private.av_read)(
                self.service,
                chip_address,
                offset,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
            )
        };
        if rc == KERN_SUCCESS {
            Ok(())
        } else {
            Err(Error::Transport(format!(
                "IOAVServiceReadI2C failed: {rc:#x}"
            )))
        }
    }
}
