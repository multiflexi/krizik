//! USB protocol of ALIENTEK C0 and C2 testers, as used by ALIENTEK's ATK-C
//! desktop software.
//!
//! Reverse-engineered from ATK-C 1.0.1.6/1.0.1.7; see `USB_PROTOCOLS.md`.
//! Both testers enumerate as `2E88:C2C2` and differ only in their version
//! reply.  Interface 0, bulk OUT 0x01 / IN 0x82.
//!
//! Host: `EE cmd len data… chk FF`, zero padded to 63 bytes.  Device:
//! `EE cmd len data… chk`, where the byte sum of the frame is 0.

use crate::firmware::app_frame;
use crate::protocol::{ascii_field, checksum, i24, ProtocolInfo, RawSample};

pub const VID: u16 = 0x2E88;
pub const PID: u16 = 0xC2C2;
pub const EP_OUT: u8 = 0x01;
pub const EP_IN: u8 = 0x82;

pub const CMD_VERSION: u8 = 0x01;
pub const CMD_RATE: u8 = 0x03;
pub const CMD_STREAM: u8 = 0x04;
/// Sent by the tester on its own: what the PD monitor sees.
pub const CMD_PD_MONITOR: u8 = 0x19;

/// Sample rates by index.
pub const RATES: [u16; 7] = [2, 4, 20, 50, 200, 500, 1000];
const LIVE_LEN: usize = 57;

pub fn version_request() -> Vec<u8> {
    app_frame(CMD_VERSION, &[0, 0])
}

pub fn rate_request() -> Vec<u8> {
    app_frame(CMD_RATE, &[0, 0, 0, 0])
}

pub fn stream_request(on: bool) -> Vec<u8> {
    app_frame(CMD_STREAM, &[on as u8, 0])
}

/// Samples per second from a rate reply; the software treats anything
/// unknown as the slowest rate.
pub fn parse_rate(d: &[u8]) -> u16 {
    d.get(1).and_then(|i| RATES.get(*i as usize)).copied().unwrap_or(RATES[0])
}

/// Reassembles device frames from bulk reads, which may split or merge them.
#[derive(Debug, Default)]
pub struct FrameParser {
    buf: Vec<u8>,
}

impl FrameParser {
    /// Returns complete frames as (command, data).
    pub fn push(&mut self, bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            let start = self.buf.iter().position(|b| *b == 0xEE).unwrap_or(self.buf.len());
            self.buf.drain(..start);
            if self.buf.len() < 4 {
                break;
            }
            let (cmd, len) = (self.buf[1], self.buf[2] as usize);
            if !(1..=46).contains(&cmd) || len == 0 {
                self.buf.remove(0);
                continue;
            }
            let total = len + 4;
            if self.buf.len() < total {
                break;
            }
            if checksum(&self.buf[..total - 1]) == self.buf[total - 1] {
                out.push((cmd, self.buf[3..3 + len].to_vec()));
                self.buf.drain(..total);
            } else {
                self.buf.remove(0);
            }
        }
        // Never keep more than a few frames of garbage.
        if self.buf.len() > 1024 {
            let cut = self.buf.len() - 256;
            self.buf.drain(..cut);
        }
        out
    }
}

/// The reply to the version request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub firmware: String,
    pub hardware: String,
    pub bootloader: String,
    pub model_code: u16,
    pub serial: String,
    pub model_name: String,
    /// The tester is in its update mode.
    pub boot: bool,
}

fn tenths(v: u8) -> String {
    format!("{}.{}", v / 10, v % 10)
}

/// Parses the 30-byte (current) or 29-byte (older firmware) version reply.
pub fn parse_version(d: &[u8]) -> Option<Version> {
    match d.len() {
        30 => {
            let v = d[1] as u32 * 100 + d[2] as u32;
            Some(Version {
                firmware: format!("{}.{}.{}", v / 100, (v / 10) % 10, v % 10),
                hardware: tenths(d[3]),
                bootloader: tenths(d[0]),
                model_code: u16::from_le_bytes([d[5], d[6]]),
                serial: ascii_field(&d[7..15]),
                model_name: ascii_field(&d[15..27]),
                boot: d[27] != 0,
            })
        }
        29 => Some(Version {
            firmware: format!("{}.x", tenths(d[1])),
            hardware: tenths(d[2]),
            bootloader: tenths(d[0]),
            model_code: u16::from_le_bytes([d[4], d[5]]),
            serial: ascii_field(&d[6..14]),
            model_name: ascii_field(&d[14..26]),
            boot: d[26] != 0,
        }),
        _ => None,
    }
}

/// Decodes a live-data frame (command 0x04 with 57 bytes): four samples of
/// VBUS and current (24-bit, 10 µV / 10 µA) and D+, D−, CC1, CC2 (mV).
/// Below 500 samples/s the software uses only the first sample.
pub fn decode_live(d: &[u8], sps: u16) -> Option<Vec<RawSample>> {
    if d.len() < LIVE_LEN {
        return None;
    }
    let n = if sps >= 500 { 4 } else { 1 };
    let mv = |i: usize| u16::from_le_bytes([d[i], d[i + 1]]) as f32 / 1000.0;
    Some(
        (0..n)
            .map(|k| RawSample {
                voltage: i24(&d[3 * k..]) as f32 / 100_000.0,
                current: i24(&d[12 + 3 * k..]) as f32 / 100_000.0,
                d_plus: mv(24 + 2 * k),
                d_minus: mv(32 + 2 * k),
                cc1: mv(40 + 2 * k),
                cc2: mv(48 + 2 * k),
            })
            .collect(),
    )
}

/// What the tester's PD monitor shows (command 0x19, ASCII fields).
pub fn parse_pd_monitor(d: &[u8]) -> Option<ProtocolInfo> {
    if d.len() < 54 {
        return None;
    }
    Some(ProtocolInfo {
        name: ascii_field(&d[0..10]),
        power: ascii_field(&d[10..18]),
        gear: ascii_field(&d[26..34]),
        gear_detail: ascii_field(&d[18..26]),
        request: ascii_field(&d[34..44]),
        request_detail: ascii_field(&d[44..54]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace().map(|b| u8::from_str_radix(b, 16).unwrap()).collect()
    }

    #[test]
    fn builds_requests() {
        assert_eq!(&version_request()[..7], hex("EE 01 02 00 00 0F FF").as_slice());
        assert_eq!(&stream_request(true)[..7], hex("EE 04 02 01 00 0B FF").as_slice());
        assert_eq!(&stream_request(false)[..7], hex("EE 04 02 00 00 0C FF").as_slice());
        assert_eq!(&rate_request()[..9], hex("EE 03 04 00 00 00 00 0B FF").as_slice());
        assert_eq!(version_request().len(), 63);
    }

    const VERSION: &str =
        "EE 01 1E 0A 02 2C 0F 00 0A 03 39 33 30 38 46 46 37 46 41 54 4B 2D 43 32 20 50 72 6F 00 00 00 00 01 EE";
    const LIVE: &str = "EE 04 39
        20 A1 07 2A A1 07 16 A1 07 20 A1 07
        A0 86 01 94 88 01 D8 85 01 A0 86 01
        58 02 59 02 58 02 57 02
        0A 00 0A 00 0B 00 0A 00
        B0 04 B0 04 B0 04 B0 04
        00 00 01 00 00 00 00 00
        01 89";

    #[test]
    fn parses_frames_across_reads() {
        let mut stream = hex("FF 00");
        stream.extend(hex(VERSION));
        stream.extend(hex(LIVE));
        let mut p = FrameParser::default();
        let mut frames = p.push(&stream[..20]);
        frames.extend(p.push(&stream[20..70]));
        frames.extend(p.push(&stream[70..]));
        assert_eq!(frames.iter().map(|f| (f.0, f.1.len())).collect::<Vec<_>>(), [(1, 30), (4, 57)]);

        let v = parse_version(&frames[0].1).unwrap();
        assert_eq!(v.firmware, "2.4.4");
        assert_eq!((v.hardware.as_str(), v.bootloader.as_str()), ("1.5", "1.0"));
        assert_eq!((v.model_code, v.serial.as_str(), v.model_name.as_str()), (0x030A, "9308FF7F", "ATK-C2 Pro"));
        assert!(!v.boot);

        let s = decode_live(&frames[1].1, 1000).unwrap();
        assert_eq!(s.len(), 4);
        assert!((s[0].voltage - 5.0).abs() < 1e-6 && (s[0].current - 1.0).abs() < 1e-6);
        assert!((s[1].voltage - 5.0001).abs() < 1e-6 && (s[1].current - 1.005).abs() < 1e-6);
        assert!((s[2].voltage - 4.9999).abs() < 1e-6 && (s[2].current - 0.998).abs() < 1e-6);
        assert!((s[1].d_plus - 0.601).abs() < 1e-6 && (s[0].d_minus - 0.010).abs() < 1e-6);
        assert!((s[0].cc1 - 1.2).abs() < 1e-6 && (s[1].cc2 - 0.001).abs() < 1e-6);
        assert_eq!(decode_live(&frames[1].1, 200).unwrap().len(), 1);
    }

    #[test]
    fn rejects_corrupt_frames_and_reads_rate() {
        let mut bad = hex(LIVE);
        bad[10] ^= 0x40;
        let mut p = FrameParser::default();
        assert!(p.push(&bad).is_empty());
        assert_eq!(parse_rate(&[0, 6, 0, 0]), 1000);
        assert_eq!(parse_rate(&[0, 9, 0, 0]), 2);
        let mut pd = vec![b' '; 55];
        pd[..5].copy_from_slice(b"PD3.0");
        pd[10..13].copy_from_slice(b"65W");
        pd[34..39].copy_from_slice(b"9.00V");
        let info = parse_pd_monitor(&pd).unwrap();
        assert_eq!((info.name.as_str(), info.power.as_str(), info.request.as_str()), ("PD3.0", "65W", "9.00V"));
    }
}
