//! DDC/CI protocol: framing, VCP codes, capability strings.
//!
//! Transport-agnostic by construction — this crate never touches IOKit. The
//! platform supplies an [`I2cTransport`]; on macOS that is IOAVService, on Linux
//! it would be `/dev/i2c-*`. Keeping the split here is what makes the protocol
//! logic testable in CI against a mock, with no hardware present.

pub mod caps;
pub mod protocol;
pub mod vcp;

use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i2c transport error: {0}")]
    Transport(String),
    #[error("checksum mismatch: computed {computed:#04x}, expected {expected:#04x}")]
    Checksum { computed: u8, expected: u8 },
    #[error("display did not respond after {0} attempts")]
    NoResponse(u8),
    #[error("malformed reply: {0}")]
    MalformedReply(String),
    #[error("capability string parse error: {0}")]
    CapabilityParse(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A raw I2C channel to a display.
pub trait I2cTransport {
    fn write(&mut self, chip_address: u32, data_address: u32, data: &[u8]) -> Result<()>;
    fn read(&mut self, chip_address: u32, offset: u32, buf: &mut [u8]) -> Result<()>;
}

/// Per-monitor timing tolerances.
///
/// These are empirical values from MonitorControl, not spec requirements: they
/// are what real monitors were observed to tolerate. The doubled write cycle and
/// the pre-write sleep look redundant but are load-bearing. Verified working
/// unmodified on an ASUS MB169CK.
#[derive(Clone, Copy, Debug)]
pub struct Timings {
    pub write_sleep: Duration,
    pub num_write_cycles: u8,
    pub read_sleep: Duration,
    pub num_retries: u8,
    pub retry_sleep: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            write_sleep: Duration::from_micros(10_000),
            num_write_cycles: 2,
            read_sleep: Duration::from_micros(50_000),
            num_retries: 4,
            retry_sleep: Duration::from_micros(20_000),
        }
    }
}
