//! USB HID protocol of FNIRSI USB testers: FNB58, FNB48S, FNB48, C1, and the
//! FNB-C2, which identifies itself as an FNB58.
//!
//! See `USB_PROTOCOLS.md`.  The wire format follows published reverse
//! engineering (OpenFNB58, baryluk's fnirsi-usb-power-data-logger).  The USB descriptors and the FNB58 identity
//! were confirmed in the FNB-C2 firmware; nothing was tested on hardware.
//!
//! The host writes 64-byte reports `AA cmd 00… crc` to the HID interface and
//! receives 64-byte reports `AA 04` with four samples of 15 bytes each, every
//! 40 ms (100 samples/s).

use std::time::Duration;

use crate::protocol::RawSample;

pub const REPORT_LEN: usize = 64;
/// One sample every 10 ms, four per report.
pub const SAMPLE_INTERVAL_MS: u16 = 10;

const HEADER: u8 = 0xAA;
pub const CMD_INIT1: u8 = 0x81;
pub const CMD_INIT2: u8 = 0x82;
/// Keeps the data coming.
pub const CMD_POLL: u8 = 0x83;
const DATA: u8 = 0x04;

/// The two generations differ only in how they are started and polled.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Generation {
    /// FNB58, FNB48S (and FNB-C2).
    Fnb58,
    /// FNB48, C1.
    Fnb48,
}

impl Generation {
    /// Commands sent after opening the device.
    pub fn init(self) -> [u8; 3] {
        match self {
            Generation::Fnb58 => [CMD_INIT1, CMD_INIT2, CMD_INIT2],
            Generation::Fnb48 => [CMD_INIT1, CMD_INIT2, CMD_POLL],
        }
    }

    pub fn poll_interval(self) -> Duration {
        match self {
            Generation::Fnb58 => Duration::from_secs(1),
            // The published logger polls every 3 ms; the HID endpoint runs at 10 ms.
            Generation::Fnb48 => Duration::from_millis(10),
        }
    }
}

/// CRC-8 with polynomial 0x39, initial value 0x42, no reflection.
pub fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0x42u8;
    for b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x39 } else { crc << 1 };
        }
    }
    crc
}

/// A 64-byte command report.
pub fn command(cmd: u8) -> [u8; REPORT_LEN] {
    let mut r = [0u8; REPORT_LEN];
    r[0] = HEADER;
    r[1] = cmd;
    r[REPORT_LEN - 1] = crc8(&r[1..REPORT_LEN - 1]);
    r
}

/// A decoded data report.
#[derive(Clone, Debug, PartialEq)]
pub struct Packet {
    pub samples: Vec<RawSample>,
    /// The tester's temperature in °C.
    pub temperature_c: f32,
    /// Whether the trailing CRC matched; not every firmware may send one.
    pub crc_ok: bool,
}

fn u16_at(p: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([p[i], p[i + 1]])
}

fn u32_at(p: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([p[i], p[i + 1], p[i + 2], p[i + 3]])
}

/// Decodes a data report; other reports give `None`.
pub fn decode(p: &[u8]) -> Option<Packet> {
    if p.len() < REPORT_LEN || p[0] != HEADER || p[1] != DATA {
        return None;
    }
    let mut temperature = 0.0;
    let samples = (0..4)
        .map(|k| {
            let o = 2 + 15 * k;
            temperature += u16_at(p, o + 13) as f32 / 10.0 / 4.0;
            RawSample {
                voltage: u32_at(p, o) as f32 / 100_000.0,
                current: u32_at(p, o + 4) as f32 / 100_000.0,
                d_plus: u16_at(p, o + 8) as f32 / 1000.0,
                d_minus: u16_at(p, o + 10) as f32 / 1000.0,
                cc1: f32::NAN,
                cc2: f32::NAN,
            }
        })
        .collect();
    let crc_ok = crc8(&p[1..REPORT_LEN - 1]) == p[REPORT_LEN - 1];
    Some(Packet { samples, temperature_c: temperature, crc_ok })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace().map(|b| u8::from_str_radix(b, 16).unwrap()).collect()
    }

    #[test]
    fn commands_match_published_bytes() {
        // The published tools send these with hard-coded checksums.
        for (cmd, crc) in [(CMD_INIT1, 0x8E), (CMD_INIT2, 0x96), (CMD_POLL, 0x9E)] {
            let r = command(cmd);
            assert_eq!((r[0], r[1], r[63]), (0xAA, cmd, crc));
            assert!(r[2..63].iter().all(|b| *b == 0));
        }
    }

    #[test]
    fn decodes_data_report() {
        let p = hex("aa 04 20 a1 07 00 f0 49 02 00 58 02 00 00 00 fd 00
             84 a1 07 00 b8 4a 02 00 5a 02 01 00 00 fd 00
             e8 a1 07 00 28 49 02 00 56 02 00 00 00 fe 00
             bc a0 07 00 e4 4b 02 00 59 02 02 00 00 fe 00
             00 08");
        let pkt = decode(&p).unwrap();
        assert!(pkt.crc_ok);
        assert_eq!(pkt.samples.len(), 4);
        let s = &pkt.samples[0];
        assert!((s.voltage - 5.0).abs() < 1e-6 && (s.current - 1.5).abs() < 1e-6);
        assert!((s.d_plus - 0.6).abs() < 1e-6 && s.d_minus == 0.0 && s.cc1.is_nan());
        assert!((pkt.samples[3].voltage - 4.999).abs() < 1e-5);
        assert!((pkt.samples[1].current - 1.502).abs() < 1e-6);
        assert!((pkt.temperature_c - 25.35).abs() < 1e-4);

        let mut bad = p.clone();
        bad[63] ^= 1;
        assert!(!decode(&bad).unwrap().crc_ok);
        bad[1] = 0x05;
        assert!(decode(&bad).is_none());
        assert!(decode(&p[..40]).is_none());
    }
}
