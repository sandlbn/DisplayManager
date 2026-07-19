//! DDC/CI packet framing and transactions.
//!
//! # A note on MonitorControl's framing
//!
//! `Arm64DDC.swift` builds its packet as
//! `[0x80|(payload+1), payload.count, ...payload, checksum]`, which reads as if
//! the second byte were a length. It is not: it is the **op code**, and the code
//! only works because Get VCP (`0x01`) and Set VCP (`0x03`) happen to equal
//! their own operand counts. That coincidence does not extend to any other
//! command — a Capabilities Request (`0xF3`) framed that way would be sent as a
//! Set VCP and corrupt the display's settings.
//!
//! This module therefore models the real structure — `[0x80|len, op, operands…,
//! checksum]` — which reproduces MonitorControl's bytes exactly for Get and Set
//! while extending correctly to `0xF3`.

use crate::{Error, I2cTransport, Result, Timings};
use std::thread::sleep;

/// 7-bit I2C address of the DDC/CI endpoint.
pub const DISPLAY_7BIT_ADDRESS: u32 = 0x37;
/// Host (source) address placed in the packet's data-address field.
pub const HOST_ADDRESS: u32 = 0x51;

const OP_GET_VCP: u8 = 0x01;
const OP_GET_VCP_REPLY: u8 = 0x02;
const OP_SET_VCP: u8 = 0x03;
const OP_CAPS_REQUEST: u8 = 0xF3;
const OP_CAPS_REPLY: u8 = 0xE3;

/// Checksum seed for host→display packets.
///
/// Both values are empirical, taken from MonitorControl and confirmed against
/// real hardware. They are inconsistent with each other — Get behaves as though
/// the source address were `0x00`, Set as though it were `0x51` — and also with
/// a literal reading of the spec. Do not "fix" them without a monitor to test
/// on; the install base says they work.
const SEED_GET: u8 = (DISPLAY_7BIT_ADDRESS as u8) << 1; // 0x6E
const SEED_SET: u8 = ((DISPLAY_7BIT_ADDRESS as u8) << 1) ^ (HOST_ADDRESS as u8); // 0x3F
/// Seed for validating display→host replies.
const SEED_REPLY: u8 = 0x50;

/// XOR checksum over `data[start..=end]`, inclusive.
pub fn checksum(seed: u8, data: &[u8], start: usize, end: usize) -> u8 {
    data[start..=end].iter().fold(seed, |acc, b| acc ^ b)
}

/// Build a host→display packet: `[0x80|len, op, operands…, checksum]`.
pub fn frame(op: u8, operands: &[u8], seed: u8) -> Vec<u8> {
    let len = 1 + operands.len();
    let mut p = Vec::with_capacity(len + 2);
    p.push(0x80 | len as u8);
    p.push(op);
    p.extend_from_slice(operands);
    p.push(0);
    let last = p.len() - 1;
    p[last] = checksum(seed, &p, 0, last - 1);
    p
}

/// Decoded Get VCP Feature reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VcpReply {
    pub code: u8,
    pub current: u16,
    pub max: u16,
    /// True for non-continuous (enumerated) codes such as input source.
    pub non_continuous: bool,
}

/// Parse an 11-byte Get VCP Feature reply.
///
/// Layout: `[src, 0x88, 0x02, result, opcode, type, max_hi, max_lo, cur_hi,
/// cur_lo, checksum]`.
pub fn parse_vcp_reply(reply: &[u8], expect_code: u8) -> Result<VcpReply> {
    if reply.len() < 11 {
        return Err(Error::MalformedReply(format!(
            "expected 11 bytes, got {}",
            reply.len()
        )));
    }
    let computed = checksum(SEED_REPLY, reply, 0, reply.len() - 2);
    let expected = reply[reply.len() - 1];
    if computed != expected {
        return Err(Error::Checksum { computed, expected });
    }
    if reply[2] != OP_GET_VCP_REPLY {
        return Err(Error::MalformedReply(format!(
            "expected op {OP_GET_VCP_REPLY:#04x}, got {:#04x}",
            reply[2]
        )));
    }
    // Result code 0x01 is the display saying it does not implement this VCP —
    // a real answer, not a transport failure. MonitorControl ignores this byte
    // and reports the resulting zeros as a valid reading.
    if reply[3] != 0x00 {
        return Err(Error::MalformedReply(format!(
            "display reports VCP {expect_code:#04x} unsupported (result {:#04x})",
            reply[3]
        )));
    }
    if reply[4] != expect_code {
        return Err(Error::MalformedReply(format!(
            "reply is for VCP {:#04x}, expected {expect_code:#04x}",
            reply[4]
        )));
    }
    Ok(VcpReply {
        code: reply[4],
        non_continuous: reply[5] == 0x01,
        max: u16::from(reply[6]) * 256 + u16::from(reply[7]),
        current: u16::from(reply[8]) * 256 + u16::from(reply[9]),
    })
}

/// A DDC/CI device over some I2C transport.
pub struct DdcDevice<T: I2cTransport> {
    transport: T,
    pub timings: Timings,
}

impl<T: I2cTransport> DdcDevice<T> {
    pub fn new(transport: T, timings: Timings) -> Self {
        DdcDevice { transport, timings }
    }

    /// Write a framed packet, honouring the empirical write-cycle repetition.
    fn write_packet(&mut self, packet: &[u8]) -> Result<()> {
        let mut last = Ok(());
        for _ in 0..self.timings.num_write_cycles.max(1) {
            sleep(self.timings.write_sleep);
            last = self
                .transport
                .write(DISPLAY_7BIT_ADDRESS, HOST_ADDRESS, packet);
        }
        last
    }

    /// Get VCP Feature.
    pub fn get_vcp(&mut self, code: u8) -> Result<VcpReply> {
        let packet = frame(OP_GET_VCP, &[code], SEED_GET);
        let mut last_err = Error::NoResponse(0);

        for _ in 0..=self.timings.num_retries {
            if self.write_packet(&packet).is_err() {
                sleep(self.timings.retry_sleep);
                continue;
            }
            sleep(self.timings.read_sleep);
            let mut reply = [0u8; 11];
            match self.transport.read(DISPLAY_7BIT_ADDRESS, 0, &mut reply) {
                Ok(()) => match parse_vcp_reply(&reply, code) {
                    Ok(r) => return Ok(r),
                    Err(e) => last_err = e,
                },
                Err(e) => last_err = e,
            }
            sleep(self.timings.retry_sleep);
        }
        Err(last_err)
    }

    /// Set VCP Feature. Fire-and-forget: DDC sends no acknowledgement.
    pub fn set_vcp(&mut self, code: u8, value: u16) -> Result<()> {
        let packet = frame(
            OP_SET_VCP,
            &[code, (value >> 8) as u8, (value & 0xFF) as u8],
            SEED_SET,
        );
        let mut last = Err(Error::NoResponse(0));
        for _ in 0..=self.timings.num_retries {
            last = self.write_packet(&packet);
            if last.is_ok() {
                return Ok(());
            }
            sleep(self.timings.retry_sleep);
        }
        last
    }

    /// Read the full capability string, which arrives in chunks.
    ///
    /// Reply layout: `[src, 0x80|len, 0xE3, off_hi, off_lo, data…, checksum]`,
    /// where the data run is `len - 3` bytes. A zero-length run terminates.
    pub fn capability_string(&mut self) -> Result<String> {
        let mut out = Vec::new();
        let mut offset: u16 = 0;

        // Bounded so a monitor that never returns an empty chunk cannot hang the
        // I2C worker; 64 chunks is far past any real capability string.
        for _ in 0..64 {
            let chunk = self.capability_chunk(offset)?;
            if chunk.is_empty() {
                break;
            }
            out.extend_from_slice(&chunk);
            offset += chunk.len() as u16;
        }

        String::from_utf8(out).map_err(|e| Error::CapabilityParse(e.to_string()))
    }

    fn capability_chunk(&mut self, offset: u16) -> Result<Vec<u8>> {
        let packet = frame(
            OP_CAPS_REQUEST,
            &[(offset >> 8) as u8, (offset & 0xFF) as u8],
            SEED_GET,
        );
        let mut last_err = Error::NoResponse(0);

        for _ in 0..=self.timings.num_retries {
            if self.write_packet(&packet).is_err() {
                sleep(self.timings.retry_sleep);
                continue;
            }
            sleep(self.timings.read_sleep);

            let mut reply = [0u8; 64];
            if let Err(e) = self.transport.read(DISPLAY_7BIT_ADDRESS, 0, &mut reply) {
                last_err = e;
                sleep(self.timings.retry_sleep);
                continue;
            }
            match parse_capability_chunk(&reply, offset) {
                Ok(data) => return Ok(data),
                Err(e) => last_err = e,
            }
            sleep(self.timings.retry_sleep);
        }
        Err(last_err)
    }
}

/// Parse one Capabilities Reply chunk out of a fixed-size read buffer.
///
/// The buffer is longer than the frame, so the frame length must be recovered
/// from the length byte before the checksum can be located.
pub fn parse_capability_chunk(reply: &[u8], expect_offset: u16) -> Result<Vec<u8>> {
    if reply.len() < 6 {
        return Err(Error::MalformedReply("caps reply too short".into()));
    }
    if reply[1] & 0x80 == 0 {
        return Err(Error::MalformedReply(format!(
            "caps reply length byte {:#04x} lacks high bit",
            reply[1]
        )));
    }
    let len = (reply[1] & 0x7F) as usize;
    if len < 3 {
        return Err(Error::MalformedReply(format!("caps run length {len} < 3")));
    }
    // frame = src + len byte + `len` payload bytes + checksum
    let frame_len = 2 + len + 1;
    if frame_len > reply.len() {
        return Err(Error::MalformedReply(format!(
            "caps frame of {frame_len} exceeds {} byte read",
            reply.len()
        )));
    }
    let frame = &reply[..frame_len];
    let computed = checksum(SEED_REPLY, frame, 0, frame_len - 2);
    let expected = frame[frame_len - 1];
    if computed != expected {
        return Err(Error::Checksum { computed, expected });
    }
    if frame[2] != OP_CAPS_REPLY {
        return Err(Error::MalformedReply(format!(
            "expected caps op {OP_CAPS_REPLY:#04x}, got {:#04x}",
            frame[2]
        )));
    }
    let offset = u16::from(frame[3]) * 256 + u16::from(frame[4]);
    if offset != expect_offset {
        return Err(Error::MalformedReply(format!(
            "caps chunk offset {offset}, expected {expect_offset}"
        )));
    }
    Ok(frame[5..frame_len - 1].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes MonitorControl puts on the wire for Get VCP 0x10, confirmed
    /// working against real hardware. Framing must reproduce them exactly.
    #[test]
    fn get_vcp_frame_matches_monitorcontrol_bytes() {
        assert_eq!(
            frame(OP_GET_VCP, &[0x10], SEED_GET),
            vec![0x82, 0x01, 0x10, 0xFD]
        );
    }

    #[test]
    fn set_vcp_frame_matches_monitorcontrol_bytes() {
        assert_eq!(
            frame(OP_SET_VCP, &[0x10, 0x00, 0x32], SEED_SET),
            vec![0x84, 0x03, 0x10, 0x00, 0x32, 0x9A]
        );
    }

    /// The bug MonitorControl's framing would have: 0xF3 must not be encoded as
    /// a Set VCP. The length byte must reflect the operand count, and the op
    /// byte must stay 0xF3.
    #[test]
    fn caps_request_is_not_mistaken_for_set_vcp() {
        let p = frame(OP_CAPS_REQUEST, &[0x00, 0x00], SEED_GET);
        assert_eq!(p[0], 0x83, "length byte must encode 3 data bytes");
        assert_eq!(p[1], OP_CAPS_REQUEST);
        assert_ne!(p[1], OP_SET_VCP);
    }

    fn vcp_reply_bytes(code: u8, ty: u8, max: u16, cur: u16, result: u8) -> [u8; 11] {
        let mut r = [
            0x6E,
            0x88,
            OP_GET_VCP_REPLY,
            result,
            code,
            ty,
            (max >> 8) as u8,
            (max & 0xFF) as u8,
            (cur >> 8) as u8,
            (cur & 0xFF) as u8,
            0,
        ];
        r[10] = checksum(SEED_REPLY, &r, 0, 9);
        r
    }

    #[test]
    fn parses_a_well_formed_reply() {
        let r = vcp_reply_bytes(0x10, 0x00, 100, 50, 0x00);
        let p = parse_vcp_reply(&r, 0x10).unwrap();
        assert_eq!(p.current, 50);
        assert_eq!(p.max, 100);
        assert!(!p.non_continuous);
    }

    #[test]
    fn flags_non_continuous_codes() {
        let r = vcp_reply_bytes(0x60, 0x01, 0x11, 0x0F, 0x00);
        assert!(parse_vcp_reply(&r, 0x60).unwrap().non_continuous);
    }

    /// Result code 1 means "unsupported", not "value is 0".
    #[test]
    fn unsupported_vcp_is_an_error_not_a_zero_reading() {
        let r = vcp_reply_bytes(0x10, 0x00, 0, 0, 0x01);
        assert!(parse_vcp_reply(&r, 0x10).is_err());
    }

    #[test]
    fn rejects_reply_for_a_different_code() {
        let r = vcp_reply_bytes(0x12, 0x00, 100, 50, 0x00);
        assert!(parse_vcp_reply(&r, 0x10).is_err());
    }

    #[test]
    fn rejects_corrupt_checksum() {
        let mut r = vcp_reply_bytes(0x10, 0x00, 100, 50, 0x00);
        r[10] ^= 0xFF;
        assert!(matches!(
            parse_vcp_reply(&r, 0x10),
            Err(Error::Checksum { .. })
        ));
    }

    fn caps_reply_bytes(offset: u16, data: &[u8], buf_len: usize) -> Vec<u8> {
        let len = data.len() + 3;
        let mut r = vec![
            0x6E,
            0x80 | len as u8,
            OP_CAPS_REPLY,
            (offset >> 8) as u8,
            (offset & 0xFF) as u8,
        ];
        r.extend_from_slice(data);
        r.push(0);
        let last = r.len() - 1;
        r[last] = checksum(SEED_REPLY, &r, 0, last - 1);
        r.resize(buf_len, 0); // trailing garbage, as a real fixed-size read has
        r
    }

    #[test]
    fn parses_caps_chunk_from_oversized_buffer() {
        let r = caps_reply_bytes(0, b"(prot(monitor)", 64);
        assert_eq!(parse_capability_chunk(&r, 0).unwrap(), b"(prot(monitor)");
    }

    #[test]
    fn empty_caps_chunk_terminates_without_error() {
        let r = caps_reply_bytes(12, b"", 64);
        assert!(parse_capability_chunk(&r, 12).unwrap().is_empty());
    }

    #[test]
    fn rejects_caps_chunk_at_wrong_offset() {
        let r = caps_reply_bytes(32, b"abc", 64);
        assert!(parse_capability_chunk(&r, 0).is_err());
    }

    #[test]
    fn rejects_caps_frame_longer_than_read_buffer() {
        let mut r = caps_reply_bytes(0, b"abcdefgh", 64);
        r[1] = 0x80 | 60; // claim more payload than was read
        assert!(parse_capability_chunk(&r[..16], 0).is_err());
    }
}
