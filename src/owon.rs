//! Bluetooth LE protocol of OWON multimeters (B35T+, B41T+, OW18 and their
//! rebrands), as used by OWON's "Multimeter BLE" and "iMeter" apps.
//!
//! See `METER_PROTOCOL.md` for the full description.  In short, the meter
//! offers GATT service `0xFFF0` (the same UUID as the C2 tester) with:
//!
//! * `0xFFF1`: authentication handshake and 16-byte text commands,
//! * `0xFFF2`: device information (series, battery, firmware, log state),
//! * `0xFFF3`: remote button presses,
//! * `0xFFF4`: notifications with the measurements (and the offline log).

use std::fmt;
use std::io::{self, Write};

use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};
use gtk::glib;

pub const AUTH_UUID: u128 = 0x0000fff1_0000_1000_8000_00805f9b34fb;
pub const INFO_UUID: u128 = 0x0000fff2_0000_1000_8000_00805f9b34fb;
pub const BUTTON_UUID: u128 = 0x0000fff3_0000_1000_8000_00805f9b34fb;
pub const DATA_UUID: u128 = 0x0000fff4_0000_1000_8000_00805f9b34fb;

/// Length of every text command written to [`AUTH_UUID`].
pub const COMMAND_LEN: usize = 16;
/// Longest Bluetooth name the meter accepts.
pub const MAX_NAME_LEN: usize = 15;
/// Limits of the "start recording" command, as enforced by the vendor app.
pub const MAX_RECORD_INTERVAL: u32 = 4_200_000;
pub const MAX_RECORD_COUNT: u32 = 10_000;

// ------------------------------------------------------------ device info

/// Meter family, from the first byte of the information characteristic.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Series {
    B33,
    B35,
    B41,
    Ow18,
    Ow20,
    Ow55,
    Unknown(u8),
}

impl Series {
    pub fn from_id(id: u8) -> Self {
        match id {
            0x21 => Self::B33,
            0x23 => Self::B35,
            0x29 => Self::B41,
            0x12 => Self::Ow18,
            0x14 => Self::Ow20,
            0x37 => Self::Ow55,
            other => Self::Unknown(other),
        }
    }

    /// Whether the meter has a Max/Min button that can be pressed remotely.
    pub fn has_max_min(self) -> bool {
        matches!(self, Self::B35 | Self::B41)
    }
}

/// State of the meter's offline log.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LogState {
    Unsupported,
    Idle,
    Recording,
}

/// Contents of the information characteristic (`0xFFF2`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    pub series: Series,
    /// Battery level in percent, if the meter reports one.
    pub battery: Option<u8>,
    /// Firmware version as major, minor, patch.
    pub firmware: Option<[u8; 3]>,
    pub log: LogState,
}

impl Info {
    /// The firmware version as the vendor app compares it (1.2.3 → 123).
    pub fn firmware_code(&self) -> u32 {
        self.firmware.map_or(0, |[a, b, c]| a as u32 * 100 + b as u32 * 10 + c as u32)
    }

    pub fn firmware_text(&self) -> Option<String> {
        self.firmware.map(|[a, b, c]| format!("{a}.{b}.{c}"))
    }

    pub fn can_rename(&self) -> bool {
        self.firmware_code() >= 11
    }

    /// Newer firmware keeps the start time and interval with the log.
    pub fn log_has_date(&self) -> bool {
        self.firmware_code() >= 12
    }

    pub fn supports_log(&self) -> bool {
        self.log != LogState::Unsupported
    }

    pub fn dialect(&self) -> Dialect {
        Dialect {
            legacy: self.series == Series::B35 && !self.supports_log(),
            ow55: self.series == Series::Ow55,
            ncv: matches!(self.series, Series::Ow18 | Series::Ow20 | Series::Ow55),
        }
    }

    pub fn model_name(&self) -> String {
        match self.series {
            Series::B33 => "OWON B33+".into(),
            Series::B35 if self.dialect().legacy => "OWON B35T".into(),
            Series::B35 => "OWON B35T+".into(),
            Series::B41 => "OWON B41T+".into(),
            Series::Ow18 => "OWON OW18".into(),
            Series::Ow20 => "OWON OW20".into(),
            Series::Ow55 => "OWON OW55".into(),
            Series::Unknown(id) => format!("OWON meter (series 0x{id:02X})"),
        }
    }
}

/// Parses the information characteristic.  Returns `None` for an empty read.
pub fn parse_info(data: &[u8]) -> Option<Info> {
    let series = Series::from_id(*data.first()?);
    let log = match data.get(5) {
        Some(0x00) => LogState::Idle,
        Some(0x01) => LogState::Recording,
        _ => LogState::Unsupported,
    };
    // The vendor app only trusts the battery byte on meters with a log.
    let battery = data.get(1).copied().filter(|b| *b <= 100 && log != LogState::Unsupported);
    let firmware = data.get(2..5).map(|v| [v[0], v[1], v[2]]);
    Some(Info { series, battery, firmware, log })
}

// ------------------------------------------------------------ readings

/// Variations of the frame format between meter families.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Dialect {
    /// Original B35T: 14-byte ASCII frames instead of 6-byte binary ones.
    pub legacy: bool,
    /// OW55: the sign lives in the function word and the value is signed.
    pub ow55: bool,
    /// Function 13 means non-contact voltage (otherwise "ADP").
    pub ncv: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Function {
    DcVoltage,
    AcVoltage,
    DcCurrent,
    AcCurrent,
    Resistance,
    Capacitance,
    Frequency,
    Duty,
    Celsius,
    Fahrenheit,
    Diode,
    Continuity,
    Hfe,
    Ncv,
    Adp,
    Unknown(u8),
}

impl Function {
    fn from_code(code: u16, d: Dialect) -> Self {
        match code {
            0 => Self::DcVoltage,
            1 => Self::AcVoltage,
            2 => Self::DcCurrent,
            3 => Self::AcCurrent,
            4 => Self::Resistance,
            5 => Self::Capacitance,
            6 => Self::Frequency,
            7 => Self::Duty,
            8 => Self::Celsius,
            9 => Self::Fahrenheit,
            10 => Self::Diode,
            11 => Self::Continuity,
            12 => Self::Hfe,
            13 if d.ncv => Self::Ncv,
            13 => Self::Adp,
            other => Self::Unknown(other as u8),
        }
    }

    /// Unit without SI prefix.
    pub fn base_unit(self) -> &'static str {
        match self {
            Self::DcVoltage | Self::AcVoltage | Self::Diode => "V",
            Self::DcCurrent | Self::AcCurrent => "A",
            Self::Resistance | Self::Continuity => "Ω",
            Self::Capacitance => "F",
            Self::Frequency => "Hz",
            Self::Duty => "%",
            Self::Celsius => "°C",
            Self::Fahrenheit => "°F",
            Self::Hfe | Self::Ncv | Self::Adp | Self::Unknown(_) => "",
        }
    }

    /// Whether the reading's SI prefix applies to this function.
    pub fn uses_prefix(self) -> bool {
        !matches!(self, Self::Duty | Self::Hfe | Self::Ncv | Self::Adp | Self::Unknown(_))
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::DcVoltage => "DC Voltage",
            Self::AcVoltage => "AC Voltage",
            Self::DcCurrent => "DC Current",
            Self::AcCurrent => "AC Current",
            Self::Resistance => "Resistance",
            Self::Capacitance => "Capacitance",
            Self::Frequency => "Frequency",
            Self::Duty => "Duty Cycle",
            Self::Celsius | Self::Fahrenheit => "Temperature",
            Self::Diode => "Diode",
            Self::Continuity => "Continuity",
            Self::Hfe => "Transistor hFE",
            Self::Ncv => "Non-Contact Voltage",
            Self::Adp => "ADP",
            Self::Unknown(_) => "Unknown Function",
        }
    }

    /// Short label for graph titles.
    pub fn short(self) -> &'static str {
        match self {
            Self::DcVoltage => "DC V",
            Self::AcVoltage => "AC V",
            Self::DcCurrent => "DC A",
            Self::AcCurrent => "AC A",
            Self::Resistance => "Ω",
            Self::Capacitance => "Capacitance",
            Self::Frequency => "Frequency",
            Self::Duty => "Duty",
            Self::Celsius => "°C",
            Self::Fahrenheit => "°F",
            Self::Diode => "Diode",
            Self::Continuity => "Continuity",
            Self::Hfe => "hFE",
            Self::Ncv => "NCV",
            Self::Adp => "ADP",
            Self::Unknown(_) => "?",
        }
    }

    /// Stable identifier for CSV files.
    pub fn key(self) -> &'static str {
        match self {
            Self::DcVoltage => "DCV",
            Self::AcVoltage => "ACV",
            Self::DcCurrent => "DCA",
            Self::AcCurrent => "ACA",
            Self::Resistance => "RES",
            Self::Capacitance => "CAP",
            Self::Frequency => "FREQ",
            Self::Duty => "DUTY",
            Self::Celsius => "TEMP_C",
            Self::Fahrenheit => "TEMP_F",
            Self::Diode => "DIODE",
            Self::Continuity => "CONT",
            Self::Hfe => "HFE",
            Self::Ncv => "NCV",
            Self::Adp => "ADP",
            Self::Unknown(_) => "UNKNOWN",
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Prefix {
    Pico,
    Nano,
    Micro,
    Milli,
    None,
    Kilo,
    Mega,
    Giga,
}

impl Prefix {
    fn from_code(code: u16) -> Self {
        [Self::Pico, Self::Nano, Self::Micro, Self::Milli, Self::None, Self::Kilo, Self::Mega, Self::Giga]
            [(code & 7) as usize]
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Self::Pico => "p",
            Self::Nano => "n",
            Self::Micro => "µ",
            Self::Milli => "m",
            Self::None => "",
            Self::Kilo => "k",
            Self::Mega => "M",
            Self::Giga => "G",
        }
    }

    pub fn exponent(self) -> i32 {
        match self {
            Self::Pico => -12,
            Self::Nano => -9,
            Self::Micro => -6,
            Self::Milli => -3,
            Self::None => 0,
            Self::Kilo => 3,
            Self::Mega => 6,
            Self::Giga => 9,
        }
    }
}

/// Position of the decimal point, or a special display state.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Point {
    /// Number of digits after the decimal point (0–4).
    Decimals(u8),
    Error,
    Underload,
    Overload,
}

impl Point {
    fn from_code(code: u16) -> Self {
        match code & 7 {
            n @ 0..=4 => Self::Decimals(n as u8),
            5 => Self::Error,
            6 => Self::Underload,
            _ => Self::Overload,
        }
    }
}

/// Status flags of a reading (bits of the second frame word).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Flags(pub u16);

impl Flags {
    pub const HOLD: u16 = 1 << 0;
    pub const REL: u16 = 1 << 1;
    pub const AUTO: u16 = 1 << 2;
    pub const LOW_BATTERY: u16 = 1 << 3;
    pub const MIN: u16 = 1 << 4;
    pub const MAX: u16 = 1 << 5;
    pub const PEAK_MIN: u16 = 1 << 8;
    pub const PEAK_MAX: u16 = 1 << 9;
    pub const LPF: u16 = (1 << 11) | (1 << 12);
    const NAMES: [(u16, &'static str); 9] = [
        (Self::AUTO, "AUTO"),
        (Self::HOLD, "HOLD"),
        (Self::REL, "REL"),
        (Self::MIN, "MIN"),
        (Self::MAX, "MAX"),
        (Self::PEAK_MIN, "PMIN"),
        (Self::PEAK_MAX, "PMAX"),
        (Self::LPF, "LPF"),
        (Self::LOW_BATTERY, "BAT"),
    ];

    pub fn has(self, bit: u16) -> bool {
        self.0 & bit != 0
    }

    /// Names of the indicators shown on the meter's display.
    pub fn names(self) -> Vec<&'static str> {
        Self::NAMES.iter().filter(|(bit, _)| self.has(*bit)).map(|(_, n)| *n).collect()
    }
}

/// One decoded reading, as shown on the meter's display.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Reading {
    pub function: Function,
    pub prefix: Prefix,
    pub point: Point,
    pub negative: bool,
    /// Displayed digits as an integer, without sign or decimal point.
    pub raw: i32,
    pub flags: Flags,
}

impl Reading {
    fn signed_raw(&self) -> i32 {
        if self.negative {
            -self.raw
        } else {
            self.raw
        }
    }

    fn scale(&self) -> f64 {
        if self.function.uses_prefix() {
            10f64.powi(self.prefix.exponent())
        } else {
            1.0
        }
    }

    /// The displayed value in the displayed unit, e.g. −1.234 for "−1.234 mV".
    pub fn value(&self) -> Option<f64> {
        match self.point {
            Point::Decimals(d) => Some(self.signed_raw() as f64 / 10f64.powi(d as i32)),
            _ => None,
        }
    }

    /// The value in the SI base unit (V, A, Ω, F, Hz…), e.g. −0.001234.
    pub fn value_si(&self) -> Option<f64> {
        Some(self.value()? * self.scale())
    }

    /// Smallest step of the current range in the SI base unit.
    pub fn resolution_si(&self) -> f64 {
        let d = match self.point {
            Point::Decimals(d) => d as i32,
            _ => 0,
        };
        10f64.powi(-d) * self.scale()
    }

    /// The displayed number, e.g. "-1.234", "OL" or "UL".
    pub fn display(&self) -> String {
        if self.function == Function::Ncv {
            return match self.raw {
                0 => "EF".into(),
                n @ 1..=4 => "-".repeat(n as usize),
                n => n.to_string(),
            };
        }
        match self.point {
            Point::Overload if self.negative => "-OL".into(),
            Point::Overload => "OL".into(),
            Point::Underload => "UL".into(),
            Point::Error => self.signed_raw().to_string(),
            Point::Decimals(d) => decimal_text(self.raw, self.negative, d as i32),
        }
    }

    /// The value in the SI base unit as exact decimal text, e.g. "-0.001234".
    pub fn value_si_text(&self) -> Option<String> {
        let Point::Decimals(d) = self.point else { return None };
        let exponent = if self.function.uses_prefix() { self.prefix.exponent() } else { 0 };
        Some(decimal_text(self.raw, self.negative, d as i32 - exponent))
    }

    /// The displayed unit including prefix, e.g. "mV" or "kΩ".
    pub fn unit(&self) -> String {
        if self.function.uses_prefix() {
            format!("{}{}", self.prefix.symbol(), self.function.base_unit())
        } else {
            self.function.base_unit().to_string()
        }
    }
}

impl fmt::Display for Reading {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let unit = self.unit();
        if unit.is_empty() {
            write!(f, "{} {}", self.display(), self.function.short())
        } else {
            write!(f, "{} {} {}", self.display(), unit, self.function.short())
        }
    }
}

/// Formats `raw / 10^places` without rounding errors.
fn decimal_text(raw: i32, negative: bool, places: i32) -> String {
    let sign = if negative { "-" } else { "" };
    let raw = raw.unsigned_abs();
    if places <= 0 {
        return format!("{sign}{raw}{}", "0".repeat(if raw == 0 { 0 } else { (-places) as usize }));
    }
    let places = places as usize;
    let digits = format!("{raw:0width$}", width = places + 1);
    let (int, frac) = digits.split_at(digits.len() - places);
    format!("{sign}{int}.{frac}")
}

fn word(lo: u8, hi: u8) -> u16 {
    u16::from_le_bytes([lo, hi])
}

/// Decodes one 6-byte binary frame: function word, flag word, value word.
pub fn decode_binary(frame: &[u8; 6], d: Dialect) -> Reading {
    let w0 = word(frame[0], frame[1]);
    let w1 = word(frame[2], frame[3]);
    let w2 = word(frame[4], frame[5]);
    let (negative, raw) =
        if d.ow55 { ((w0 >> 10) & 1 == 1, w2 as i16 as i32) } else { (w2 & 0x8000 != 0, (w2 & 0x7FFF) as i32) };
    Reading {
        function: Function::from_code((w0 >> 6) & 0x0F, d),
        prefix: Prefix::from_code(w0 >> 3),
        point: Point::from_code(w0),
        negative,
        raw,
        flags: Flags(w1 & 0x3FFF),
    }
}

const LEGACY_LEN: usize = 14;

/// Decodes one 14-byte ASCII frame of the original B35T (FS9922 style):
/// `±dddd ␠ point SB1 SB2 SB3 SB4 bar CR LF`.  Returns `None` if the frame
/// is malformed.
pub fn decode_legacy(f: &[u8]) -> Option<Reading> {
    if f.len() < LEGACY_LEN || !matches!(f[0], b'+' | b'-') || f[5] != b' ' || f[12] != b'\r' || f[13] != b'\n' {
        return None;
    }
    let digits = &f[1..5];
    let overload = digits.iter().any(|b| !b.is_ascii_digit());
    let raw = if overload { 0 } else { digits.iter().fold(0i32, |acc, b| acc * 10 + (b - b'0') as i32) };
    let (sb1, sb2, sb3, sb4) = (f[7], f[8], f[9], f[10]);
    let decimals = match f[6] {
        b'1' => 3,
        b'2' => 2,
        b'4' => 1,
        _ => 0,
    };
    let ac = sb1 & 0x08 != 0;
    let function = if sb4 & 0x80 != 0 {
        if sb3 & 0x04 != 0 {
            Function::Diode
        } else if ac {
            Function::AcVoltage
        } else {
            Function::DcVoltage
        }
    } else if sb4 & 0x40 != 0 {
        if ac {
            Function::AcCurrent
        } else {
            Function::DcCurrent
        }
    } else if sb4 & 0x20 != 0 {
        if sb3 & 0x08 != 0 {
            Function::Continuity
        } else {
            Function::Resistance
        }
    } else if sb3 & 0x02 != 0 {
        Function::Duty
    } else if sb4 & 0x08 != 0 {
        Function::Frequency
    } else if sb4 & 0x04 != 0 {
        Function::Capacitance
    } else if sb4 & 0x02 != 0 {
        Function::Celsius
    } else if sb4 & 0x01 != 0 {
        Function::Fahrenheit
    } else if sb4 & 0x10 != 0 {
        Function::Hfe
    } else if sb3 & 0x04 != 0 {
        Function::Diode
    } else if sb3 & 0x08 != 0 {
        Function::Continuity
    } else {
        Function::Unknown(0xFF)
    };
    let prefix = if sb2 & 0x02 != 0 {
        Prefix::Nano
    } else if sb3 & 0x80 != 0 {
        Prefix::Micro
    } else if sb3 & 0x40 != 0 {
        Prefix::Milli
    } else if sb3 & 0x20 != 0 {
        Prefix::Kilo
    } else if sb3 & 0x10 != 0 {
        Prefix::Mega
    } else {
        Prefix::None
    };
    let mut flags = 0;
    for (byte, mask, flag) in [
        (sb1, 0x20, Flags::AUTO),
        (sb1, 0x04, Flags::REL),
        (sb1, 0x02, Flags::HOLD),
        (sb2, 0x20, Flags::MAX),
        (sb2, 0x10, Flags::MIN),
        (sb2, 0x04, Flags::LOW_BATTERY),
    ] {
        if byte & mask != 0 {
            flags |= flag;
        }
    }
    Some(Reading {
        function,
        prefix,
        point: if overload { Point::Overload } else { Point::Decimals(decimals) },
        negative: f[0] == b'-',
        raw,
        flags: Flags(flags),
    })
}

/// Reassembles measurement frames from notifications.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    dialect: Dialect,
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new(dialect: Dialect) -> Self {
        FrameDecoder { dialect, buf: Vec::new() }
    }

    pub fn reset(&mut self) {
        self.buf.clear();
    }

    pub fn push(&mut self, data: &[u8]) -> Vec<Reading> {
        if self.dialect.legacy {
            return self.push_legacy(data);
        }
        // Binary frames have no header to resynchronise on.  Notifications
        // normally carry whole frames, so a leftover in front of a
        // notification that is itself made of whole frames is a fragment of a
        // lost notification.
        if !self.buf.is_empty() && !data.is_empty() && data.len().is_multiple_of(6) {
            log::debug!("Dropping {} stray meter bytes", self.buf.len());
            self.buf.clear();
        }
        self.buf.extend_from_slice(data);
        let (frames, _) = self.buf.as_chunks::<6>();
        let out = frames.iter().map(|f| decode_binary(f, self.dialect)).collect();
        let whole = frames.len() * 6;
        self.buf.drain(..whole);
        out
    }

    fn push_legacy(&mut self, data: &[u8]) -> Vec<Reading> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            // Skip to the next possible frame start.
            let start = self.buf.iter().position(|b| matches!(b, b'+' | b'-')).unwrap_or(self.buf.len());
            self.buf.drain(..start);
            if self.buf.len() < LEGACY_LEN {
                break;
            }
            match decode_legacy(&self.buf[..LEGACY_LEN]) {
                Some(r) => {
                    out.push(r);
                    self.buf.drain(..LEGACY_LEN);
                }
                None => {
                    self.buf.remove(0);
                }
            }
        }
        out
    }
}

// ------------------------------------------------------------ authentication

const MIX: [u8; 6] = [200, 100, 50, 20, 10, 5];
const KEY1: &[u8; 36] = b"zyx0wv1uts2rq3pon4ml5kji6hg7fed8cb9a";
const KEY2: &[u8; 36] = b"ab9cde8fg7hij6kl5mno4pq3rst2uv1wxy0z";

/// The identity check the vendor apps perform after connecting: a scrambled
/// random challenge is written to `0xFFF1`, and reading `0xFFF1` back must
/// return the MD5 of a string derived from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Challenge {
    pub request: Vec<u8>,
    pub expected: [u8; 16],
}

impl Challenge {
    /// `coords` must be in 0..36.  The original B35T takes only 6 bytes.
    pub fn new(coords: [u8; 6], legacy: bool) -> Self {
        let mut request: Vec<u8> = coords.iter().zip(MIX).map(|(c, m)| c.wrapping_add(m)).collect();
        if !legacy {
            request.resize(COMMAND_LEN, 0);
        }
        let text: Vec<u8> = coords
            .iter()
            .enumerate()
            .map(|(i, c)| if i < 3 { KEY1[*c as usize % 36] } else { KEY2[*c as usize % 36] })
            .collect();
        Challenge { request, expected: md5(&text) }
    }

    pub fn random(legacy: bool) -> Self {
        let coords = std::array::from_fn(|_| glib::random_int_range(0, 36) as u8);
        Self::new(coords, legacy)
    }

    pub fn verify(&self, reply: &[u8]) -> bool {
        reply == self.expected
    }
}

fn md5(data: &[u8]) -> [u8; 16] {
    let hex = glib::compute_checksum_for_data(glib::ChecksumType::Md5, data).expect("MD5 is supported");
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex digest");
    }
    out
}

// ------------------------------------------------------------ commands

/// Remote-control buttons, written to [`BUTTON_UUID`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Button {
    Select = 1,
    Range = 2,
    Hold = 3,
    Rel = 4,
    HzDuty = 5,
    MaxMin = 6,
}

impl Button {
    pub fn label(self) -> &'static str {
        match self {
            Self::Select => "Select",
            Self::Range => "Range",
            Self::Hold => "Hold",
            Self::Rel => "Rel",
            Self::HzDuty => "Hz/%",
            Self::MaxMin => "Max/Min",
        }
    }
}

/// A short or long press of a button.
pub fn button_frame(button: Button, long: bool) -> [u8; 2] {
    [button as u8, if long { 0 } else { 1 }]
}

fn text_command(parts: &[&[u8]]) -> [u8; COMMAND_LEN] {
    let mut out = [0u8; COMMAND_LEN];
    let mut i = 0;
    for p in parts {
        out[i..i + p.len()].copy_from_slice(p);
        i += p.len();
    }
    out
}

/// Asks for the size of the offline log; the answer is read from `0xFFF1`.
pub fn cmd_log_length() -> [u8; COMMAND_LEN] {
    text_command(&[b"*READlen?"])
}

/// Starts sending the offline log over `0xFFF4`.
pub fn cmd_read_log(dated: bool) -> [u8; COMMAND_LEN] {
    text_command(&[if dated { b"*READ1?" } else { b"*READ?" }])
}

pub fn cmd_stop_recording() -> [u8; COMMAND_LEN] {
    text_command(&[b"*STOP"])
}

/// Starts recording `count` readings, one every `interval_s` seconds.
pub fn cmd_record(interval_s: u32, count: u32) -> [u8; COMMAND_LEN] {
    text_command(&[b"*RECOrd,", &interval_s.to_le_bytes(), &count.to_le_bytes()])
}

/// Sets the meter's clock, used as the log's start time.
pub fn cmd_set_date(t: NaiveDateTime) -> [u8; COMMAND_LEN] {
    // The vendor app formats the hour as 1–24.
    let hour = if t.hour() == 0 { 24 } else { t.hour() };
    let fields = [
        (t.year() / 100) as u8,
        (t.year() % 100) as u8,
        t.month() as u8,
        t.day() as u8,
        hour as u8,
        t.minute() as u8,
        t.second() as u8,
    ];
    text_command(&[b"*DATe", &fields])
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
                write!(f, "“{c}” is not allowed; use English letters, digits or “_”")
            }
        }
    }
}

impl std::error::Error for NameError {}

pub fn validate_name(name: &str) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if let Some(c) = name.chars().find(|c| !(c.is_ascii_alphanumeric() || *c == '_')) {
        return Err(NameError::InvalidCharacter(c));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(NameError::TooLong);
    }
    Ok(())
}

pub fn cmd_rename(name: &str) -> Result<[u8; COMMAND_LEN], NameError> {
    validate_name(name)?;
    Ok(text_command(&[b"@", name.as_bytes()]))
}

// ------------------------------------------------------------ offline log

/// Number of `FF FF` words that open and close the log transfer.
const LOG_MARKS: u32 = 10;

/// Collects the offline log from `0xFFF4` notifications.
#[derive(Debug)]
pub struct LogDecoder {
    dated: bool,
    dialect: Dialect,
    odd: Option<u8>,
    marks: u32,
    capturing: bool,
    done: bool,
    buf: Vec<u8>,
}

impl LogDecoder {
    pub fn new(dated: bool, dialect: Dialect) -> Self {
        LogDecoder { dated, dialect, odd: None, marks: 0, capturing: false, done: false, buf: Vec::new() }
    }

    /// Adds a notification; returns true once the transfer is complete.
    pub fn push(&mut self, data: &[u8]) -> bool {
        let mut bytes = data.iter().copied();
        while !self.done {
            let lo = match self.odd.take() {
                Some(b) => b,
                None => match bytes.next() {
                    Some(b) => b,
                    None => break,
                },
            };
            let Some(hi) = bytes.next() else {
                self.odd = Some(lo);
                break;
            };
            let mark = lo == 0xFF && hi == 0xFF;
            if mark && !self.capturing {
                self.marks += 1;
                self.capturing = self.marks >= LOG_MARKS;
            } else if mark {
                self.marks -= 1;
                if self.marks == 0 {
                    self.capturing = false;
                    self.done = true;
                }
            } else if self.capturing {
                self.buf.extend_from_slice(&[lo, hi]);
            }
        }
        self.done
    }

    /// Bytes received so far, comparable with the log length.
    pub fn received(&self) -> usize {
        self.buf.len()
    }

    pub fn finish(&self) -> Result<Log, String> {
        let header = if self.dated { 16 } else { 4 };
        if self.buf.is_empty() {
            return Ok(Log::default());
        }
        if self.buf.len() < header {
            return Err(format!("The log is too short ({} bytes)", self.buf.len()));
        }
        let b = &self.buf;
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let (start, interval_s) = if self.dated {
            let date = NaiveDate::from_ymd_opt(b[0] as i32 * 100 + b[1] as i32, b[2] as u32, b[3] as u32);
            let start = date.and_then(|d| d.and_hms_opt(b[4] as u32 % 24, b[5] as u32, b[6] as u32));
            (start, Some(u32_at(8)))
        } else {
            (None, None)
        };
        // A value before the first function word should not happen; assume DC V.
        let mut gear: u16 = 0xF020;
        let mut records = Vec::new();
        for w in b[header..].as_chunks::<2>().0 {
            if w[1] & 0xFC == 0xF0 {
                gear = word(w[0], w[1]);
                continue;
            }
            let g = gear.to_le_bytes();
            let reading = decode_binary(&[g[0], g[1], 0, 0, w[0], w[1]], self.dialect);
            let n = records.len() as i64;
            let time = match (start, interval_s) {
                (Some(s), Some(i)) => s.checked_add_signed(chrono::Duration::seconds(i as i64 * (n + 1))),
                _ => None,
            };
            records.push(LogRecord { time, reading });
        }
        Ok(Log { start, interval_s, records })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRecord {
    pub time: Option<NaiveDateTime>,
    pub reading: Reading,
}

/// Readings downloaded from the meter's offline log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Log {
    pub start: Option<NaiveDateTime>,
    pub interval_s: Option<u32>,
    pub records: Vec<LogRecord>,
}

impl Log {
    pub const CSV_HEADER: &'static str = "index,timestamp,function,value,unit,value_si,si_unit,flags";

    pub fn write_csv(&self, mut w: impl Write) -> io::Result<()> {
        writeln!(w, "{}", Self::CSV_HEADER)?;
        for (i, r) in self.records.iter().enumerate() {
            let rd = &r.reading;
            writeln!(
                w,
                "{},{},{},{},{},{},{},{}",
                i,
                r.time.map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default(),
                rd.function.key(),
                rd.display(),
                rd.unit(),
                rd.value_si_text().unwrap_or_default(),
                rd.function.base_unit(),
                rd.flags.names().join(" "),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace().map(|b| u8::from_str_radix(b, 16).unwrap()).collect()
    }

    const B41: Dialect = Dialect { legacy: false, ow55: false, ncv: false };

    #[test]
    fn challenge_matches_vendor_app() {
        let c = Challenge::new([0; 6], false);
        assert_eq!(c.request, hex("c8 64 32 14 0a 05 00 00 00 00 00 00 00 00 00 00"));
        assert_eq!(c.expected.to_vec(), hex("4E 2F 41 BD 78 31 3C 12 8F 13 16 ED 4C B5 16 9D"));
        assert!(c.verify(&c.expected));
        assert!(!c.verify(&[0; 16]));

        let c = Challenge::new([1, 2, 3, 4, 5, 6], true);
        assert_eq!(c.request, hex("c9 66 35 18 0f 0b"));
        assert_eq!(c.expected.to_vec(), hex("2A CA AD 8E 09 BB EB F7 1D 30 7B A9 89 32 D6 1D"));

        let r = Challenge::random(false);
        assert_eq!(r.request.len(), COMMAND_LEN);
    }

    #[test]
    fn parses_info() {
        let i = parse_info(&hex("29 50 01 02 03 00")).unwrap();
        assert_eq!(i.series, Series::B41);
        assert_eq!(i.battery, Some(80));
        assert_eq!(i.firmware_text().as_deref(), Some("1.2.3"));
        assert_eq!(i.firmware_code(), 123);
        assert!(i.can_rename() && i.log_has_date() && i.supports_log());
        assert_eq!(i.log, LogState::Idle);
        assert_eq!(i.dialect(), B41);
        assert_eq!(i.model_name(), "OWON B41T+");

        let i = parse_info(&hex("23 50 00 01 00 ff")).unwrap();
        assert_eq!(i.log, LogState::Unsupported);
        assert_eq!(i.battery, None);
        assert!(i.dialect().legacy);
        assert!(!i.can_rename());
        assert_eq!(i.model_name(), "OWON B35T");

        let i = parse_info(&hex("23 64 00 01 01 01")).unwrap();
        assert_eq!(i.log, LogState::Recording);
        assert!(!i.dialect().legacy && i.can_rename() && !i.log_has_date());

        assert_eq!(parse_info(&[0x12]).unwrap().dialect(), Dialect { legacy: false, ow55: false, ncv: true });
        assert!(parse_info(&[]).is_none());
    }

    #[test]
    fn decodes_binary_frames() {
        let r = decode_binary(&hex("1B F0 04 00 D2 84").try_into().unwrap(), B41);
        assert_eq!(r.function, Function::DcVoltage);
        assert_eq!(r.prefix, Prefix::Milli);
        assert_eq!(r.point, Point::Decimals(3));
        assert!(r.negative);
        assert_eq!(r.raw, 1234);
        assert!(r.flags.has(Flags::AUTO) && !r.flags.has(Flags::HOLD));
        assert_eq!(r.flags.names(), ["AUTO"]);
        assert_eq!(r.display(), "-1.234");
        assert_eq!(r.unit(), "mV");
        assert_eq!(r.to_string(), "-1.234 mV DC V");
        assert!((r.value().unwrap() + 1.234).abs() < 1e-12);
        assert!((r.value_si().unwrap() + 0.001234).abs() < 1e-12);
        assert!((r.resolution_si() - 1e-6).abs() < 1e-15);
        assert_eq!(r.value_si_text().as_deref(), Some("-0.001234"));

        // OW55 carries the sign in the function word.
        let ow55 = Dialect { legacy: false, ow55: true, ncv: true };
        let r = decode_binary(&hex("1B F4 04 00 D2 04").try_into().unwrap(), ow55);
        assert_eq!(r.display(), "-1.234");

        // Overload on the kΩ range, HOLD set.
        let r = decode_binary(&hex("2F F1 01 00 00 00").try_into().unwrap(), B41);
        assert_eq!(r.function, Function::Resistance);
        assert_eq!(r.point, Point::Overload);
        assert_eq!(r.display(), "OL");
        assert_eq!(r.unit(), "kΩ");
        assert_eq!(r.value_si(), None);
        assert_eq!(r.value_si_text(), None);
        assert!(r.flags.has(Flags::HOLD));
        // kΩ and MΩ shift the point to the right.
        let r = decode_binary(&hex("2B F1 00 00 5E 12").try_into().unwrap(), B41);
        assert_eq!((r.display().as_str(), r.value_si_text().as_deref()), ("4.702", Some("4702")));
        let r = decode_binary(&hex("31 F1 00 00 0C 00").try_into().unwrap(), B41);
        assert_eq!(
            (r.display().as_str(), r.unit().as_str(), r.value_si_text().as_deref()),
            ("1.2", "MΩ", Some("1200000"))
        );

        // Small values keep their leading zero; 5-digit B41T+ values work.
        let r = decode_binary(&hex("23 F0 00 00 05 00").try_into().unwrap(), B41);
        assert_eq!((r.display().as_str(), r.unit().as_str()), ("0.005", "V"));
        let r = decode_binary(&hex("22 F0 00 00 D0 55").try_into().unwrap(), B41);
        assert_eq!(r.display(), "219.68");

        // Duty cycle ignores the prefix; °C has no prefix.
        let r = decode_binary(&hex("E1 F1 00 00 7B 01").try_into().unwrap(), B41);
        assert_eq!((r.function, r.display().as_str(), r.unit().as_str()), (Function::Duty, "37.9", "%"));
        let r = decode_binary(&hex("21 F2 00 00 F1 00").try_into().unwrap(), B41);
        assert_eq!((r.function, r.display().as_str(), r.unit().as_str()), (Function::Celsius, "24.1", "°C"));

        // Function 13 depends on the series.
        let f = hex("60 F3 00 00 02 00");
        assert_eq!(decode_binary(&f.clone().try_into().unwrap(), B41).function, Function::Adp);
        let r = decode_binary(&f.try_into().unwrap(), ow55);
        assert_eq!((r.function, r.display().as_str()), (Function::Ncv, "--"));
    }

    #[test]
    fn reassembles_binary_frames() {
        let mut d = FrameDecoder::new(B41);
        let frames = hex("1B F0 04 00 D2 84 23 F0 00 00 05 00");
        assert!(d.push(&frames[..4]).is_empty());
        let out = d.push(&frames[4..]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].display(), "0.005");

        // A fragment followed by whole frames is discarded.
        assert!(d.push(&frames[..3]).is_empty());
        let out = d.push(&frames[..6]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].display(), "-1.234");
    }

    #[test]
    fn decodes_legacy_frames() {
        let frame = hex("2D 31 32 33 34 20 31 30 00 40 80 00 0D 0A");
        let r = decode_legacy(&frame).unwrap();
        assert_eq!((r.function, r.prefix, r.point), (Function::DcVoltage, Prefix::Milli, Point::Decimals(3)));
        assert_eq!(r.display(), "-1.234");
        assert!(r.flags.has(Flags::AUTO));

        // Overload shows as non-digit glyphs.
        let mut ol = frame.clone();
        ol[1..5].copy_from_slice(b"?0:?");
        ol[9] = 0x20;
        ol[10] = 0x20;
        let r = decode_legacy(&ol).unwrap();
        assert_eq!((r.function, r.point, r.unit().as_str()), (Function::Resistance, Point::Overload, "kΩ"));

        // Junk in front and a split notification.
        let mut d = FrameDecoder::new(Dialect { legacy: true, ..Dialect::default() });
        let mut stream = hex("00 34 0A");
        stream.extend_from_slice(&frame);
        stream.extend_from_slice(&frame);
        assert_eq!(d.push(&stream[..10]).len(), 0);
        let out = d.push(&stream[10..]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].display(), "-1.234");
    }

    #[test]
    fn encodes_commands() {
        assert_eq!(cmd_log_length().to_vec(), hex("2a 52 45 41 44 6c 65 6e 3f 00 00 00 00 00 00 00"));
        assert_eq!(&cmd_read_log(true)[..8], b"*READ1?\0");
        assert_eq!(&cmd_read_log(false)[..7], b"*READ?\0");
        assert_eq!(&cmd_stop_recording()[..6], b"*STOP\0");
        assert_eq!(cmd_record(1, 10).to_vec(), hex("2a 52 45 43 4f 72 64 2c 01 00 00 00 0a 00 00 00"));
        let t = NaiveDate::from_ymd_opt(2026, 11, 11).unwrap().and_hms_opt(14, 5, 9).unwrap();
        assert_eq!(cmd_set_date(t).to_vec(), hex("2a 44 41 54 65 14 1a 0b 0b 0e 05 09 00 00 00 00"));
        let midnight = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap().and_hms_opt(0, 0, 0).unwrap();
        assert_eq!(cmd_set_date(midnight)[9], 24);
        assert_eq!(button_frame(Button::Hold, false), [3, 1]);
        assert_eq!(button_frame(Button::Range, true), [2, 0]);
    }

    #[test]
    fn validates_names() {
        assert_eq!(cmd_rename("BDM_2").unwrap().to_vec(), hex("40 42 44 4d 5f 32 00 00 00 00 00 00 00 00 00 00"));
        assert!(cmd_rename("ABCDEFGHIJKLMNO").is_ok());
        assert_eq!(cmd_rename("ABCDEFGHIJKLMNOP"), Err(NameError::TooLong));
        assert_eq!(cmd_rename("my meter"), Err(NameError::InvalidCharacter(' ')));
        assert_eq!(cmd_rename(""), Err(NameError::Empty));
    }

    fn log_stream(header: &str) -> Vec<u8> {
        let marks = "FF FF ".repeat(10);
        hex(&format!("{marks} {header} 23 F0 D2 04 F4 81 27 F1 00 00 {marks}"))
    }

    #[test]
    fn decodes_dated_log() {
        let stream = log_stream("14 18 0B 0B 0A 1E 00 00 05 00 00 00 0A 00 00 00");
        let mut d = LogDecoder::new(true, B41);
        // Split at odd offsets, with junk before the transfer.
        assert!(!d.push(&hex("12 34")));
        assert!(!d.push(&stream[..7]));
        assert!(!d.push(&stream[7..33]));
        assert!(d.push(&stream[33..]));
        assert_eq!(d.received(), 26);
        let log = d.finish().unwrap();
        let start = NaiveDate::from_ymd_opt(2024, 11, 11).unwrap().and_hms_opt(10, 30, 0).unwrap();
        assert_eq!(log.start, Some(start));
        assert_eq!(log.interval_s, Some(5));
        let texts: Vec<String> =
            log.records.iter().map(|r| format!("{} {}", r.reading.display(), r.reading.unit())).collect();
        assert_eq!(texts, ["1.234 V", "-0.500 V", "OL Ω"]);
        assert_eq!(log.records[0].time, Some(start + chrono::Duration::seconds(5)));
        assert_eq!(log.records[2].time, Some(start + chrono::Duration::seconds(15)));
        assert_eq!(log.records[2].reading.function, Function::Resistance);

        let mut csv = Vec::new();
        log.write_csv(&mut csv).unwrap();
        let csv = String::from_utf8(csv).unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], Log::CSV_HEADER);
        assert_eq!(lines[1], "0,2024-11-11 10:30:05,DCV,1.234,V,1.234,V,");
        assert_eq!(lines[3], "2,2024-11-11 10:30:15,RES,OL,Ω,,Ω,");
    }

    #[test]
    fn decodes_undated_log() {
        let mut d = LogDecoder::new(false, B41);
        assert!(d.push(&log_stream("0A 00 00 00")));
        let log = d.finish().unwrap();
        assert_eq!(log.start, None);
        assert_eq!(log.records.len(), 3);
        assert!(log.records.iter().all(|r| r.time.is_none()));

        // An empty log is fine, a truncated header is not.
        let mut d = LogDecoder::new(true, B41);
        assert!(d.push(&hex(&"FF FF ".repeat(20))));
        assert_eq!(d.finish().unwrap().records.len(), 0);
        let mut d = LogDecoder::new(true, B41);
        assert!(d.push(&hex(&format!("{} 01 02 {}", "FF FF ".repeat(10), "FF FF ".repeat(10)))));
        assert!(d.finish().is_err());
    }
}
