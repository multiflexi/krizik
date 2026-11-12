//! Firmware update over USB.
//!
//! The tester's PC port (USB-C opposite the buttons) talks to the PC software
//! through bulk endpoints 0x01 (OUT) and 0x82 (IN) on interface 0. Bluetooth
//! cannot update firmware.
//!
//! * Application mode `2E88:C2C2`: 63-byte frames `EE cmd len data… chk FF`.
//!   Command 0x01 returns version information, 0x0F restarts into the
//!   bootloader.
//! * Bootloader `2E3C:5780`: frames `AF cmd seq len data… chk`, answered by
//!   `DF cmd …` with a status byte at offset 4 (0 = OK).
//!
//! Update sequence (as done by the ALIENTEK PC software): 0x10 bootloader
//! info, then for every section of the `.atk` file 0x11 (13-byte section
//! header), 0x12 (start), 0x13 (data, `chunk size` bytes each), 0x14 (end),
//! and finally 0x15 `A0` to start the new firmware. See PROTOCOL.md.

use std::fmt;
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use nusb::transfer::{Buffer, Bulk, In, Out};
use nusb::{DeviceInfo, Endpoint, MaybeFuture};

use crate::protocol::{ascii_field, checksum};

pub const APP_ID: (u16, u16) = (0x2E88, 0xC2C2);
pub const BOOT_ID: (u16, u16) = (0x2E3C, 0x5780);
const EP_OUT: u8 = 0x01;
const EP_IN: u8 = 0x82;
const APP_FRAME_LEN: usize = 63;
const SECTION_HEADER_LEN: usize = 13;

const BOOT_INFO: u8 = 0x10;
const BOOT_SECTION_INFO: u8 = 0x11;
const BOOT_START: u8 = 0x12;
const BOOT_DATA: u8 = 0x13;
const BOOT_END: u8 = 0x14;
const BOOT_RUN_APP: u8 = 0x15;
const APP_VERSION: u8 = 0x01;
const APP_ENTER_BOOTLOADER: u8 = 0x0F;

// ------------------------------------------------------------------ files

/// One section of an `.atk` firmware file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    /// Sent to the bootloader unchanged.
    pub header: [u8; SECTION_HEADER_LEN],
    /// Obfuscated image data, decoded by the bootloader.
    pub data: Vec<u8>,
}

impl Section {
    pub fn device_type(&self) -> u16 {
        u16::from_le_bytes([self.header[0], self.header[1]])
    }

    /// Minimum hardware version, in tenths.
    pub fn min_hardware(&self) -> u8 {
        self.header[2]
    }

    /// Release series in tenths (25 = 2.5.x).
    pub fn series(&self) -> u8 {
        self.header[3]
    }

    pub fn chunk_size(&self) -> usize {
        self.header[5] as usize
    }

    /// Build date (year, month, day).
    pub fn date(&self) -> (u16, u8, u8) {
        (2000 + self.header[10] as u16, self.header[11], self.header[12])
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirmwareFile {
    pub sections: Vec<Section>,
}

impl FirmwareFile {
    /// Parses an `.atk` file: sections of a 13-byte header followed by the
    /// data, padded to a multiple of four bytes.
    pub fn parse(bytes: &[u8]) -> Result<FirmwareFile, String> {
        let mut sections = Vec::new();
        let mut off = 0usize;
        while bytes.len() - off > SECTION_HEADER_LEN - 1 {
            let header: [u8; SECTION_HEADER_LEN] = bytes[off..off + SECTION_HEADER_LEN].try_into().unwrap();
            let len = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;
            let padded = (len + 3) & !3;
            if header[5] == 0 || bytes.len() - off - SECTION_HEADER_LEN < padded {
                return Err("This is not a valid C2 firmware file".into());
            }
            let start = off + SECTION_HEADER_LEN;
            sections.push(Section { header, data: bytes[start..start + len].to_vec() });
            off = start + padded;
        }
        if sections.is_empty() {
            return Err("This is not a valid C2 firmware file".into());
        }
        Ok(FirmwareFile { sections })
    }

    pub fn load(path: &Path) -> Result<FirmwareFile, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("Could not read the file: {e}"))?;
        FirmwareFile::parse(&bytes)
    }

    pub fn first(&self) -> &Section {
        &self.sections[0]
    }

    pub fn size(&self) -> usize {
        self.sections.iter().map(|s| s.data.len()).sum()
    }

    pub fn describe(&self) -> String {
        let s = self.first();
        let (y, m, d) = s.date();
        format!(
            "Firmware {}.{}.x, built {y}-{m:02}-{d:02}, {} KB",
            s.series() / 10,
            s.series() % 10,
            self.size().div_ceil(1024)
        )
    }
}

// ---------------------------------------------------------------- framing

/// Application-mode request, zero padded to 63 bytes.
pub fn app_frame(cmd: u8, data: &[u8]) -> Vec<u8> {
    let mut f = vec![0u8; APP_FRAME_LEN];
    f[0] = 0xEE;
    f[1] = cmd;
    f[2] = data.len() as u8;
    f[3..3 + data.len()].copy_from_slice(data);
    f[3 + data.len()] = checksum(&f[..3 + data.len()]);
    f[4 + data.len()] = 0xFF;
    f
}

/// Finds a checksum-valid application-mode frame for `cmd` in a packet.
pub fn find_app_reply(packet: &[u8], cmd: u8) -> Option<Vec<u8>> {
    let mut i = 0;
    while i + 4 <= packet.len() {
        if packet[i] == 0xEE && packet[i + 1] == cmd {
            let len = packet[i + 2] as usize;
            let end = i + 3 + len;
            if end < packet.len() && checksum(&packet[i..end]) == packet[end] {
                return Some(packet[i + 3..end].to_vec());
            }
        }
        i += 1;
    }
    None
}

/// Bootloader request: `AF cmd seq len data… chk`.
pub fn boot_frame(cmd: u8, seq: u8, data: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(data.len() + 5);
    f.extend_from_slice(&[0xAF, cmd, seq, data.len() as u8]);
    f.extend_from_slice(data);
    f.push(checksum(&f));
    f
}

/// Version information from the application-mode reply to command 0x01.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppInfo {
    /// Exact firmware version, e.g. "2.4.4".
    pub firmware: String,
    pub hardware: u8,
    pub bootloader: u8,
    pub model_code: u16,
    pub serial: String,
    pub model_name: String,
}

pub fn parse_app_info(d: &[u8]) -> Option<AppInfo> {
    if d.len() < 27 {
        return None;
    }
    let v = d[1] as u32 * 100 + d[2] as u32;
    Some(AppInfo {
        firmware: format!("{}.{}.{}", v / 100, (v / 10) % 10, v % 10),
        bootloader: d[0],
        hardware: d[3],
        model_code: u16::from_le_bytes([d[5], d[6]]),
        serial: ascii_field(&d[7..15]),
        model_name: ascii_field(&d[15..27]),
    })
}

// -------------------------------------------------------------------- usb

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsbError {
    NotFound,
    PermissionDenied,
    Busy,
    Other(String),
}

impl fmt::Display for UsbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UsbError::NotFound => write!(f, "No tester is connected over USB"),
            UsbError::PermissionDenied => write!(
                f,
                "No permission to use the tester over USB. Install the udev rule from the Křižík README, then reconnect the tester."
            ),
            UsbError::Busy => write!(f, "The tester is in use by another program (for example the ATK-C PC software)"),
            UsbError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl From<nusb::Error> for UsbError {
    fn from(e: nusb::Error) -> Self {
        match e.kind() {
            nusb::ErrorKind::PermissionDenied => UsbError::PermissionDenied,
            nusb::ErrorKind::Busy => UsbError::Busy,
            nusb::ErrorKind::NotFound | nusb::ErrorKind::Disconnected => UsbError::NotFound,
            _ => UsbError::Other(format!("USB error: {e}")),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    App,
    Bootloader,
}

fn find(mode: Mode) -> Option<DeviceInfo> {
    let (vid, pid) = match mode {
        Mode::App => APP_ID,
        Mode::Bootloader => BOOT_ID,
    };
    nusb::list_devices().wait().ok()?.find(|d| d.vendor_id() == vid && d.product_id() == pid)
}

/// Which mode a USB-connected tester is in, if any.
pub fn detect() -> Option<Mode> {
    if find(Mode::Bootloader).is_some() {
        Some(Mode::Bootloader)
    } else if find(Mode::App).is_some() {
        Some(Mode::App)
    } else {
        None
    }
}

struct Link {
    out: Endpoint<Bulk, Out>,
    inp: Endpoint<Bulk, In>,
    _interface: nusb::Interface,
}

impl Link {
    fn open(info: &DeviceInfo) -> Result<Link, UsbError> {
        let device = info.open().wait()?;
        let interface = device.detach_and_claim_interface(0).wait()?;
        let out = interface.endpoint::<Bulk, Out>(EP_OUT).map_err(|e| UsbError::Other(e.to_string()))?;
        let inp = interface.endpoint::<Bulk, In>(EP_IN).map_err(|e| UsbError::Other(e.to_string()))?;
        Ok(Link { out, inp, _interface: interface })
    }

    /// Opens the first device in `mode`, waiting for it to appear (after a
    /// restart) and for udev to grant access.
    fn open_mode(mode: Mode, wait: Duration) -> Result<Link, UsbError> {
        let end = Instant::now() + wait;
        let mut last = UsbError::NotFound;
        loop {
            if let Some(info) = find(mode) {
                match Link::open(&info) {
                    Ok(l) => return Ok(l),
                    Err(e) => last = e,
                }
            }
            if Instant::now() >= end {
                return Err(last);
            }
            sleep(Duration::from_millis(250));
        }
    }

    fn send(&mut self, data: &[u8]) -> Result<(), UsbError> {
        let mut buf = Buffer::new(data.len());
        buf.extend_from_slice(data);
        self.out
            .transfer_blocking(buf, Duration::from_millis(1000))
            .status
            .map_err(|e| UsbError::Other(format!("USB write failed: {e}")))
    }

    /// Reads packets until `accept` returns a value or the timeout expires.
    fn receive<T>(&mut self, timeout: Duration, mut accept: impl FnMut(&[u8]) -> Option<T>) -> Option<T> {
        let end = Instant::now() + timeout;
        loop {
            if self.inp.pending() == 0 {
                let mps = self.inp.max_packet_size().max(64);
                self.inp.submit(Buffer::new(mps));
            }
            let left = end.saturating_duration_since(Instant::now());
            let c = self.inp.wait_next_complete(left)?;
            if c.status.is_err() {
                return None;
            }
            let packet = &c.buffer[..c.actual_len.min(c.buffer.len())];
            if let Some(v) = accept(packet) {
                return Some(v);
            }
            if Instant::now() >= end {
                return None;
            }
        }
    }

    fn app_request(&mut self, cmd: u8, data: &[u8], timeout: Duration) -> Result<Option<Vec<u8>>, UsbError> {
        self.send(&app_frame(cmd, data))?;
        Ok(self.receive(timeout, |p| find_app_reply(p, cmd)))
    }
}

/// Reads the exact firmware version from a tester in application mode.
pub fn read_app_info() -> Result<AppInfo, UsbError> {
    let info = find(Mode::App).ok_or(UsbError::NotFound)?;
    let mut link = Link::open(&info)?;
    for _ in 0..3 {
        if let Some(d) = link.app_request(APP_VERSION, &[0, 0], Duration::from_millis(800))? {
            if let Some(i) = parse_app_info(&d) {
                return Ok(i);
            }
        }
    }
    Err(UsbError::Other("The tester did not answer over USB".into()))
}

struct Boot {
    link: Link,
    seq: u8,
}

impl Boot {
    /// Returns the reply packet for `cmd`.
    fn request(&mut self, cmd: u8, data: &[u8], timeout: Duration) -> Result<Vec<u8>, String> {
        let frame = boot_frame(cmd, self.seq, data);
        self.seq = self.seq.wrapping_add(1);
        self.link.send(&frame).map_err(|e| e.to_string())?;
        self.link
            .receive(timeout, |p| (p.len() >= 5 && p[0] == 0xDF && p[1] == cmd).then(|| p.to_vec()))
            .ok_or_else(|| format!("The bootloader did not answer (command 0x{cmd:02X})"))
    }

    /// Sends a command whose reply carries a status byte (0 = OK).
    fn command(&mut self, cmd: u8, data: &[u8], timeout: Duration) -> Result<(), String> {
        let reply = self.request(cmd, data, timeout)?;
        match reply[4] {
            0 => Ok(()),
            s => Err(format!("The bootloader rejected command 0x{cmd:02X} (status {s})")),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Progress {
    Stage(String),
    /// Fraction of the firmware written, 0.0–1.0.
    Written(f64),
}

/// What [`update`] should do.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Everything except writing: enter the bootloader, check the device
    /// against the file and start the existing firmware again.
    Check,
    Install,
}

/// Puts the tester into its bootloader, checks compatibility and (for
/// [`Action::Install`]) writes the firmware. Returns the version reported by
/// the tester afterwards, if it could be read. Blocking; run it on a thread.
pub fn update(
    fw: &FirmwareFile,
    action: Action,
    mut progress: impl FnMut(Progress),
) -> Result<Option<AppInfo>, String> {
    macro_rules! stage {
        ($($t:tt)*) => {{
            let s = format!($($t)*);
            log::info!("{s}");
            progress(Progress::Stage(s));
        }};
    }

    match detect() {
        None => return Err(UsbError::NotFound.to_string()),
        Some(Mode::App) => {
            stage!("Restarting the tester into update mode…");
            let info = find(Mode::App).ok_or(UsbError::NotFound.to_string())?;
            let mut link = Link::open(&info).map_err(|e| e.to_string())?;
            link.send(&app_frame(APP_ENTER_BOOTLOADER, &[1, 0])).map_err(|e| e.to_string())?;
            drop(link);
        }
        Some(Mode::Bootloader) => stage!("The tester is already in update mode"),
    }

    stage!("Waiting for the update mode…");
    let link = Link::open_mode(Mode::Bootloader, Duration::from_secs(15)).map_err(|e| match e {
        UsbError::NotFound => "The tester did not switch to update mode".to_string(),
        e => e.to_string(),
    })?;
    let mut boot = Boot { link, seq: 0 };

    stage!("Checking the tester…");
    let reply = boot.request(BOOT_INFO, &[], Duration::from_secs(5))?;
    if reply.len() < 7 {
        return Err("Unexpected answer from the bootloader".into());
    }
    let device_type = u16::from_le_bytes([reply[4], reply[5]]);
    let hardware = reply[6];
    let first = fw.first();
    if device_type != first.device_type() {
        let _ = boot.command(BOOT_RUN_APP, &[0xA0], Duration::from_secs(5));
        return Err(format!(
            "This firmware is for a different device (file 0x{:04X}, tester 0x{device_type:04X})",
            first.device_type()
        ));
    }
    if hardware < first.min_hardware() {
        let _ = boot.command(BOOT_RUN_APP, &[0xA0], Duration::from_secs(5));
        return Err(format!(
            "This firmware needs hardware {}.{} or newer",
            first.min_hardware() / 10,
            first.min_hardware() % 10
        ));
    }

    if action == Action::Install {
        let total = fw.size().max(1) as f64;
        let mut written = 0usize;
        let mut last_reported = -1.0;
        for (n, section) in fw.sections.iter().enumerate() {
            stage!("Preparing section {} of {}…", n + 1, fw.sections.len());
            boot.command(BOOT_SECTION_INFO, &section.header, Duration::from_secs(10))?;
            boot.command(BOOT_START, &[], Duration::from_secs(20))?;
            stage!("Writing firmware…");
            let chunk = section.chunk_size();
            for piece in section.data.chunks(chunk) {
                // Like the PC software, always send full chunks.
                let mut data = piece.to_vec();
                data.resize(chunk, 0);
                boot.command(BOOT_DATA, &data, Duration::from_secs(5))?;
                written += piece.len();
                let f = written as f64 / total;
                if f - last_reported >= 0.005 || written as f64 >= total {
                    last_reported = f;
                    progress(Progress::Written(f));
                }
            }
            boot.command(BOOT_END, &[], Duration::from_secs(20))?;
        }
    }

    stage!("Starting the firmware…");
    if let Err(e) = boot.command(BOOT_RUN_APP, &[0xA0], Duration::from_secs(5)) {
        // The tester may restart before answering; check that it comes back.
        log::warn!("{e}");
    }
    drop(boot);

    stage!("Waiting for the tester to restart…");
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end {
        if find(Mode::App).is_some() {
            sleep(Duration::from_millis(500));
            return Ok(read_app_info().ok());
        }
        sleep(Duration::from_millis(250));
    }
    if find(Mode::Bootloader).is_some() {
        return Err("The tester stayed in update mode. Try the update again.".into());
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(len: usize, chunk: u8) -> Vec<u8> {
        let mut h = vec![0x0A, 0x03, 0x0A, 0x19, 0xA0, chunk];
        h.extend_from_slice(&(len as u32).to_le_bytes());
        h.extend_from_slice(&[0x1A, 0x08, 0x1C]);
        h.extend((0..len).map(|i| i as u8));
        h.resize(13 + ((len + 3) & !3), 0);
        h
    }

    #[test]
    fn parses_atk_sections() {
        let mut bytes = section(112, 0x38);
        bytes.extend(section(10, 0x20));
        let fw = FirmwareFile::parse(&bytes).unwrap();
        assert_eq!(fw.sections.len(), 2);
        assert_eq!(fw.first().device_type(), 0x030A);
        assert_eq!(fw.first().chunk_size(), 56);
        assert_eq!(fw.first().date(), (2026, 8, 28));
        assert_eq!(fw.sections[1].data.len(), 10);
        assert_eq!(fw.size(), 122);
        assert!(FirmwareFile::parse(&bytes[..50]).is_err());
        assert!(FirmwareFile::parse(&[1, 2, 3]).is_err());
    }

    #[test]
    fn frames() {
        // Same bytes as the PC software's EnterBootload().
        let f = app_frame(APP_ENTER_BOOTLOADER, &[1, 0]);
        assert_eq!(&f[..7], &[0xEE, 0x0F, 0x02, 0x01, 0x00, 0x00, 0xFF]);
        assert_eq!(f.len(), 63);
        let b = boot_frame(BOOT_RUN_APP, 7, &[0xA0]);
        assert_eq!(b, vec![0xAF, 0x15, 0x07, 0x01, 0xA0, checksum(&[0xAF, 0x15, 0x07, 0x01, 0xA0])]);
    }

    #[test]
    fn parses_app_version_reply() {
        // Reply layout from firmware 2.5.0: EE 01 1E <30 bytes> chk FF.
        let mut d = vec![0x0A, 2, 44, 0x0F, 0x19, 0x0A, 0x03];
        d.extend_from_slice(b"9308FF7F");
        d.extend_from_slice(b"ATK-C2 Pro\0\0");
        d.extend_from_slice(&[0, 0, 0]);
        let mut packet = vec![0xEE, 0x01, d.len() as u8];
        packet.extend_from_slice(&d);
        packet.push(checksum(&packet));
        packet.push(0xFF);
        packet.resize(63, 0);
        let reply = find_app_reply(&packet, 0x01).unwrap();
        let info = parse_app_info(&reply).unwrap();
        assert_eq!(info.firmware, "2.4.4");
        assert_eq!(info.hardware, 15);
        assert_eq!(info.serial, "9308FF7F");
        assert_eq!(info.model_name, "ATK-C2 Pro");
        assert_eq!(info.model_code, 0x030A);
    }
}
