//! USB testers: discovery (including hot-plugging) and connections.
//!
//! USB devices appear in the same device list as Bluetooth ones and use the
//! same connection events.  Each supported model is listed in [`MODELS`]
//! with the protocol it speaks.

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use nusb::descriptors::TransferType;
use nusb::transfer::{Buffer, Bulk, ControlOut, ControlType, Direction, In, Interrupt, Out, Recipient, TransferError};
use tokio::sync::mpsc;
use tokio::time::{timeout, Instant};

use crate::atkc;
use crate::ble::{
    emit, ConnEvent, ConnTx, Ctl, DeviceId, End, Event, Events, Failure, Features, Guess, Kind, SeenDevice,
    TesterDetails, TesterEvent, UsbPort,
};
use crate::fnirsi;
use crate::protocol::{ProtocolInfo, RawSample, RealtimeBlock};
use crate::ut70;

/// The protocols spoken by supported USB devices.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// ALIENTEK testers (C0, C2) with the ATK-C desktop software.
    AtkC,
    /// FNIRSI testers over HID.
    Fnirsi(fnirsi::Generation),
    /// ALIENTEK UT70 over HID.
    Ut70,
}

/// A supported USB device.
#[derive(Debug)]
pub struct Model {
    pub vid: u16,
    pub pid: u16,
    pub name: &'static str,
    pub kind: Kind,
    pub protocol: Protocol,
}

pub const MODELS: &[Model] = &[
    // The C0 and the C2 enumerate alike; their version reply tells them apart.
    Model { vid: atkc::VID, pid: atkc::PID, name: "ALIENTEK C0 / C2", kind: Kind::Tester, protocol: Protocol::AtkC },
    Model { vid: 0x19F5, pid: 0x6666, name: "ALIENTEK UT70", kind: Kind::Tester, protocol: Protocol::Ut70 },
    // The FNB-C2 identifies itself as an FNB58.
    Model {
        vid: 0x2E3C,
        pid: 0x5558,
        name: "FNIRSI FNB58",
        kind: Kind::Tester,
        protocol: Protocol::Fnirsi(fnirsi::Generation::Fnb58),
    },
    Model {
        vid: 0x2E3C,
        pid: 0x0049,
        name: "FNIRSI FNB48S",
        kind: Kind::Tester,
        protocol: Protocol::Fnirsi(fnirsi::Generation::Fnb58),
    },
    Model {
        vid: 0x0483,
        pid: 0x003A,
        name: "FNIRSI FNB48",
        kind: Kind::Tester,
        protocol: Protocol::Fnirsi(fnirsi::Generation::Fnb48),
    },
    Model {
        vid: 0x0483,
        pid: 0x003B,
        name: "FNIRSI C1",
        kind: Kind::Tester,
        protocol: Protocol::Fnirsi(fnirsi::Generation::Fnb48),
    },
];

pub fn model(vid: u16, pid: u16) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.vid == vid && m.pid == pid)
}

fn port_of(info: &nusb::DeviceInfo) -> UsbPort {
    UsbPort::new(info.vendor_id(), info.product_id(), info.busnum(), info.port_chain())
}

fn seen(info: &nusb::DeviceInfo) -> Option<SeenDevice> {
    let m = model(info.vendor_id(), info.product_id())?;
    let guess = match m.kind {
        Kind::Tester => Guess::Tester,
        Kind::Meter => Guess::Meter,
    };
    Some(SeenDevice { address: DeviceId::Usb(port_of(info)), name: Some(m.name.into()), rssi: None, guess })
}

/// Reports the supported devices that are plugged in.
pub async fn report_present(evt: &Events) {
    match nusb::list_devices().await {
        Ok(list) => {
            for d in list.filter_map(|i| seen(&i)) {
                emit(evt, Event::DeviceSeen(d));
            }
        }
        Err(e) => log::debug!("Listing USB devices failed: {e}"),
    }
}

/// Reports supported devices as they are plugged in and out.
pub async fn watch(evt: Events) {
    let mut watch = match nusb::watch_devices() {
        Ok(w) => w,
        Err(e) => {
            log::info!("USB hot-plug is not available: {e}");
            return;
        }
    };
    let mut known: HashMap<nusb::DeviceId, UsbPort> = HashMap::new();
    if let Ok(list) = nusb::list_devices().await {
        for info in list {
            if let Some(d) = seen(&info) {
                known.insert(info.id(), port_of(&info));
                emit(&evt, Event::DeviceSeen(d));
            }
        }
    }
    while let Some(ev) = watch.next().await {
        match ev {
            nusb::hotplug::HotplugEvent::Connected(info) => {
                if let Some(d) = seen(&info) {
                    log::info!("USB device plugged in: {} ({})", info.product_string().unwrap_or("?"), port_of(&info));
                    known.insert(info.id(), port_of(&info));
                    emit(&evt, Event::DeviceSeen(d));
                }
            }
            nusb::hotplug::HotplugEvent::Disconnected(id) => {
                if let Some(port) = known.remove(&id) {
                    emit(&evt, Event::DeviceGone(DeviceId::Usb(port)));
                }
            }
        }
    }
}

/// The device in `port`, or else the only device of the same model (it may
/// have been plugged into another port).
async fn find(port: UsbPort) -> Option<nusb::DeviceInfo> {
    let same_model: Vec<nusb::DeviceInfo> =
        nusb::list_devices().await.ok()?.filter(|i| i.vendor_id() == port.vid && i.product_id() == port.pid).collect();
    match same_model.iter().position(|i| port_of(i) == port) {
        Some(k) => same_model.into_iter().nth(k),
        None if same_model.len() == 1 => same_model.into_iter().next(),
        None => None,
    }
}

pub(crate) fn failure(e: nusb::Error) -> Failure {
    match e.kind() {
        nusb::ErrorKind::PermissionDenied => Failure::Fatal(
            "No permission to use the USB device. Install Křižík's udev rule (see the README) and plug it in again."
                .into(),
        ),
        nusb::ErrorKind::Busy => Failure::Retry("The USB device is in use by another program".into()),
        nusb::ErrorKind::NotFound | nusb::ErrorKind::Disconnected => {
            Failure::Unplugged("The USB device was unplugged".into())
        }
        _ => Failure::Retry(format!("USB error: {e}")),
    }
}

pub(crate) async fn run_session(
    port: UsbPort,
    ctl: &mut mpsc::UnboundedReceiver<Ctl>,
    tx: &ConnTx,
) -> Result<End, Failure> {
    let Some(m) = model(port.vid, port.pid) else {
        return Err(Failure::Fatal("This USB device is not supported".into()));
    };
    let Some(info) = find(port).await else {
        return Err(Failure::Unplugged(format!("The {} is not plugged in", m.name)));
    };
    let device = info.open().await.map_err(failure)?;
    match m.protocol {
        Protocol::AtkC => run_atkc(&device, m, ctl, tx).await,
        Protocol::Fnirsi(generation) => run_fnirsi(&device, &info, m, generation, ctl, tx).await,
        Protocol::Ut70 => run_ut70(&device, &info, m, ctl, tx).await,
    }
}

/// Live samples as the tester pages expect them.
fn block(samples: Vec<RawSample>, interval_ms: u16, temperature_c: Option<f32>) -> RealtimeBlock {
    RealtimeBlock {
        protocol: ProtocolInfo::default(),
        power_bank_mode: false,
        interval_ms,
        samples,
        status: 0,
        temperature_c: temperature_c.map_or(0, |t| t.round().clamp(0.0, 255.0) as u8),
    }
}

/// What every USB session does with commands from the UI.  Returns `Some`
/// when the session should end.
fn handle_ctl(c: Option<Ctl>, tx: &ConnTx) -> Option<End> {
    match c {
        Some(Ctl::Stop) | None => Some(End::User),
        Some(Ctl::Rename(_)) => {
            tx.send(ConnEvent::Error("This device can't be renamed".into()));
            None
        }
        Some(Ctl::Meter(_)) => {
            tx.send(ConnEvent::Error("This is a tester, not a multimeter".into()));
            None
        }
    }
}

fn details(info: &nusb::DeviceInfo, m: &Model) -> TesterDetails {
    let model = match (m.vid, m.pid) {
        (0x2E3C, 0x5558) => "FNIRSI FNB58 or FNB-C2".to_string(),
        _ => m.name.to_string(),
    };
    TesterDetails {
        model: Some(model),
        serial: info.serial_number().filter(|s| !s.trim().is_empty()).map(str::to_string),
        firmware: None,
        hardware: Some(format!("{:x}.{:02x}", info.device_version() >> 8, info.device_version() & 0xFF)),
    }
}

// ---------------------------------------------------------------- HID

/// A claimed HID interface with 64-byte style interrupt reports.
pub(crate) struct HidLink {
    interface: nusb::Interface,
    number: u8,
    inp: nusb::Endpoint<Interrupt, In>,
    out: Option<nusb::Endpoint<Interrupt, Out>>,
    in_len: usize,
}

impl HidLink {
    /// Claims the first HID interface, or interface `number`.
    pub async fn open(device: &nusb::Device, number: Option<u8>) -> Result<HidLink, Failure> {
        let (n, in_ep, out_ep) = {
            let cfg = device
                .active_configuration()
                .map_err(|e| Failure::Retry(format!("Reading the USB configuration failed: {e}")))?;
            let alt = cfg
                .interface_alt_settings()
                .find(|i| i.alternate_setting() == 0 && number.map_or(i.class() == 3, |n| i.interface_number() == n))
                .ok_or_else(|| Failure::Fatal("The device has no HID interface".into()))?;
            let find = |dir: Direction| {
                alt.endpoints()
                    .find(|e| e.transfer_type() == TransferType::Interrupt && e.direction() == dir)
                    .map(|e| (e.address(), e.max_packet_size()))
            };
            let in_ep = find(Direction::In).ok_or_else(|| Failure::Fatal("The HID interface has no input".into()))?;
            (alt.interface_number(), in_ep, find(Direction::Out))
        };
        let interface = device.detach_and_claim_interface(n).await.map_err(failure)?;
        let ep_err = |e: nusb::Error| Failure::Retry(format!("USB endpoint: {e}"));
        let mut inp = interface.endpoint::<Interrupt, In>(in_ep.0).map_err(ep_err)?;
        let in_len = in_ep.1.max(1);
        for _ in 0..4 {
            inp.submit(Buffer::new(in_len));
        }
        let out = match out_ep {
            Some((addr, _)) => Some(interface.endpoint::<Interrupt, Out>(addr).map_err(ep_err)?),
            None => None,
        };
        Ok(HidLink { interface, number: n, inp, out, in_len })
    }

    /// Sends an output report (without report ID).
    pub async fn write(&mut self, report: &[u8]) -> Result<(), String> {
        match &mut self.out {
            Some(ep) => {
                let mut buf = Buffer::new(report.len());
                buf.extend_from_slice(report);
                ep.submit(buf);
                match timeout(Duration::from_secs(1), ep.next_complete()).await {
                    Ok(c) => c.status.map_err(|e| transfer_error("write", e)),
                    Err(_) => {
                        ep.cancel_all();
                        Err("Writing to the USB device timed out".into())
                    }
                }
            }
            None => {
                // HID SET_REPORT, output report 0.
                let req = ControlOut {
                    control_type: ControlType::Class,
                    recipient: Recipient::Interface,
                    request: 0x09,
                    value: 0x0200,
                    index: self.number as u16,
                    data: report,
                };
                self.interface.control_out(req, Duration::from_secs(1)).await.map_err(|e| transfer_error("write", e))
            }
        }
    }

    /// The next input report.  Cancel-safe.
    pub async fn read(&mut self) -> Result<Vec<u8>, String> {
        let c = self.inp.next_complete().await;
        let data = c.buffer[..c.actual_len.min(c.buffer.len())].to_vec();
        c.status.map_err(|e| transfer_error("read", e))?;
        let mut buf = c.buffer;
        buf.clear();
        buf.set_requested_len(self.in_len);
        self.inp.submit(buf);
        Ok(data)
    }
}

fn transfer_error(what: &str, e: TransferError) -> String {
    match e {
        TransferError::Disconnected => "The USB device was unplugged".into(),
        e => format!("USB {what} failed: {e}"),
    }
}

// ------------------------------------------------------------- FNIRSI

async fn run_fnirsi(
    device: &nusb::Device,
    info: &nusb::DeviceInfo,
    m: &Model,
    generation: fnirsi::Generation,
    ctl: &mut mpsc::UnboundedReceiver<Ctl>,
    tx: &ConnTx,
) -> Result<End, Failure> {
    let mut hid = HidLink::open(device, None).await?;
    tx.send(ConnEvent::Connected { name: Some(m.name.to_string()), kind: Kind::Tester });
    tx.tester(TesterEvent::Features(Features {
        data_lines: true,
        cc_lines: false,
        protocol: false,
        temperature: true,
        rename: false,
    }));
    tx.tester(TesterEvent::Details(details(info, m)));
    for cmd in generation.init() {
        hid.write(&fnirsi::command(cmd)).await?;
    }
    let mut poll = tokio::time::interval_at(Instant::now() + generation.poll_interval(), generation.poll_interval());
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_data = Instant::now();
    let mut bad_crc = 0u32;
    loop {
        tokio::select! {
            r = hid.read() => {
                let report = match r {
                    Ok(r) => r,
                    Err(e) => return Ok(End::Lost(e)),
                };
                let Some(pkt) = fnirsi::decode(&report) else { continue };
                if !pkt.crc_ok {
                    bad_crc += 1;
                    if bad_crc.is_power_of_two() {
                        log::debug!("FNIRSI report with a wrong CRC ({bad_crc} so far): {report:02X?}");
                    }
                }
                last_data = Instant::now();
                tx.tester(TesterEvent::Data(block(pkt.samples, fnirsi::SAMPLE_INTERVAL_MS, Some(pkt.temperature_c))));
            }
            _ = poll.tick() => {
                if last_data.elapsed() > Duration::from_secs(5) {
                    return Err(Failure::Retry("The tester stopped sending data".into()));
                }
                hid.write(&fnirsi::command(fnirsi::CMD_POLL)).await?;
            }
            c = ctl.recv() => {
                if let Some(end) = handle_ctl(c, tx) {
                    return Ok(end);
                }
            }
        }
    }
}

// --------------------------------------------------------------- UT70

async fn run_ut70(
    device: &nusb::Device,
    info: &nusb::DeviceInfo,
    m: &Model,
    ctl: &mut mpsc::UnboundedReceiver<Ctl>,
    tx: &ConnTx,
) -> Result<End, Failure> {
    let mut hid = HidLink::open(device, None).await?;
    tx.send(ConnEvent::Connected { name: Some(m.name.to_string()), kind: Kind::Tester });
    tx.tester(TesterEvent::Features(Features {
        data_lines: true,
        cc_lines: false,
        protocol: false,
        temperature: false,
        rename: false,
    }));
    let mut details = details(info, m);
    tx.tester(TesterEvent::Details(details.clone()));
    // The tester streams by itself; these only fill in details.
    hid.write(&ut70::get_version()).await?;
    hid.write(&ut70::get_rate()).await?;
    // Until the tester reports its rate, assume the vendor default of 4/s.
    let mut rate: Option<u8> = None;
    let mut check = tokio::time::interval_at(Instant::now() + Duration::from_secs(5), Duration::from_secs(5));
    let mut last_data = Instant::now();
    loop {
        tokio::select! {
            r = hid.read() => {
                let report = match r {
                    Ok(r) => r,
                    Err(e) => return Ok(End::Lost(e)),
                };
                let Some(frame) = ut70::parse_report(&report) else {
                    log::debug!("Unexpected UT70 report {report:02X?}");
                    continue;
                };
                if !frame.sum_ok {
                    log::debug!("UT70 frame with a wrong checksum: {report:02X?}");
                }
                match frame.fid {
                    ut70::FID_DATA => {
                        last_data = Instant::now();
                        let index = rate.unwrap_or(0);
                        let samples = ut70::decode_data(&frame.data, index);
                        tx.tester(TesterEvent::Data(block(samples, ut70::period_ms(index), None)));
                    }
                    ut70::FID_VERSION => {
                        if let Some((hw, serial)) = ut70::parse_version(&frame.data) {
                            details.hardware = Some(format!("{}.{}", hw / 10, hw % 10));
                            if !serial.is_empty() {
                                details.serial = Some(serial);
                            }
                            tx.tester(TesterEvent::Details(details.clone()));
                        }
                    }
                    ut70::FID_FREQUENCY => {
                        if let Some(i) = ut70::parse_rate(&frame.data) {
                            log::debug!("UT70 sample rate: {}/s", ut70::RATES[i as usize]);
                            rate = Some(i);
                        }
                    }
                    other => log::trace!("UT70 frame 0x{other:02X}: {:02X?}", frame.data),
                }
            }
            _ = check.tick() => {
                if last_data.elapsed() > Duration::from_secs(15) {
                    return Err(Failure::Retry("The tester stopped sending data".into()));
                }
                if rate.is_none() {
                    hid.write(&ut70::get_rate()).await?;
                }
            }
            c = ctl.recv() => {
                if let Some(end) = handle_ctl(c, tx) {
                    return Ok(end);
                }
            }
        }
    }
}

// ------------------------------------------------------- ALIENTEK C0/C2

/// Bulk endpoints of the ATK-C protocol.
struct BulkLink {
    _interface: nusb::Interface,
    out: nusb::Endpoint<Bulk, Out>,
    inp: nusb::Endpoint<Bulk, In>,
}

const BULK_READ_LEN: usize = 4096;

impl BulkLink {
    async fn open(device: &nusb::Device) -> Result<BulkLink, Failure> {
        let interface = device.detach_and_claim_interface(0).await.map_err(failure)?;
        let ep_err = |e: nusb::Error| Failure::Retry(format!("USB endpoint: {e}"));
        let out = interface.endpoint::<Bulk, Out>(atkc::EP_OUT).map_err(ep_err)?;
        let mut inp = interface.endpoint::<Bulk, In>(atkc::EP_IN).map_err(ep_err)?;
        for _ in 0..4 {
            inp.submit(Buffer::new(BULK_READ_LEN));
        }
        Ok(BulkLink { _interface: interface, out, inp })
    }

    async fn write(&mut self, frame: &[u8]) -> Result<(), String> {
        let mut buf = Buffer::new(frame.len());
        buf.extend_from_slice(frame);
        self.out.submit(buf);
        let result = match timeout(Duration::from_millis(500), self.out.next_complete()).await {
            Ok(c) => c.status.map_err(|e| transfer_error("write", e)),
            Err(_) => {
                self.out.cancel_all();
                Err("Writing to the USB device timed out".into())
            }
        };
        // The vendor software leaves at least 10 ms between requests.
        tokio::time::sleep(Duration::from_millis(10)).await;
        result
    }

    /// The next chunk of data.  Cancel-safe.
    async fn read(&mut self) -> Result<Vec<u8>, String> {
        let c = self.inp.next_complete().await;
        let data = c.buffer[..c.actual_len.min(c.buffer.len())].to_vec();
        c.status.map_err(|e| transfer_error("read", e))?;
        let mut buf = c.buffer;
        buf.clear();
        buf.set_requested_len(BULK_READ_LEN);
        self.inp.submit(buf);
        Ok(data)
    }

    /// Reads frames until `accept` returns a value or `wait` has passed.
    async fn wait_for<T>(
        &mut self,
        parser: &mut atkc::FrameParser,
        wait: Duration,
        mut accept: impl FnMut(u8, &[u8]) -> Option<T>,
    ) -> Result<Option<T>, String> {
        let end = Instant::now() + wait;
        loop {
            let Ok(chunk) = tokio::time::timeout_at(end, self.read()).await else { return Ok(None) };
            for (cmd, data) in parser.push(&chunk?) {
                if let Some(v) = accept(cmd, &data) {
                    return Ok(Some(v));
                }
            }
        }
    }
}

async fn run_atkc(
    device: &nusb::Device,
    m: &Model,
    ctl: &mut mpsc::UnboundedReceiver<Ctl>,
    tx: &ConnTx,
) -> Result<End, Failure> {
    let mut usb = BulkLink::open(device).await?;
    let mut parser = atkc::FrameParser::default();
    // Like the vendor software: give the tester a moment, then ask for its
    // version up to six times.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut version = None;
    for _ in 0..6 {
        usb.write(&atkc::version_request()).await?;
        version = usb
            .wait_for(&mut parser, Duration::from_millis(300), |cmd, d| {
                (cmd == atkc::CMD_VERSION).then(|| atkc::parse_version(d)).flatten()
            })
            .await?;
        if version.is_some() {
            break;
        }
    }
    let Some(version) = version else {
        return Err(Failure::Retry("The tester does not answer over USB".into()));
    };
    if version.boot {
        return Err(Failure::Fatal(
            "The tester is in firmware update mode. Finish the update or restart the tester.".into(),
        ));
    }
    log::info!("USB tester: {version:?}");
    let name = if version.model_name.is_empty() { m.name.to_string() } else { version.model_name.clone() };
    tx.send(ConnEvent::Connected { name: Some(name.clone()), kind: Kind::Tester });
    tx.tester(TesterEvent::Features(Features {
        data_lines: true,
        cc_lines: true,
        protocol: true,
        temperature: false,
        rename: false,
    }));
    tx.tester(TesterEvent::Details(TesterDetails {
        model: Some(name),
        serial: Some(version.serial.clone()).filter(|s| !s.is_empty()),
        firmware: Some(version.firmware.clone()),
        hardware: Some(version.hardware.clone()),
    }));

    // Streaming needs the sample rate, which is chosen on the tester.
    usb.write(&atkc::rate_request()).await?;
    let mut sps = usb
        .wait_for(&mut parser, Duration::from_secs(1), |cmd, d| (cmd == atkc::CMD_RATE).then(|| atkc::parse_rate(d)))
        .await?
        .unwrap_or(atkc::RATES[0]);
    log::debug!("USB tester sample rate: {sps}/s");
    usb.write(&atkc::stream_request(true)).await?;

    let mut pd = ProtocolInfo::default();
    let mut check = tokio::time::interval_at(Instant::now() + Duration::from_secs(3), Duration::from_secs(3));
    let mut last_data = Instant::now();
    loop {
        tokio::select! {
            r = usb.read() => {
                let chunk = match r {
                    Ok(c) => c,
                    Err(e) => return Ok(End::Lost(e)),
                };
                for (cmd, data) in parser.push(&chunk) {
                    match cmd {
                        atkc::CMD_STREAM => {
                            if let Some(samples) = atkc::decode_live(&data, sps) {
                                last_data = Instant::now();
                                let mut b = block(samples, (1000 / sps.max(1)).max(1), None);
                                b.protocol = pd.clone();
                                tx.tester(TesterEvent::Data(b));
                            }
                        }
                        atkc::CMD_RATE => sps = atkc::parse_rate(&data),
                        atkc::CMD_PD_MONITOR => {
                            if let Some(p) = atkc::parse_pd_monitor(&data) {
                                pd = p;
                            }
                        }
                        other => log::trace!("USB tester frame 0x{other:02X}: {data:02X?}"),
                    }
                }
            }
            _ = check.tick() => {
                let silent = last_data.elapsed();
                if silent > Duration::from_secs(12) {
                    return Err(Failure::Retry("The tester stopped sending data".into()));
                }
                // Slow rates send a frame every half second; anything longer
                // means streaming stopped (e.g. the rate was changed on the tester).
                if silent > Duration::from_secs(3) {
                    usb.write(&atkc::rate_request()).await?;
                    usb.write(&atkc::stream_request(true)).await?;
                }
            }
            c = ctl.recv() => {
                if let Some(end) = handle_ctl(c, tx) {
                    let _ = usb.write(&atkc::stream_request(false)).await;
                    let _ = usb.wait_for(&mut parser, Duration::from_millis(100), |_, _| None::<()>).await;
                    return Ok(end);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_round_trip() {
        let p = UsbPort::new(0x2E88, 0xC2C2, 1, &[3, 2]);
        assert_eq!(p.to_string(), "usb:2e88:c2c2@1-3.2");
        assert_eq!("usb:2e88:c2c2@1-3.2".parse::<UsbPort>(), Ok(p));
        let id: DeviceId = "usb:2e88:c2c2@1-3.2".parse().unwrap();
        assert_eq!(id, DeviceId::Usb(p));
        let ble: DeviceId = "12:34:56:78:9A:BC".parse().unwrap();
        assert_eq!(ble.to_string(), "12:34:56:78:9A:BC");
        assert!("usb:zz".parse::<DeviceId>().is_err());
    }
}
