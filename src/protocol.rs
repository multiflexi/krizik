//! Bluetooth LE protocol of the ALIENTEK C2 USB tester.
//!
//! Reverse-engineered from firmware V2.5.0 and verified against a C2 Pro.
//! See `PROTOCOL.md` for the full description.
//!
//! The BLE module is a transparent UART bridge.  The tester exposes GATT
//! service `0xFFF0`; requests are written (without response) to `0xFFF3`
//! (`0xFFF1` also works) and responses arrive as notifications on `0xFFF2`.
//!
//! Request:  `AF 0A 03 <cmd> <len> <data…> <chk>` (identify uses `AF FF FF`)
//! Response: `DF 0A 03 <cmd> <len> <data…> <chk>` (identify uses `DF FF FF`)
//! `chk` is the two's complement of the byte sum of everything before it.

use std::fmt;

pub const SERVICE_UUID: u128 = 0x0000fff0_0000_1000_8000_00805f9b34fb;
pub const NOTIFY_UUID: u128 = 0x0000fff2_0000_1000_8000_00805f9b34fb;
pub const WRITE_UUID: u128 = 0x0000fff3_0000_1000_8000_00805f9b34fb;
pub const WRITE_ALT_UUID: u128 = 0x0000fff1_0000_1000_8000_00805f9b34fb;

pub const REQUEST_HEADER: u8 = 0xAF;
pub const RESPONSE_HEADER: u8 = 0xDF;
const DEVICE_CLASS: [u8; 2] = [0x0A, 0x03];
const BROADCAST_CLASS: [u8; 2] = [0xFF, 0xFF];

/// Maximum length of the Bluetooth name stored by the tester.
pub const MAX_NAME_LEN: usize = 12;
/// Number of samples carried by one realtime block.
pub const SAMPLES_PER_BLOCK: usize = 10;
/// Size of the realtime block payload.
pub const REALTIME_LEN: usize = 199;

/// Commands understood by the tester over BLE.  The firmware treats any other
/// command code like [`Command::Poll`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Command {
    /// Returns the model code; also enables BLE sample buffering.
    Identify = 0x00,
    /// Enables BLE sample buffering, answers with a single `0x00`.
    Start = 0x01,
    /// Returns versions, serial number and model name.
    DeviceInfo = 0x02,
    /// Returns the next block of ten samples.  The tester holds the request
    /// until a new block is ready, so polling naturally follows the sample rate.
    Poll = 0x04,
    /// Stores a new Bluetooth name and restarts the BLE module.
    SetName = 0x0B,
    /// Returns the stored Bluetooth name (12 bytes, NUL padded).
    GetName = 0x0C,
}

impl Command {
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0x00 => Self::Identify,
            0x01 => Self::Start,
            0x02 => Self::DeviceInfo,
            0x04 => Self::Poll,
            0x0B => Self::SetName,
            0x0C => Self::GetName,
            _ => return None,
        })
    }
}

/// Two's complement of the 8-bit sum, as used by all ALIENTEK frames.
pub fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |acc, b| acc.wrapping_add(*b)).wrapping_neg()
}

/// Builds a request frame.
pub fn encode_request(cmd: Command, data: &[u8]) -> Vec<u8> {
    assert!(data.len() <= u8::MAX as usize, "payload too long");
    let class = if cmd == Command::Identify { BROADCAST_CLASS } else { DEVICE_CLASS };
    let mut frame = Vec::with_capacity(6 + data.len());
    frame.push(REQUEST_HEADER);
    frame.extend_from_slice(&class);
    frame.push(cmd as u8);
    frame.push(data.len() as u8);
    frame.extend_from_slice(data);
    frame.push(checksum(&frame));
    frame
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameError {
    Empty,
    TooLong,
    InvalidCharacter(char),
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameError::Empty => write!(f, "The name must not be empty"),
            NameError::TooLong => write!(f, "The name can have at most {MAX_NAME_LEN} characters"),
            NameError::InvalidCharacter(c) => {
                write!(f, "“{c}” is not allowed; use English letters, digits, “-” or “_”")
            }
        }
    }
}

impl std::error::Error for NameError {}

/// The tester passes the name to its BLE module as an AT command, so only
/// plain ASCII without separators is safe.
pub fn validate_name(name: &str) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if let Some(c) = name.chars().find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_')) {
        return Err(NameError::InvalidCharacter(c));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(NameError::TooLong);
    }
    Ok(())
}

/// Builds the "set Bluetooth name" request.  The firmware formats the AT
/// command with `%s` straight from its receive buffer, so the name is sent
/// NUL-terminated.
pub fn encode_set_name(name: &str) -> Result<Vec<u8>, NameError> {
    validate_name(name)?;
    let mut data = name.as_bytes().to_vec();
    data.push(0);
    Ok(encode_request(Command::SetName, &data))
}

/// A complete, checksum-verified response frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub cmd: u8,
    pub data: Vec<u8>,
}

/// Reassembles response frames from notification payloads, which can be
/// split or merged depending on the negotiated MTU.
#[derive(Default, Debug)]
pub struct FrameParser {
    buf: Vec<u8>,
}

impl FrameParser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Frame> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            match self.buf.iter().position(|&b| b == RESPONSE_HEADER) {
                None => {
                    self.buf.clear();
                    break;
                }
                Some(0) => {}
                Some(start) => {
                    self.buf.drain(..start);
                }
            }
            if self.buf.len() < 5 {
                break;
            }
            let total = 5 + self.buf[4] as usize + 1;
            if self.buf.len() < total {
                break;
            }
            if checksum(&self.buf[..total - 1]) == self.buf[total - 1] {
                frames.push(Frame { cmd: self.buf[3], data: self.buf[5..total - 1].to_vec() });
                self.buf.drain(..total);
            } else {
                // Not a real frame start: skip this header byte and resync.
                self.buf.drain(..1);
            }
        }
        frames
    }

    pub fn reset(&mut self) {
        self.buf.clear();
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Model {
    Pro,
    Std,
    Lite,
    Unknown(u8),
}

impl Model {
    pub fn from_code(code: u8) -> Self {
        match code {
            b'A' => Model::Pro,
            b'B' => Model::Std,
            b'C' => Model::Lite,
            other => Model::Unknown(other),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Model::Pro => "C2 Pro".into(),
            Model::Std => "C2 Std".into(),
            Model::Lite => "C2 Lite".into(),
            Model::Unknown(c) => format!("C2 (model 0x{c:02X})"),
        }
    }
}

/// Parses the identify response: `0A 03 <model>`.
pub fn parse_identify(data: &[u8]) -> Option<Model> {
    (data.len() >= 3).then(|| Model::from_code(data[2]))
}

/// Versions are encoded as tenths, e.g. 25 means 2.5.
pub fn format_version(v: u8) -> String {
    format!("{}.{}", v / 10, v % 10)
}

/// The firmware series without the unknown patch level, e.g. "2.4.x".
pub fn format_firmware_series(v: u8) -> String {
    format!("{}.x", format_version(v))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub bootloader_version: u8,
    pub hardware_version: u8,
    /// Major.minor of the installed firmware (tenths, e.g. 24 = 2.4.x).
    /// Bluetooth does not report the patch level.
    pub firmware_series: u8,
    pub model_code: u16,
    pub serial: String,
    pub model_name: String,
    pub is_lite: bool,
}

/// Parses the 29-byte device information block.
///
/// | offset | size | meaning                                   |
/// |--------|------|-------------------------------------------|
/// | 0      | 1    | bootloader version (tenths)               |
/// | 1      | 1    | firmware series (tenths, 24 = any 2.4.x)  |
/// | 2      | 1    | hardware version (tenths)                 |
/// | 3      | 1    | byte 1 + 1, not a version                 |
/// | 4      | 2    | model code, little endian (0x030A)        |
/// | 6      | 8    | serial number (ASCII)                     |
/// | 14     | 12   | model name (ASCII, NUL padded)            |
/// | 26     | 1    | bootloader flag                           |
/// | 27     | 1    | 1 on the C2 Lite                          |
pub fn parse_device_info(data: &[u8]) -> Option<DeviceInfo> {
    if data.len() < 26 {
        return None;
    }
    Some(DeviceInfo {
        bootloader_version: data[0],
        hardware_version: data[2],
        // Byte 1 is the version byte of the firmware header the bootloader
        // received during the last update; byte 3 is merely byte 1 + 1.
        firmware_series: data[1],
        model_code: u16::from_le_bytes([data[4], data[5]]),
        serial: ascii_field(&data[6..14]),
        model_name: ascii_field(&data[14..26]),
        is_lite: data.get(27).copied() == Some(1),
    })
}

/// Parses the 12-byte stored Bluetooth name.
pub fn parse_name(data: &[u8]) -> String {
    ascii_field(&data[..data.len().min(MAX_NAME_LEN)])
}

/// Reads a NUL-terminated ASCII field, dropping non-printable bytes.
pub fn ascii_field(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    bytes[..end]
        .iter()
        .filter(|b| b.is_ascii_graphic() || **b == b' ')
        .map(|&b| b as char)
        .collect::<String>()
        .trim()
        .to_string()
}

/// One measurement taken by the tester.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct RawSample {
    /// VBUS in volts.
    pub voltage: f32,
    /// Current in amperes.
    pub current: f32,
    pub d_plus: f32,
    pub d_minus: f32,
    pub cc1: f32,
    pub cc2: f32,
}

/// Fast-charging protocol details as shown on the tester's own screen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProtocolInfo {
    /// Detected protocol, "Protocols" when none was detected.
    pub name: String,
    /// Total power advertised by the power source.
    pub power: String,
    /// Requested gear / number of gears, "---NA---" when not available.
    pub gear: String,
    pub gear_detail: String,
    /// Type, voltage and current of the requested gear.
    pub request: String,
    pub request_detail: String,
}

/// A realtime block returned by [`Command::Poll`].
#[derive(Clone, Debug, PartialEq)]
pub struct RealtimeBlock {
    pub protocol: ProtocolInfo,
    /// Power bank mode (periodic dummy load) is enabled on the tester.
    pub power_bank_mode: bool,
    /// Interval between samples in milliseconds.
    pub interval_ms: u16,
    pub samples: Vec<RawSample>,
    /// Undocumented status nibble.
    pub status: u8,
    /// Tester temperature in °C (0 when the tester has no sensor).
    pub temperature_c: u8,
}

const STRING_FIELDS: [(usize, usize); 6] = [(0, 10), (10, 18), (18, 26), (26, 34), (34, 44), (44, 55)];
const OFF_POWER_BANK: usize = 55;
const OFF_INTERVAL: usize = 56;
const OFF_VOLTAGE: usize = 57;
const OFF_CURRENT: usize = 87;
const OFF_D_PLUS: usize = 117;
const OFF_D_MINUS: usize = 137;
const OFF_CC1: usize = 157;
const OFF_CC2: usize = 177;
const OFF_STATUS: usize = 197;
const OFF_TEMPERATURE: usize = 198;

pub fn i24(b: &[u8]) -> i32 {
    // Sign-extend; real readings never come close to 2^23 (83 V / 83 A).
    (i32::from_le_bytes([b[0], b[1], b[2], 0]) << 8) >> 8
}

fn u16_at(d: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([d[off], d[off + 1]])
}

/// Parses the 199-byte realtime block.
///
/// Voltage and current are 24-bit little-endian integers in units of 10 µV /
/// 10 µA; D+, D−, CC1 and CC2 are 16-bit millivolts.
pub fn parse_realtime(d: &[u8]) -> Option<RealtimeBlock> {
    if d.len() < REALTIME_LEN {
        return None;
    }
    let s = |i: usize| ascii_field(&d[STRING_FIELDS[i].0..STRING_FIELDS[i].1]);
    let samples = (0..SAMPLES_PER_BLOCK)
        .map(|k| RawSample {
            voltage: i24(&d[OFF_VOLTAGE + 3 * k..]) as f32 / 100_000.0,
            current: i24(&d[OFF_CURRENT + 3 * k..]) as f32 / 100_000.0,
            d_plus: u16_at(d, OFF_D_PLUS + 2 * k) as f32 / 1000.0,
            d_minus: u16_at(d, OFF_D_MINUS + 2 * k) as f32 / 1000.0,
            cc1: u16_at(d, OFF_CC1 + 2 * k) as f32 / 1000.0,
            cc2: u16_at(d, OFF_CC2 + 2 * k) as f32 / 1000.0,
        })
        .collect();
    Some(RealtimeBlock {
        protocol: ProtocolInfo {
            name: s(0),
            power: s(1),
            gear: s(2),
            gear_detail: s(3),
            request: s(4),
            request_detail: s(5),
        },
        power_bank_mode: d[OFF_POWER_BANK] != 0,
        interval_ms: d[OFF_INTERVAL] as u16,
        samples,
        status: d[OFF_STATUS],
        temperature_c: d[OFF_TEMPERATURE],
    })
}

/// A decoded response.
#[derive(Clone, Debug, PartialEq)]
pub enum Response {
    Identify(Model),
    Started,
    DeviceInfo(DeviceInfo),
    Realtime(RealtimeBlock),
    NameStored,
    Name(String),
    Unknown(Frame),
}

impl Response {
    pub fn from_frame(frame: Frame) -> Response {
        let parsed = match Command::from_code(frame.cmd) {
            Some(Command::Identify) => parse_identify(&frame.data).map(Response::Identify),
            Some(Command::Start) => Some(Response::Started),
            Some(Command::DeviceInfo) => parse_device_info(&frame.data).map(Response::DeviceInfo),
            Some(Command::Poll) => parse_realtime(&frame.data).map(Response::Realtime),
            Some(Command::SetName) => Some(Response::NameStored),
            Some(Command::GetName) => Some(Response::Name(parse_name(&frame.data))),
            None => None,
        };
        parsed.unwrap_or(Response::Unknown(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace().map(|b| u8::from_str_radix(b, 16).unwrap()).collect()
    }

    #[test]
    fn requests_match_captured_traffic() {
        assert_eq!(encode_request(Command::Identify, &[]), hex("af ff ff 00 00 53"));
        assert_eq!(encode_request(Command::Start, &[]), hex("af 0a 03 01 00 43"));
        assert_eq!(encode_request(Command::DeviceInfo, &[]), hex("af 0a 03 02 00 42"));
        assert_eq!(encode_request(Command::Poll, &[]), hex("af 0a 03 04 00 40"));
        assert_eq!(encode_request(Command::GetName, &[]), hex("af 0a 03 0c 00 38"));
    }

    #[test]
    fn set_name_is_nul_terminated() {
        let f = encode_set_name("Bench_1").unwrap();
        assert_eq!(&f[..5], &[0xAF, 0x0A, 0x03, 0x0B, 8]);
        assert_eq!(&f[5..13], b"Bench_1\0");
        assert_eq!(checksum(&f[..f.len() - 1]), *f.last().unwrap());
        assert_eq!(encode_set_name(""), Err(NameError::Empty));
        assert_eq!(encode_set_name("ThisIsTooLong"), Err(NameError::TooLong));
        assert_eq!(encode_set_name("a b"), Err(NameError::InvalidCharacter(' ')));
    }

    #[test]
    fn parses_identify_info_and_name() {
        let mut p = FrameParser::default();
        let frames = p.push(&hex("df ff ff 00 03 0a 03 41 d2"));
        assert_eq!(Response::from_frame(frames[0].clone()), Response::Identify(Model::Pro));

        let frames = p.push(&hex(
            "df 0a 03 02 1d 0a 18 0f 19 0a 03 39 33 30 38 46 46 37 46 41 54 4b 2d 43 32 20 50 72 6f 00 00 00 00 00 ee",
        ));
        let Response::DeviceInfo(info) = Response::from_frame(frames[0].clone()) else { panic!() };
        assert_eq!(info.serial, "9308FF7F");
        assert_eq!(info.model_name, "ATK-C2 Pro");
        // The tester runs 2.4.4; Bluetooth only carries the 2.4 series.
        assert_eq!(format_firmware_series(info.firmware_series), "2.4.x");
        assert_eq!(format_version(info.hardware_version), "1.5");
        assert_eq!(format_version(info.bootloader_version), "1.0");
        assert_eq!(info.model_code, 0x030A);
        assert!(!info.is_lite);

        let frames = p.push(&hex("df 0a 03 0c 0c 43 32 5f 42 4c 45 30 33 00 00 00 00 f2"));
        assert_eq!(Response::from_frame(frames[0].clone()), Response::Name("C2_BLE03".into()));
        let frames = p.push(&hex("df 0a 03 01 01 00 12"));
        assert_eq!(Response::from_frame(frames[0].clone()), Response::Started);
    }

    const REALTIME: &str = "df 0a 03 04 c7 50 72 6f 74 6f 63 6f 6c 73 00 00 00 00 00 00 00 00 00 2d 2d 2d 4e 41 2d 2d 2d 00 00 00 00 00 00 00 00 2d 2d 2d 4e 41 2d 2d 2d 00 00 00 00 00 00 00 00 00 00 00 00 00 00 14 4a 02 00 88 00 00 98 02 00 9a 01 00 73 01 00 86 01 00 23 02 00 4a 02 00 fe 00 00 98 02 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 43 00 00 38 00 45 00 38 00 45 00 45 00 36 00 36 00 45 00 45 00 43 00 45 00 46 00 45 00 45 00 45 00 43 00 45 00 43 00 46 00 45 00 38 00 3b 00 3a 00 3b 00 3a 00 3a 00 3c 00 3b 00 37 00 3b 00 37 00 37 00 38 00 38 00 38 00 36 00 37 00 37 00 39 00 36 00 01 1c 33";

    #[test]
    fn parses_realtime_block() {
        let mut p = FrameParser::default();
        let frames = p.push(&hex(REALTIME));
        assert_eq!(frames.len(), 1);
        let Response::Realtime(b) = Response::from_frame(frames[0].clone()) else { panic!() };
        assert_eq!(b.protocol.name, "Protocols");
        assert_eq!(b.protocol.gear, "---NA---");
        assert_eq!(b.protocol.request, "---NA---");
        assert_eq!(b.interval_ms, 20);
        assert_eq!(b.samples.len(), SAMPLES_PER_BLOCK);
        assert!((b.samples[0].voltage - 0.00586).abs() < 1e-6);
        assert_eq!(b.samples[0].current, 0.0);
        assert!((b.samples[9].current - 0.00067).abs() < 1e-8);
        assert!((b.samples[0].d_plus - 0.056).abs() < 1e-6);
        assert!((b.samples[0].d_minus - 0.069).abs() < 1e-6);
        assert!((b.samples[0].cc1 - 0.056).abs() < 1e-6);
        assert!((b.samples[0].cc2 - 0.055).abs() < 1e-6);
        assert_eq!(b.temperature_c, 28);
        assert!(!b.power_bank_mode);
    }

    #[test]
    fn reassembles_fragmented_and_merged_frames() {
        let frame = hex(REALTIME);
        let mut p = FrameParser::default();
        let mut got = Vec::new();
        // Garbage, then the frame in 20-byte chunks (default ATT MTU), then a
        // second frame glued to the end of the last chunk.
        got.extend(p.push(&[0x00, 0x13, 0xDF]));
        for chunk in frame.chunks(20) {
            got.extend(p.push(chunk));
        }
        got.extend(p.push(&hex("df 0a 03 01 01 00 12 df 0a 03")));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].cmd, 0x04);
        assert_eq!(got[0].data.len(), REALTIME_LEN);
        assert_eq!(got[1].cmd, 0x01);
        // The partial frame is completed by the next notification.
        let rest = p.push(&hex("0c 0c 43 32 5f 42 4c 45 30 33 00 00 00 00 f2"));
        assert_eq!(rest.len(), 1);
    }

    #[test]
    fn rejects_corrupted_frames() {
        let mut frame = hex(REALTIME);
        frame[100] ^= 0x55;
        let mut p = FrameParser::default();
        assert!(p.push(&frame).is_empty());
        assert_eq!(p.push(&hex("df 0a 03 01 01 00 12")).len(), 1);
    }

    #[test]
    fn negative_values_are_sign_extended() {
        assert_eq!(i24(&[0xFF, 0xFF, 0xFF]), -1);
        assert_eq!(i24(&[0x40, 0x42, 0x0F]), 1_000_000);
    }
}
