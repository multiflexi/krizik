//! USB HID protocol of the ALIENTEK UT70 USB tester.
//!
//! Reverse-engineered from ALIENTEK's ATK_UT70DLL; see `USB_PROTOCOLS.md`.
//! Not tested on hardware.
//!
//! 64-byte HID reports without report ID.  Host to device:
//! `EE fid len payload… sum FF`, zero padded; device to host:
//! `EE fid len payload… sum`.  `sum` is the 8-bit sum of all bytes before
//! it.  The tester streams live data (function 0x08) by itself once opened.

use crate::protocol::RawSample;

const HEADER: u8 = 0xEE;
const END: u8 = 0xFF;
pub const REPORT_LEN: usize = 64;
const MAX_PAYLOAD: usize = 60;

pub const FID_VERSION: u8 = 0x02;
pub const FID_DATA: u8 = 0x08;
pub const FID_FREQUENCY: u8 = 0x70;

/// Sample rates selectable on the tester, by index.
pub const RATES: [u16; 6] = [4, 10, 50, 200, 500, 1000];
/// Time between samples in ms, by rate index.
const PERIOD_MS: [u16; 6] = [250, 100, 20, 5, 2, 1];

fn sum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b))
}

/// A command frame, zero padded to a report.
pub fn command(fid: u8, payload: &[u8]) -> [u8; REPORT_LEN] {
    assert!(payload.len() <= MAX_PAYLOAD);
    let mut r = [0u8; REPORT_LEN];
    r[0] = HEADER;
    r[1] = fid;
    r[2] = payload.len() as u8;
    r[3..3 + payload.len()].copy_from_slice(payload);
    let n = 3 + payload.len();
    r[n] = sum(&r[..n]);
    r[n + 1] = END;
    r
}

/// Asks for the hardware version and serial number.
pub fn get_version() -> [u8; REPORT_LEN] {
    command(FID_VERSION, &[0, 0])
}

/// Asks for the current sample rate.
pub fn get_rate() -> [u8; REPORT_LEN] {
    command(FID_FREQUENCY, &[0, 0, 0, 0])
}

/// The sample period for a rate index (the vendor library falls back to 250 ms).
pub fn period_ms(index: u8) -> u16 {
    PERIOD_MS.get(index as usize).copied().unwrap_or(250)
}

/// A frame received from the tester.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub fid: u8,
    pub data: Vec<u8>,
    /// Whether the checksum matched (the vendor library doesn't check it).
    pub sum_ok: bool,
}

/// Parses an input report.  Tolerates a leading report-ID byte.
pub fn parse_report(r: &[u8]) -> Option<Frame> {
    let r = if r.len() > REPORT_LEN || (r.first() == Some(&0) && r.get(1) == Some(&HEADER)) { &r[1..] } else { r };
    if r.len() < 3 {
        return None;
    }
    let len = r[2] as usize;
    if len > MAX_PAYLOAD || r.len() < 3 + len {
        return None;
    }
    let sum_ok = r.get(3 + len) == Some(&sum(&r[..3 + len]));
    Some(Frame { fid: r[1], data: r[3..3 + len].to_vec(), sum_ok })
}

/// Hardware version and serial number (function 0x02).
pub fn parse_version(d: &[u8]) -> Option<(u8, String)> {
    let hw = *d.get(2)?;
    let serial = d.get(4..12).map(|s| String::from_utf8_lossy(s).trim_matches(char::from(0)).trim().to_string());
    Some((hw, serial.unwrap_or_default()))
}

/// Rate index from a sample-rate reply (function 0x70).
pub fn parse_rate(d: &[u8]) -> Option<u8> {
    d.get(1).copied().filter(|i| (*i as usize) < RATES.len())
}

fn f32_at(d: &[u8], i: usize) -> f32 {
    f32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]])
}

fn u16_at(d: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([d[i], d[i + 1]])
}

/// Live samples (function 0x08).  The payload has five sample slots
/// (voltages at 0, currents at 20, D+ at 40, D− at 50); at rates below
/// 500/s only the first is used, as in the vendor library.
pub fn decode_data(d: &[u8], rate_index: u8) -> Vec<RawSample> {
    let count = if rate_index >= 4 { 5 } else { 1 };
    (0..count)
        .take_while(|i| 50 + 2 * i + 2 <= d.len())
        .map(|i| RawSample {
            voltage: f32_at(d, 4 * i),
            current: f32_at(d, 20 + 4 * i),
            d_plus: u16_at(d, 40 + 2 * i) as f32 / 10_000.0,
            d_minus: u16_at(d, 50 + 2 * i) as f32 / 10_000.0,
            cc1: f32::NAN,
            cc2: f32::NAN,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace().map(|b| u8::from_str_radix(b, 16).unwrap()).collect()
    }

    #[test]
    fn builds_commands() {
        assert_eq!(&get_version()[..7], hex("ee 02 02 00 00 f2 ff").as_slice());
        assert_eq!(&get_rate()[..9], hex("ee 70 04 00 00 00 00 62 ff").as_slice());
        // Setting the rate (not used by Křižík): [1, index, rate as u16 LE].
        assert_eq!(&command(FID_FREQUENCY, &[1, 5, 0xe8, 3])[..9], hex("ee 70 04 01 05 e8 03 53 ff").as_slice());
        assert_eq!(&command(0x00, &[1, 0])[..7], hex("ee 00 02 01 00 f1 ff").as_slice());
        assert!(get_rate()[9..].iter().all(|b| *b == 0));
    }

    const CV: &str = "ee 08 3c 00 00 a0 40 00 00 a0 40 00 00 a0 40 00
                      00 a0 40 00 00 a0 40 00 00 00 40 00 00 00 40 00
                      00 00 40 00 00 00 40 00 00 00 40 70 17 70 17 70
                      17 70 17 70 17 b8 0b b8 0b b8 0b b8 0b b8 0b 44";

    #[test]
    fn decodes_live_data() {
        let f = parse_report(&hex(CV)).unwrap();
        assert_eq!((f.fid, f.data.len(), f.sum_ok), (FID_DATA, 60, true));
        let s = decode_data(&f.data, 4);
        assert_eq!(s.len(), 5);
        assert_eq!((s[4].voltage, s[4].current), (5.0, 2.0));
        assert!((s[0].d_plus - 0.6).abs() < 1e-6 && (s[0].d_minus - 0.3).abs() < 1e-6);
        assert!(s[0].cc1.is_nan());
        assert_eq!(decode_data(&f.data, 2).len(), 1);
        assert_eq!(period_ms(4), 2);
        assert_eq!(period_ms(9), 250);

        // With a report-ID byte in front.
        let mut with_id = vec![0];
        with_id.extend(hex(CV));
        assert_eq!(parse_report(&with_id).unwrap().data, f.data);
        // Too long a payload is rejected.
        let mut bad = hex(CV);
        bad[2] = 61;
        assert!(parse_report(&bad).is_none());
    }

    #[test]
    fn parses_replies() {
        let r = command(FID_VERSION, &[1, 2, 15, 3, b'U', b'T', b'7', b'0', b'1', b'2', b'3', b'4']);
        let f = parse_report(&r).unwrap();
        assert!(f.sum_ok);
        assert_eq!(parse_version(&f.data), Some((15, "UT701234".into())));
        let f = parse_report(&command(FID_FREQUENCY, &[0, 5, 0xe8, 3])).unwrap();
        assert_eq!(parse_rate(&f.data), Some(5));
        assert_eq!(parse_rate(&[0, 9]), None);
    }
}
