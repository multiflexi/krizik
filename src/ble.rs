//! Bluetooth LE worker.
//!
//! Talks to BlueZ through `bluer` on a dedicated Tokio runtime thread and
//! exchanges [`Cmd`]s and [`Event`]s with the GTK main loop over channels.
//!
//! Several devices can be connected at once (the UI allows one tester and one
//! multimeter).  Each connection runs in its own task, identified by the
//! device address and a token chosen by the UI; every connection event
//! carries both, so the UI can ignore events from connections it has
//! already abandoned.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bluer::gatt::remote::{Characteristic, CharacteristicWriteRequest};
use bluer::gatt::{CharacteristicFlags, WriteOp};
use bluer::{
    Adapter, AdapterEvent, AdapterProperty, Address, Device, DeviceEvent, DeviceProperty, DiscoveryFilter,
    DiscoveryTransport, Session, Uuid,
};
use futures::{pin_mut, StreamExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{sleep, sleep_until, timeout, Instant};

use crate::owon::{self, Button};
use crate::protocol::{self, Command, DeviceInfo, FrameParser, Model, RealtimeBlock, Response};

/// Identifies a device, whether it is connected over Bluetooth or USB.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DeviceId {
    Ble(Address),
    Usb(UsbPort),
}

impl DeviceId {
    pub fn is_usb(self) -> bool {
        matches!(self, DeviceId::Usb(_))
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceId::Ble(a) => write!(f, "{a}"),
            DeviceId::Usb(p) => write!(f, "{p}"),
        }
    }
}

impl std::str::FromStr for DeviceId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.starts_with("usb:") {
            s.parse().map(DeviceId::Usb)
        } else {
            s.parse::<Address>().map(DeviceId::Ble).map_err(|e| e.to_string())
        }
    }
}

/// A USB device: its model and the port it is plugged into.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UsbPort {
    pub vid: u16,
    pub pid: u16,
    pub bus: u8,
    /// Port numbers from the root hub, `depth` of them used.
    pub ports: [u8; 7],
    pub depth: u8,
}

impl UsbPort {
    pub fn new(vid: u16, pid: u16, bus: u8, chain: &[u8]) -> Self {
        let mut ports = [0u8; 7];
        let depth = chain.len().min(7);
        ports[..depth].copy_from_slice(&chain[..depth]);
        UsbPort { vid, pid, bus, ports, depth: depth as u8 }
    }

    /// Where it is plugged in, like the kernel names it: "1-3.2".
    pub fn location(&self) -> String {
        let chain: Vec<String> = self.ports[..self.depth as usize].iter().map(u8::to_string).collect();
        format!("{}-{}", self.bus, chain.join("."))
    }
}

impl std::fmt::Display for UsbPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "usb:{:04x}:{:04x}@{}", self.vid, self.pid, self.location())
    }
}

impl std::str::FromStr for UsbPort {
    type Err = String;

    /// Parses "usb:2e88:c2c2@1-3.2".
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || format!("Invalid USB device “{s}”");
        let rest = s.strip_prefix("usb:").ok_or_else(bad)?;
        let (ids, loc) = rest.split_once('@').ok_or_else(bad)?;
        let (vid, pid) = ids.split_once(':').ok_or_else(bad)?;
        let (bus, chain) = loc.split_once('-').ok_or_else(bad)?;
        let hex = |x: &str| u16::from_str_radix(x, 16).map_err(|_| bad());
        let chain: Vec<u8> = chain.split('.').map(|p| p.parse().map_err(|_| bad())).collect::<Result<_, _>>()?;
        if chain.is_empty() || chain.len() > 7 {
            return Err(bad());
        }
        Ok(UsbPort::new(hex(vid)?, hex(pid)?, bus.parse().map_err(|_| bad())?, &chain))
    }
}

/// Address used by the simulated tester (`--demo`).
pub const DEMO_ADDRESS: DeviceId = DeviceId::Ble(Address([0x02, 0x00, 0x00, 0xC2, 0xDE, 0x30]));
/// Address used by the simulated multimeter (`--demo`).
pub const DEMO_METER_ADDRESS: DeviceId = DeviceId::Ble(Address([0x02, 0x00, 0x00, 0xB4, 0x1D, 0x30]));

/// The simulated USB tester (`--demo` with `KRIZIK_DEMO=usb`).
pub const DEMO_USB_ADDRESS: DeviceId =
    DeviceId::Usb(UsbPort { vid: 0x2E3C, pid: 0x5558, bus: 0, ports: [9, 0, 0, 0, 0, 0, 0], depth: 1 });

pub fn is_demo(address: DeviceId) -> bool {
    address == DEMO_ADDRESS || address == DEMO_METER_ADDRESS || address == DEMO_USB_ADDRESS
}

/// How long a scan runs before it stops by itself (saves battery on phones).
const SCAN_DURATION: Duration = Duration::from_secs(45);
/// Minimum time between two reports of the same device while scanning.
const REPORT_INTERVAL: Duration = Duration::from_millis(1500);
/// How often the multimeter's battery and log state are refreshed.
const METER_INFO_INTERVAL: Duration = Duration::from_secs(10);
/// A log download fails if the meter sends nothing for this long.
const LOG_IDLE_TIMEOUT: Duration = Duration::from_secs(8);

/// Chosen by the UI for each connection and echoed in its events.
pub type Token = u32;

/// What a connected device turned out to be.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Tester,
    Meter,
}

/// What a device seen while scanning probably is.  Testers and multimeters
/// both advertise service `0xFFF0`, so only the connection tells for sure.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Guess {
    Tester,
    Meter,
    /// Advertises the measurement service, could be either.
    Unknown,
    /// Something else.
    Other,
}

impl Guess {
    pub fn kind(self) -> Option<Kind> {
        match self {
            Guess::Tester => Some(Kind::Tester),
            Guess::Meter => Some(Kind::Meter),
            Guess::Unknown | Guess::Other => None,
        }
    }

    pub fn is_candidate(self) -> bool {
        self != Guess::Other
    }
}

#[derive(Debug)]
pub enum Cmd {
    StartScan,
    StopScan,
    Connect {
        address: DeviceId,
        token: Token,
    },
    Disconnect(DeviceId),
    DisconnectAll,
    /// Stores a new Bluetooth name on the device.
    Rename(DeviceId, String),
    Meter(DeviceId, MeterCmd),
    PowerOn,
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum MeterCmd {
    Press { button: Button, long: bool },
    StartRecording { interval_s: u32, count: u32 },
    StopRecording,
    DownloadLog,
}

#[derive(Clone, Debug)]
pub struct SeenDevice {
    pub address: DeviceId,
    pub name: Option<String>,
    pub rssi: Option<i16>,
    pub guess: Guess,
}

#[derive(Clone, Debug)]
pub enum Event {
    AdapterReady { name: String, powered: bool },
    AdapterUnavailable(String),
    AdapterPowered(bool),
    Scanning(bool),
    DeviceSeen(SeenDevice),
    DeviceGone(DeviceId),
    Conn { address: DeviceId, token: Token, ev: ConnEvent },
    Error(String),
}

#[derive(Clone, Debug)]
pub enum ConnEvent {
    Connecting {
        attempt: u32,
    },
    Connected {
        name: Option<String>,
        kind: Kind,
    },
    /// `retrying`: the worker keeps trying to reconnect.
    Disconnected {
        by_user: bool,
        error: Option<String>,
        retrying: bool,
    },
    Rssi(i16),
    Renamed(String),
    Error(String),
    Tester(TesterEvent),
    Meter(MeterEvent),
}

#[derive(Clone, Debug)]
pub enum TesterEvent {
    Identified(Model),
    Info(DeviceInfo),
    BleName(String),
    Data(RealtimeBlock),
    /// What the tester measures (sent by testers other than the C2 over Bluetooth).
    Features(Features),
    /// Model, serial number and versions of testers other than the C2 over Bluetooth.
    Details(TesterDetails),
}

/// What a tester can report, so that the pages hide what it can't.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Features {
    /// D+ and D− voltages.
    pub data_lines: bool,
    /// CC1 and CC2 voltages.
    pub cc_lines: bool,
    /// Fast-charging protocol detection.
    pub protocol: bool,
    /// The tester's own temperature.
    pub temperature: bool,
    /// The Bluetooth name can be changed.
    pub rename: bool,
}

impl Features {
    /// The C2 over Bluetooth.
    pub const ALL: Features =
        Features { data_lines: true, cc_lines: true, protocol: true, temperature: true, rename: true };
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TesterDetails {
    pub model: Option<String>,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub hardware: Option<String>,
}

#[derive(Clone, Debug)]
pub enum MeterEvent {
    Info(owon::Info),
    Reading { at: std::time::Instant, reading: owon::Reading },
    LogProgress { received: usize, total: Option<u32> },
    Log(Arc<owon::Log>),
    LogFailed(String),
}

/// Sends commands to the worker thread.
#[derive(Clone)]
pub struct Handle {
    tx: async_channel::Sender<Cmd>,
    done: Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
}

impl Handle {
    pub fn send(&self, cmd: Cmd) {
        if let Err(e) = self.tx.try_send(cmd) {
            log::warn!("Bluetooth worker is gone: {e}");
        }
    }

    /// Disconnects cleanly and waits (bounded) for the worker to finish.
    pub fn shutdown(&self, wait: Duration) {
        let _ = self.tx.try_send(Cmd::Shutdown);
        if let Some(rx) = self.done.lock().ok().and_then(|mut d| d.take()) {
            let _ = rx.recv_timeout(wait);
        }
    }
}

pub fn spawn() -> (Handle, async_channel::Receiver<Event>) {
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::unbounded();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("bluetooth".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("failed to start the Tokio runtime");
            rt.block_on(worker(cmd_rx, evt_tx));
            rt.shutdown_timeout(Duration::from_millis(500));
            let _ = done_tx.send(());
        })
        .expect("failed to spawn the Bluetooth thread");
    (Handle { tx: cmd_tx, done: Arc::new(Mutex::new(Some(done_rx))) }, evt_rx)
}

pub(crate) type Events = async_channel::Sender<Event>;

pub(crate) fn emit(evt: &Events, e: Event) {
    let _ = evt.try_send(e);
}

/// Sends events of one connection.
#[derive(Clone)]
pub(crate) struct ConnTx {
    pub evt: Events,
    pub address: DeviceId,
    token: Token,
}

impl ConnTx {
    pub fn send(&self, ev: ConnEvent) {
        emit(&self.evt, Event::Conn { address: self.address, token: self.token, ev });
    }

    pub fn tester(&self, ev: TesterEvent) {
        self.send(ConnEvent::Tester(ev));
    }

    fn meter(&self, ev: MeterEvent) {
        self.send(ConnEvent::Meter(ev));
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

struct Bt {
    // Keeps the D-Bus connection alive.
    _session: Session,
    adapter: Adapter,
}

async fn open_adapter(evt: &Events) -> Option<Bt> {
    let result = async {
        let session = Session::new().await?;
        let adapter = session.default_adapter().await?;
        let powered = adapter.is_powered().await?;
        Ok::<_, bluer::Error>((session, adapter, powered))
    }
    .await;
    match result {
        Ok((session, adapter, powered)) => {
            emit(evt, Event::AdapterReady { name: adapter.name().to_string(), powered });
            let a = adapter.clone();
            let e = evt.clone();
            tokio::spawn(async move {
                if let Ok(events) = a.events().await {
                    pin_mut!(events);
                    while let Some(ev) = events.next().await {
                        if let AdapterEvent::PropertyChanged(AdapterProperty::Powered(p)) = ev {
                            emit(&e, Event::AdapterPowered(p));
                        }
                    }
                }
            });
            Some(Bt { _session: session, adapter })
        }
        Err(e) => {
            emit(evt, Event::AdapterUnavailable(friendly_bluez_error(&e.to_string())));
            None
        }
    }
}

fn friendly_bluez_error(e: &str) -> String {
    if e.contains("org.freedesktop.DBus.Error.ServiceUnknown") || e.contains("org.bluez") && e.contains("not") {
        "The Bluetooth service (BlueZ) is not running.".into()
    } else if e.contains("NotFound") || e.contains("No Bluetooth adapter") {
        "No Bluetooth adapter was found.".into()
    } else {
        e.to_string()
    }
}

#[derive(Debug)]
pub(crate) enum Ctl {
    Stop,
    Rename(String),
    Meter(MeterCmd),
}

struct Running {
    token: Token,
    ctl: mpsc::UnboundedSender<Ctl>,
    task: JoinHandle<()>,
}

/// Shared by all connections.
#[derive(Clone)]
struct Ctx {
    adapter: Adapter,
    /// BlueZ handles one pending LE connection at a time, so connection
    /// setups are serialised.
    connect_lock: Arc<tokio::sync::Mutex<()>>,
}

async fn worker(cmds: async_channel::Receiver<Cmd>, evt: Events) {
    let mut bt = open_adapter(&evt).await;
    let connect_lock = Arc::new(tokio::sync::Mutex::new(()));
    let mut scan: Option<JoinHandle<()>> = None;
    let mut running: HashMap<DeviceId, Running> = HashMap::new();
    // Connections asked to stop that may still be disconnecting.
    let mut stopping: Vec<(DeviceId, JoinHandle<()>)> = Vec::new();
    let usb_watch = tokio::spawn(crate::usb::watch(evt.clone()));

    let stop =
        |running: &mut HashMap<DeviceId, Running>, stopping: &mut Vec<(DeviceId, JoinHandle<()>)>, a: DeviceId| {
            if let Some(r) = running.remove(&a) {
                let _ = r.ctl.send(Ctl::Stop);
                stopping.push((a, r.task));
            }
        };

    while let Ok(cmd) = cmds.recv().await {
        stopping.retain(|(_, h)| !h.is_finished());
        running.retain(|_, r| !r.task.is_finished());
        match cmd {
            Cmd::Shutdown => break,
            Cmd::Disconnect(a) => stop(&mut running, &mut stopping, a),
            Cmd::DisconnectAll => {
                for a in running.keys().copied().collect::<Vec<_>>() {
                    stop(&mut running, &mut stopping, a);
                }
            }
            Cmd::Rename(a, name) => match running.get(&a) {
                Some(r) => {
                    let _ = r.ctl.send(Ctl::Rename(name));
                }
                None => emit(&evt, Event::Error("Not connected; the name was not changed".into())),
            },
            Cmd::Meter(a, m) => match running.get(&a) {
                Some(r) => {
                    let _ = r.ctl.send(Ctl::Meter(m));
                }
                None => emit(&evt, Event::Error("The multimeter is not connected".into())),
            },
            Cmd::StopScan => {
                if let Some(h) = scan.take() {
                    h.abort();
                    emit(&evt, Event::Scanning(false));
                }
            }
            Cmd::Connect { address, token } => {
                if running.get(&address).is_some_and(|r| r.token == token) {
                    continue;
                }
                stop(&mut running, &mut stopping, address);
                // Two tasks on one device would disconnect each other.
                let (same, rest): (Vec<_>, Vec<_>) = stopping.drain(..).partition(|(a, _)| *a == address);
                stopping = rest;
                for (_, h) in same {
                    let _ = timeout(Duration::from_secs(3), h).await;
                }
                let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
                let tx = ConnTx { evt: evt.clone(), address, token };
                let task = if address == DEMO_ADDRESS {
                    tokio::spawn(run_demo_tester(ctl_rx, tx))
                } else if address == DEMO_METER_ADDRESS {
                    tokio::spawn(run_demo_meter(ctl_rx, tx))
                } else if address == DEMO_USB_ADDRESS {
                    tokio::spawn(run_demo_usb_tester(ctl_rx, tx))
                } else if let DeviceId::Usb(port) = address {
                    tokio::spawn(run_connection(Target::Usb(port), ctl_rx, tx))
                } else if let DeviceId::Ble(ble_address) = address {
                    if bt.is_none() {
                        bt = open_adapter(&evt).await;
                    }
                    let Some(b) = bt.as_ref() else {
                        let error = Some("Bluetooth is not available".into());
                        tx.send(ConnEvent::Disconnected { by_user: false, error, retrying: false });
                        continue;
                    };
                    if let Some(h) = scan.take() {
                        h.abort();
                        emit(&evt, Event::Scanning(false));
                    }
                    let ctx = Ctx { adapter: b.adapter.clone(), connect_lock: connect_lock.clone() };
                    tokio::spawn(run_connection(Target::Ble(ctx, ble_address), ctl_rx, tx))
                } else {
                    unreachable!()
                };
                running.insert(address, Running { token, ctl: ctl_tx, task });
            }
            Cmd::StartScan | Cmd::PowerOn => {
                if matches!(cmd, Cmd::StartScan) {
                    crate::usb::report_present(&evt).await;
                }
                if bt.is_none() {
                    bt = open_adapter(&evt).await;
                }
                let Some(b) = bt.as_ref() else { continue };
                if matches!(cmd, Cmd::PowerOn) {
                    if let Err(e) = b.adapter.set_powered(true).await {
                        emit(&evt, Event::Error(format!("Could not turn Bluetooth on: {e}")));
                    }
                } else if scan.as_ref().is_none_or(|h| h.is_finished()) {
                    let (a, e) = (b.adapter.clone(), evt.clone());
                    scan = Some(tokio::spawn(async move {
                        if let Err(error) = run_scan(a, e.clone()).await {
                            emit(&e, Event::Error(format!("Scanning failed: {error}")));
                        }
                        emit(&e, Event::Scanning(false));
                    }));
                }
            }
        }
    }

    if let Some(h) = scan.take() {
        h.abort();
    }
    usb_watch.abort();
    for a in running.keys().copied().collect::<Vec<_>>() {
        stop(&mut running, &mut stopping, a);
    }
    let _ = timeout(Duration::from_secs(2), futures::future::join_all(stopping.into_iter().map(|(_, h)| h))).await;
}

// ---------------------------------------------------------------- scanning

fn le_filter() -> DiscoveryFilter {
    DiscoveryFilter { transport: DiscoveryTransport::Le, ..Default::default() }
}

async fn run_scan(adapter: Adapter, evt: Events) -> bluer::Result<()> {
    let _ = adapter.set_discovery_filter(le_filter()).await;
    let stream = adapter.discover_devices().await?;
    pin_mut!(stream);
    emit(&evt, Event::Scanning(true));
    let mut watchers: HashMap<Address, JoinHandle<()>> = HashMap::new();
    let deadline = sleep(SCAN_DURATION);
    pin_mut!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            ev = stream.next() => match ev {
                Some(AdapterEvent::DeviceAdded(addr)) => {
                    if let Ok(dev) = adapter.device(addr) {
                        report(&dev, &evt).await;
                        if let Some(old) = watchers.insert(addr, tokio::spawn(watch_device(dev, evt.clone()))) {
                            old.abort();
                        }
                    }
                }
                Some(AdapterEvent::DeviceRemoved(addr)) => {
                    if let Some(h) = watchers.remove(&addr) {
                        h.abort();
                    }
                    emit(&evt, Event::DeviceGone(DeviceId::Ble(addr)));
                }
                Some(_) => {}
                None => break,
            }
        }
    }
    for h in watchers.values() {
        h.abort();
    }
    Ok(())
}

async fn watch_device(dev: Device, evt: Events) {
    let Ok(events) = dev.events().await else { return };
    pin_mut!(events);
    let mut last = Instant::now();
    while let Some(DeviceEvent::PropertyChanged(p)) = events.next().await {
        let relevant = matches!(
            p,
            DeviceProperty::Rssi(_)
                | DeviceProperty::Name(_)
                | DeviceProperty::Uuids(_)
                | DeviceProperty::ManufacturerData(_)
        );
        if relevant && last.elapsed() >= REPORT_INTERVAL {
            last = Instant::now();
            report(&dev, &evt).await;
        }
    }
}

/// The tester's BLE module advertises its own MAC address as manufacturer
/// data (company id = first two address bytes, value = the other four).
fn mac_signature(addr: Address, data: &HashMap<u16, Vec<u8>>) -> bool {
    let key = u16::from_le_bytes([addr.0[0], addr.0[1]]);
    data.get(&key).is_some_and(|v| v.as_slice() == &addr.0[2..6])
}

/// Advertised names of OWON multimeters.
const METER_NAME_PREFIXES: [&str; 9] = ["BDM", "B33", "B35", "B41", "OW1", "OW2", "OW5", "OW6", "CM2"];

/// Guesses what an advertising device is.
pub fn classify_advert(
    name: Option<&str>,
    address: Address,
    mfr: &HashMap<u16, Vec<u8>>,
    uuids: &HashSet<Uuid>,
) -> Guess {
    let name = name.unwrap_or("");
    if name.starts_with("C2") {
        Guess::Tester
    } else if METER_NAME_PREFIXES.iter().any(|p| name.starts_with(p)) {
        Guess::Meter
    } else if mac_signature(address, mfr) {
        Guess::Tester
    } else if uuids.contains(&Uuid::from_u128(protocol::SERVICE_UUID)) {
        Guess::Unknown
    } else {
        Guess::Other
    }
}

async fn report(dev: &Device, evt: &Events) {
    let address = dev.address();
    let name = dev.name().await.ok().flatten();
    let rssi = dev.rssi().await.ok().flatten();
    let uuids = dev.uuids().await.ok().flatten().unwrap_or_default();
    let mfr = dev.manufacturer_data().await.ok().flatten().unwrap_or_default();
    let guess = classify_advert(name.as_deref(), address, &mfr, &uuids);
    emit(evt, Event::DeviceSeen(SeenDevice { address: DeviceId::Ble(address), name, rssi, guess }));
}

// -------------------------------------------------------------- connection

pub(crate) enum End {
    /// Disconnected on request.
    User,
    /// The link dropped; reconnect.
    Lost(String),
    /// The device went away on purpose (e.g. it started logging); don't reconnect.
    Expected(String),
}

pub(crate) enum Failure {
    /// Temporary; try again.
    Retry(String),
    /// The device is not in range; try again, less often.
    NotFound(String),
    /// A USB device is unplugged; look again soon.
    Unplugged(String),
    /// Retrying won't help.
    Fatal(String),
}

impl From<String> for Failure {
    fn from(s: String) -> Self {
        Failure::Retry(s)
    }
}

/// How to reach a device.
enum Target {
    Ble(Ctx, Address),
    Usb(UsbPort),
}

async fn run_connection(target: Target, mut ctl: mpsc::UnboundedReceiver<Ctl>, tx: ConnTx) {
    let address = tx.address;
    let mut attempt = 0u32;
    let mut not_found = 0u32;
    loop {
        attempt += 1;
        tx.send(ConnEvent::Connecting { attempt });
        let result = match &target {
            Target::Ble(ctx, a) => run_session(ctx, *a, &mut ctl, &tx).await,
            Target::Usb(port) => crate::usb::run_session(*port, &mut ctl, &tx).await,
        };
        let mut quick = false;
        let error = match result {
            Ok(End::User) => {
                tx.send(ConnEvent::Disconnected { by_user: true, error: None, retrying: false });
                return;
            }
            Ok(End::Expected(msg)) | Err(Failure::Fatal(msg)) => {
                log::info!("Connection to {address} ended: {msg}");
                tx.send(ConnEvent::Disconnected { by_user: false, error: Some(msg), retrying: false });
                return;
            }
            Ok(End::Lost(reason)) => {
                attempt = 0;
                not_found = 0;
                reason
            }
            Err(Failure::NotFound(e)) => {
                not_found += 1;
                e
            }
            Err(Failure::Unplugged(e)) => {
                quick = true;
                e
            }
            Err(Failure::Retry(e)) => e,
        };
        log::info!("Connection to {address} ended: {error}");
        tx.send(ConnEvent::Disconnected { by_user: false, error: Some(error), retrying: true });

        // Searching for a device that is switched off keeps the radio busy,
        // which disturbs the other connection; back off further.
        let secs = if quick {
            2
        } else if not_found >= 3 {
            60
        } else {
            (2 * attempt as u64).clamp(1, 10)
        };
        let backoff = sleep(Duration::from_secs(secs));
        pin_mut!(backoff);
        loop {
            tokio::select! {
                _ = &mut backoff => break,
                c = ctl.recv() => match c {
                    Some(Ctl::Stop) | None => {
                        tx.send(ConnEvent::Disconnected { by_user: true, error: None, retrying: false });
                        return;
                    }
                    Some(_) => tx.send(ConnEvent::Error("Not connected; try again once the device is back".into())),
                },
            }
        }
    }
}

async fn find_device(adapter: &Adapter, address: Address) -> Result<Device, Failure> {
    let known = adapter.device_addresses().await.map_err(err)?;
    if !known.contains(&address) {
        let _ = adapter.set_discovery_filter(le_filter()).await;
        let stream = adapter.discover_devices().await.map_err(err)?;
        pin_mut!(stream);
        let found = timeout(Duration::from_secs(20), async {
            while let Some(ev) = stream.next().await {
                if matches!(ev, AdapterEvent::DeviceAdded(a) if a == address) {
                    return true;
                }
            }
            false
        })
        .await;
        if found != Ok(true) {
            return Err(Failure::NotFound("The device was not found nearby. Is its Bluetooth switched on?".into()));
        }
    }
    Ok(adapter.device(address).map_err(err)?)
}

async fn connect(device: &Device) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..3 {
        match timeout(Duration::from_secs(15), device.connect()).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(e)) => last = e.to_string(),
            Err(_) => {
                last = "Connection timed out".into();
                let _ = device.disconnect().await;
            }
        }
        sleep(Duration::from_millis(700)).await;
    }
    Err(last)
}

async fn wait_services(device: &Device) -> Result<(), String> {
    for _ in 0..150 {
        if device.is_services_resolved().await.unwrap_or(false) {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err("Timed out while discovering the device's services".into())
}

/// Decides what a device is from the characteristics of its `0xFFF0`
/// service, given as (UUID, can notify).  Only the multimeter has `0xFFF4`.
fn classify_gatt(chars: &[(u128, bool)]) -> Result<Kind, String> {
    let has = |uuid: u128, notify: bool| chars.iter().any(|(u, n)| *u == uuid && (*n || !notify));
    if has(owon::DATA_UUID, true) {
        Ok(Kind::Meter)
    } else if has(protocol::NOTIFY_UUID, true)
        && (has(protocol::WRITE_UUID, false) || has(protocol::WRITE_ALT_UUID, false))
    {
        Ok(Kind::Tester)
    } else {
        Err("This device is neither a C2 tester nor a supported multimeter".into())
    }
}

struct MeterChars {
    data: Characteristic,
    info: Option<Characteristic>,
    auth: Option<(Characteristic, WriteOp)>,
    button: Option<(Characteristic, WriteOp)>,
}

enum Gatt {
    Tester { notify: Characteristic, write: Characteristic },
    Meter(MeterChars),
}

fn write_op(flags: &CharacteristicFlags) -> WriteOp {
    if flags.write_without_response {
        WriteOp::Command
    } else {
        WriteOp::Request
    }
}

async fn find_gatt(device: &Device) -> Result<Gatt, Failure> {
    for service in device.services().await.map_err(err)? {
        if service.uuid().await.map_err(err)? != Uuid::from_u128(protocol::SERVICE_UUID) {
            continue;
        }
        let mut chars: Vec<(u128, Characteristic, CharacteristicFlags)> = Vec::new();
        for c in service.characteristics().await.map_err(err)? {
            let uuid = c.uuid().await.map_err(err)?.as_u128();
            let flags = c.flags().await.map_err(err)?;
            chars.push((uuid, c, flags));
        }
        let summary: Vec<(u128, bool)> = chars.iter().map(|(u, _, f)| (*u, f.notify || f.indicate)).collect();
        log::debug!("GATT characteristics of {}: {summary:x?}", device.address());
        let kind = classify_gatt(&summary).map_err(Failure::Fatal)?;
        let take = |uuid: u128| chars.iter().find(|(u, ..)| *u == uuid).map(|(_, c, f)| (c.clone(), *f));
        let found = match kind {
            Kind::Meter => take(owon::DATA_UUID).map(|(data, _)| {
                Gatt::Meter(MeterChars {
                    data,
                    info: take(owon::INFO_UUID).map(|(c, _)| c),
                    auth: take(owon::AUTH_UUID).map(|(c, f)| (c, write_op(&f))),
                    button: take(owon::BUTTON_UUID).map(|(c, f)| (c, write_op(&f))),
                })
            }),
            Kind::Tester => {
                let notify = take(protocol::NOTIFY_UUID).map(|(c, _)| c);
                let write = take(protocol::WRITE_UUID).or_else(|| take(protocol::WRITE_ALT_UUID)).map(|(c, _)| c);
                notify.zip(write).map(|(notify, write)| Gatt::Tester { notify, write })
            }
        };
        return found.ok_or_else(|| Failure::Fatal("The device's measurement characteristics are missing".into()));
    }
    Err(Failure::Fatal("This device does not offer a supported measurement service".into()))
}

async fn run_session(
    ctx: &Ctx,
    address: Address,
    ctl: &mut mpsc::UnboundedReceiver<Ctl>,
    tx: &ConnTx,
) -> Result<End, Failure> {
    let setup = async {
        let device = find_device(&ctx.adapter, address).await?;
        {
            let _guard = ctx.connect_lock.lock().await;
            if !device.is_connected().await.unwrap_or(false) {
                connect(&device).await?;
            }
            wait_services(&device).await?;
        }
        let gatt = find_gatt(&device).await?;
        Ok::<_, Failure>((device, gatt))
    };
    // Commands need a connection, so ignore them until connected.
    let stop_requested = async {
        loop {
            match ctl.recv().await {
                Some(Ctl::Stop) | None => break,
                Some(other) => log::debug!("Ignoring {other:?} while connecting"),
            }
        }
    };
    let (device, gatt) = tokio::select! {
        r = setup => r?,
        _ = stop_requested => {
            if let Ok(d) = ctx.adapter.device(address) {
                let _ = d.disconnect().await;
            }
            return Ok(End::User);
        }
    };

    let name = device.name().await.ok().flatten();
    match gatt {
        Gatt::Tester { notify, write } => {
            tx.send(ConnEvent::Connected { name, kind: Kind::Tester });
            run_tester_session(&device, notify, write, ctl, tx).await
        }
        Gatt::Meter(chars) => {
            tx.send(ConnEvent::Connected { name, kind: Kind::Meter });
            run_meter_session(&device, chars, ctl, tx).await
        }
    }
}

async fn write_with(c: &Characteristic, data: &[u8], op: WriteOp) -> Result<(), String> {
    let req = CharacteristicWriteRequest { op_type: op, ..Default::default() };
    c.write_ext(data, &req).await.map_err(|e| format!("Write failed: {e}"))
}

// ------------------------------------------------------------------ tester

struct Outstanding {
    cmd: Command,
    frame: Vec<u8>,
    deadline: Instant,
}

fn deadline_for(cmd: Command, interval_ms: u16) -> Instant {
    let wait = match cmd {
        // The tester answers a poll once ten new samples are ready.
        Command::Poll => Duration::from_millis((30 * interval_ms.max(1) as u64).max(2500)),
        _ => Duration::from_secs(2),
    };
    Instant::now() + wait
}

async fn run_tester_session(
    device: &Device,
    notify_char: Characteristic,
    write_char: Characteristic,
    ctl: &mut mpsc::UnboundedReceiver<Ctl>,
    tx: &ConnTx,
) -> Result<End, Failure> {
    let notify = notify_char.notify().await.map_err(err)?;
    pin_mut!(notify);
    let events = device.events().await.map_err(err)?;
    pin_mut!(events);

    let mut parser = FrameParser::default();
    let mut queue: VecDeque<Command> =
        VecDeque::from([Command::Identify, Command::DeviceInfo, Command::GetName, Command::Start]);
    // A rename waiting to be sent, and the one sent last.
    let mut pending_name: Option<(String, Vec<u8>)> = None;
    let mut sent_name: Option<String> = None;
    let mut outstanding: Option<Outstanding>;
    let mut misses = 0u32;
    let mut interval_ms = 20u16;

    macro_rules! send_next {
        () => {{
            let (cmd, frame) = match pending_name.take() {
                Some((name, f)) => {
                    sent_name = Some(name);
                    (Command::SetName, f)
                }
                None => {
                    let cmd = queue.pop_front().unwrap_or(Command::Poll);
                    (cmd, protocol::encode_request(cmd, &[]))
                }
            };
            write_with(&write_char, &frame, WriteOp::Command).await?;
            outstanding = Some(Outstanding { cmd, deadline: deadline_for(cmd, interval_ms), frame });
        }};
    }

    send_next!();
    loop {
        let deadline =
            outstanding.as_ref().map(|o| o.deadline).unwrap_or_else(|| Instant::now() + Duration::from_secs(1));
        tokio::select! {
            data = notify.next() => {
                let Some(bytes) = data else {
                    return Ok(End::Lost("Notifications stopped".into()));
                };
                for frame in parser.push(&bytes) {
                    let code = frame.cmd;
                    match Response::from_frame(frame) {
                        Response::Identify(m) => tx.tester(TesterEvent::Identified(m)),
                        Response::Started => {}
                        Response::DeviceInfo(i) => tx.tester(TesterEvent::Info(i)),
                        Response::Realtime(b) => {
                            interval_ms = b.interval_ms;
                            tx.tester(TesterEvent::Data(b));
                        }
                        Response::NameStored => {
                            if let Some(n) = sent_name.take() {
                                tx.send(ConnEvent::Renamed(n));
                            }
                        }
                        Response::Name(n) => tx.tester(TesterEvent::BleName(n)),
                        Response::Unknown(f) => log::debug!("Unexpected frame {:02X}: {:02X?}", f.cmd, f.data),
                    }
                    if outstanding.as_ref().is_some_and(|o| o.cmd as u8 == code) {
                        outstanding = None;
                        misses = 0;
                    }
                }
                if outstanding.is_none() {
                    send_next!();
                }
            }
            ev = events.next() => match ev {
                Some(DeviceEvent::PropertyChanged(DeviceProperty::Connected(false))) | None => {
                    return Ok(End::Lost("Connection lost".into()));
                }
                Some(DeviceEvent::PropertyChanged(DeviceProperty::Rssi(r))) => tx.send(ConnEvent::Rssi(r)),
                _ => {}
            },
            c = ctl.recv() => match c {
                Some(Ctl::Rename(name)) => match protocol::encode_set_name(&name) {
                    Ok(frame) => {
                        pending_name = Some((name, frame));
                        // The tester only keeps the latest request, so a
                        // pending poll can simply be replaced.
                        if outstanding.as_ref().is_none_or(|o| o.cmd == Command::Poll) {
                            send_next!();
                        }
                    }
                    Err(e) => tx.send(ConnEvent::Error(e.to_string())),
                },
                Some(Ctl::Meter(_)) => tx.send(ConnEvent::Error("This is a tester, not a multimeter".into())),
                Some(Ctl::Stop) | None => {
                    let _ = device.disconnect().await;
                    return Ok(End::User);
                }
            },
            _ = sleep_until(deadline) => {
                if let Some(o) = outstanding.take() {
                    misses += 1;
                    log::debug!("No answer to {:?} (miss {misses})", o.cmd);
                    if misses >= 4 {
                        return Err(Failure::Retry("The tester stopped responding".into()));
                    }
                    parser.reset();
                    write_with(&write_char, &o.frame, WriteOp::Command).await?;
                    outstanding = Some(Outstanding { deadline: deadline_for(o.cmd, interval_ms), ..o });
                } else {
                    send_next!();
                }
            }
        }
    }
}

// --------------------------------------------------------------- multimeter

async fn read_info(c: Option<&Characteristic>) -> Option<owon::Info> {
    let bytes = c?.read().await.map_err(|e| log::debug!("Reading meter info failed: {e}")).ok()?;
    log::debug!("Meter info: {bytes:02X?}");
    owon::parse_info(&bytes)
}

/// The identity check of the vendor apps.  The meter doesn't seem to depend
/// on it, so a failure is only logged.
async fn authenticate(auth: &(Characteristic, WriteOp), legacy: bool) {
    let challenge = owon::Challenge::random(legacy);
    if let Err(e) = write_with(&auth.0, &challenge.request, auth.1).await {
        log::warn!("Meter identity check: {e}");
        return;
    }
    sleep(Duration::from_millis(100)).await;
    match auth.0.read().await {
        Ok(reply) if challenge.verify(&reply) => log::debug!("Meter identity check passed"),
        Ok(reply) => log::warn!("Meter identity check: unexpected reply {reply:02X?}"),
        Err(e) => log::warn!("Meter identity check: {e}"),
    }
}

struct Download {
    decoder: owon::LogDecoder,
    total: Option<u32>,
    last_data: Instant,
}

async fn run_meter_session(
    device: &Device,
    chars: MeterChars,
    ctl: &mut mpsc::UnboundedReceiver<Ctl>,
    tx: &ConnTx,
) -> Result<End, Failure> {
    // Subscribe first so that nothing sent after the handshake is missed.
    let notify = chars.data.notify().await.map_err(err)?;
    pin_mut!(notify);
    let events = device.events().await.map_err(err)?;
    pin_mut!(events);

    let mut info = read_info(chars.info.as_ref()).await.unwrap_or(owon::Info {
        series: owon::Series::Unknown(0),
        battery: None,
        firmware: None,
        log: owon::LogState::Unsupported,
    });
    tx.meter(MeterEvent::Info(info.clone()));
    if let Some(auth) = &chars.auth {
        authenticate(auth, info.dialect().legacy).await;
    }

    let mut frames = owon::FrameDecoder::new(info.dialect());
    let mut download: Option<Download> = None;
    let mut info_tick = tokio::time::interval_at(Instant::now() + METER_INFO_INTERVAL, METER_INFO_INTERVAL);
    info_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let text_command = |cmd: [u8; owon::COMMAND_LEN]| {
        let auth = chars.auth.clone();
        async move {
            match auth {
                Some((c, op)) => write_with(&c, &cmd, op).await,
                None => Err("The multimeter does not accept commands".to_string()),
            }
        }
    };

    loop {
        let log_deadline =
            download.as_ref().map_or(Instant::now() + Duration::from_secs(3600), |d| d.last_data + LOG_IDLE_TIMEOUT);
        tokio::select! {
            data = notify.next() => {
                let Some(bytes) = data else {
                    return Ok(End::Lost("Notifications stopped".into()));
                };
                log::trace!("Meter data: {bytes:02X?}");
                match download.as_mut() {
                    None => {
                        for reading in frames.push(&bytes) {
                            tx.meter(MeterEvent::Reading { at: std::time::Instant::now(), reading });
                        }
                    }
                    Some(d) => {
                        d.last_data = Instant::now();
                        let done = d.decoder.push(&bytes);
                        tx.meter(MeterEvent::LogProgress { received: d.decoder.received(), total: d.total });
                        if done {
                            match d.decoder.finish() {
                                Ok(log) => tx.meter(MeterEvent::Log(Arc::new(log))),
                                Err(e) => tx.meter(MeterEvent::LogFailed(e)),
                            }
                            download = None;
                            frames.reset();
                        }
                    }
                }
            }
            ev = events.next() => match ev {
                Some(DeviceEvent::PropertyChanged(DeviceProperty::Connected(false))) | None => {
                    return Ok(End::Lost("Connection lost".into()));
                }
                Some(DeviceEvent::PropertyChanged(DeviceProperty::Rssi(r))) => tx.send(ConnEvent::Rssi(r)),
                _ => {}
            },
            _ = info_tick.tick(), if download.is_none() => {
                if let Some(i) = read_info(chars.info.as_ref()).await {
                    info = i;
                    tx.meter(MeterEvent::Info(info.clone()));
                }
            }
            _ = sleep_until(log_deadline), if download.is_some() => {
                download = None;
                frames.reset();
                tx.meter(MeterEvent::LogFailed("The multimeter stopped sending its log".into()));
            }
            c = ctl.recv() => match c {
                Some(Ctl::Stop) | None => {
                    let _ = device.disconnect().await;
                    return Ok(End::User);
                }
                Some(Ctl::Rename(name)) => {
                    let result = if info.can_rename() {
                        match owon::cmd_rename(&name) {
                            Ok(cmd) => text_command(cmd).await,
                            Err(e) => Err(e.to_string()),
                        }
                    } else {
                        Err("This multimeter's firmware can't change its name".into())
                    };
                    match result {
                        Ok(()) => tx.send(ConnEvent::Renamed(name)),
                        Err(e) => tx.send(ConnEvent::Error(e)),
                    }
                }
                Some(Ctl::Meter(MeterCmd::Press { button, long })) => match &chars.button {
                    Some((c, op)) => {
                        let frame = owon::button_frame(button, long);
                        log::debug!("Meter button {button:?} (long: {long}): {frame:02X?} ({op:?})");
                        write_with(c, &frame, *op).await?
                    }
                    None => tx.send(ConnEvent::Error("The multimeter has no remote control".into())),
                },
                Some(Ctl::Meter(MeterCmd::StartRecording { interval_s, count })) => {
                    if info.log_has_date() {
                        text_command(owon::cmd_set_date(chrono::Local::now().naive_local())).await?;
                        sleep(Duration::from_millis(200)).await;
                    }
                    text_command(owon::cmd_record(interval_s, count)).await?;
                    // The meter turns its Bluetooth off while it records.
                    sleep(Duration::from_secs(1)).await;
                    let _ = device.disconnect().await;
                    return Ok(End::Expected(
                        "The multimeter is recording. Connect again when it has finished to download the readings.".into(),
                    ));
                }
                Some(Ctl::Meter(MeterCmd::StopRecording)) => {
                    text_command(owon::cmd_stop_recording()).await?;
                    sleep(Duration::from_millis(300)).await;
                    if let Some(i) = read_info(chars.info.as_ref()).await {
                        info = i;
                        tx.meter(MeterEvent::Info(info.clone()));
                    }
                }
                Some(Ctl::Meter(MeterCmd::DownloadLog)) => {
                    if download.is_some() {
                        continue;
                    }
                    let Some((auth, op)) = chars.auth.clone() else {
                        tx.meter(MeterEvent::LogFailed("The multimeter does not accept commands".into()));
                        continue;
                    };
                    write_with(&auth, &owon::cmd_log_length(), op).await?;
                    sleep(Duration::from_millis(200)).await;
                    let total = match auth.read().await {
                        Ok(r) if r.len() >= 4 => Some(u32::from_le_bytes([r[0], r[1], r[2], r[3]])),
                        Ok(r) => {
                            log::debug!("Unexpected log length reply {r:02X?}");
                            None
                        }
                        Err(e) => {
                            log::debug!("Reading the log length failed: {e}");
                            None
                        }
                    };
                    tx.meter(MeterEvent::LogProgress { received: 0, total });
                    write_with(&auth, &owon::cmd_read_log(info.log_has_date()), op).await?;
                    download = Some(Download {
                        decoder: owon::LogDecoder::new(info.log_has_date(), info.dialect()),
                        total,
                        last_data: Instant::now(),
                    });
                }
            },
        }
    }
}

// -------------------------------------------------------------------- demo

pub(crate) fn demo_stop(c: Option<Ctl>, tx: &ConnTx) -> bool {
    if matches!(c, Some(Ctl::Stop) | None) {
        tx.send(ConnEvent::Disconnected { by_user: true, error: None, retrying: false });
        true
    } else {
        false
    }
}

/// A simulated tester: a phone negotiating 9 V over USB PD and charging.
async fn run_demo_tester(mut ctl: mpsc::UnboundedReceiver<Ctl>, tx: ConnTx) {
    tx.send(ConnEvent::Connecting { attempt: 1 });
    sleep(Duration::from_millis(300)).await;
    tx.send(ConnEvent::Connected { name: Some("C2_DEMO".into()), kind: Kind::Tester });
    tx.tester(TesterEvent::Identified(Model::Pro));
    tx.tester(TesterEvent::Info(DeviceInfo {
        bootloader_version: 10,
        hardware_version: 15,
        firmware_series: 25,
        model_code: 0x030A,
        serial: "DEMO0001".into(),
        model_name: "ATK-C2 Pro".into(),
        is_lite: false,
    }));
    tx.tester(TesterEvent::BleName("C2_DEMO".into()));
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let mut t = 0.0f64;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                tx.tester(TesterEvent::Data(demo_block(t)));
                t += 0.2;
            }
            c = ctl.recv() => match c {
                Some(Ctl::Rename(n)) => {
                    tx.send(ConnEvent::Renamed(n.clone()));
                    tx.tester(TesterEvent::BleName(n));
                }
                c => {
                    if demo_stop(c, &tx) {
                        return;
                    }
                }
            },
        }
    }
}

/// A simulated FNIRSI tester on USB: no CC lines, no protocol detection.
async fn run_demo_usb_tester(mut ctl: mpsc::UnboundedReceiver<Ctl>, tx: ConnTx) {
    tx.send(ConnEvent::Connecting { attempt: 1 });
    sleep(Duration::from_millis(200)).await;
    tx.send(ConnEvent::Connected { name: Some("FNIRSI FNB58".into()), kind: Kind::Tester });
    tx.tester(TesterEvent::Features(Features {
        data_lines: true,
        cc_lines: false,
        protocol: false,
        temperature: true,
        rename: false,
    }));
    tx.tester(TesterEvent::Details(TesterDetails {
        model: Some("FNIRSI FNB58 or FNB-C2".into()),
        serial: Some("0123456789AB".into()),
        firmware: None,
        hardware: Some("2.00".into()),
    }));
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let mut t = 0.0f64;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let mut b = demo_block(t);
                b.protocol = protocol::ProtocolInfo::default();
                for s in &mut b.samples {
                    s.cc1 = f32::NAN;
                    s.cc2 = f32::NAN;
                }
                b.temperature_c = 27;
                tx.tester(TesterEvent::Data(b));
                t += 0.2;
            }
            c = ctl.recv() => {
                if matches!(c, Some(Ctl::Stop) | None) {
                    tx.send(ConnEvent::Disconnected { by_user: true, error: None, retrying: false });
                    return;
                }
                tx.send(ConnEvent::Error("This device can't be renamed".into()));
            }
        }
    }
}

fn demo_block(t0: f64) -> RealtimeBlock {
    let pd = t0 >= 4.0;
    let samples = (0..protocol::SAMPLES_PER_BLOCK)
        .map(|k| {
            let t = t0 + k as f64 * 0.02;
            let ripple = 0.006 * (t * 37.0).sin() + 0.003 * (t * 211.0).sin();
            let (v, i) = if pd {
                let fade = ((t - 4.0) / 240.0).min(1.0);
                (9.02 + ripple - 0.05 * fade, 2.15 - 0.9 * fade + 0.06 * (t * 0.7).sin() + 0.01 * (t * 53.0).sin())
            } else {
                (5.08 + ripple, 0.45 + 0.01 * (t * 53.0).sin())
            };
            protocol::RawSample {
                voltage: v as f32,
                current: i.max(0.0) as f32,
                d_plus: (0.60 + 0.002 * (t * 13.0).sin()) as f32,
                d_minus: (0.59 + 0.002 * (t * 17.0).cos()) as f32,
                cc1: if pd { 1.07 } else { 1.66 } + 0.003 * (t * 7.0).sin() as f32,
                cc2: 0.012 + 0.002 * (t * 5.0).cos() as f32,
            }
        })
        .collect();
    let s = |x: &str| x.to_string();
    let protocol = if pd {
        protocol::ProtocolInfo {
            name: s("PD3.0"),
            power: s("65W"),
            gear: s("2/5"),
            gear_detail: s("FIX"),
            request: s("9.00V"),
            request_detail: s("3.00A"),
        }
    } else {
        protocol::ProtocolInfo {
            name: s("Protocols"),
            gear: s("---NA---"),
            request: s("---NA---"),
            ..Default::default()
        }
    };
    RealtimeBlock { protocol, power_bank_mode: false, interval_ms: 20, samples, status: 1, temperature_c: 31 }
}

/// State of the simulated multimeter.
struct DemoMeter {
    function: usize,
    flags: u16,
    held: Option<i32>,
    reference: i32,
    log: owon::LogState,
}

/// Functions the simulated meter cycles through with Select:
/// (function, prefix, decimals).
const DEMO_FUNCTIONS: [(owon::Function, owon::Prefix, u8); 4] = [
    (owon::Function::Celsius, owon::Prefix::None, 1),
    (owon::Function::DcVoltage, owon::Prefix::None, 4),
    (owon::Function::AcVoltage, owon::Prefix::None, 1),
    (owon::Function::Resistance, owon::Prefix::Kilo, 3),
];

impl DemoMeter {
    fn raw(&self, t: f64) -> i32 {
        let noise = (t * 7.3).sin() + 0.5 * (t * 23.1).sin();
        match self.function {
            // A charging battery pack warming up.
            0 => (240.0 + 90.0 * (1.0 - (-t / 90.0).exp()) + noise) as i32,
            1 => (37_000.0 + 1_500.0 * (1.0 - (-t / 200.0).exp()) + 6.0 * noise) as i32,
            2 => (2_301.0 + 8.0 * (t / 13.0).sin() + noise) as i32,
            _ => (4_702.0 + 2.0 * noise) as i32,
        }
    }

    fn reading(&self, t: f64) -> owon::Reading {
        let (function, prefix, decimals) = DEMO_FUNCTIONS[self.function];
        let raw = self.held.unwrap_or_else(|| self.raw(t)) - self.reference;
        owon::Reading {
            function,
            prefix,
            point: owon::Point::Decimals(decimals),
            negative: raw < 0,
            raw: raw.abs(),
            flags: owon::Flags(self.flags),
        }
    }

    fn info(&self) -> owon::Info {
        owon::Info { series: owon::Series::B41, battery: Some(76), firmware: Some([1, 2, 6]), log: self.log }
    }

    fn press(&mut self, button: Button, long: bool, t: f64) {
        use owon::Flags as F;
        match (button, long) {
            (Button::Select, _) => {
                self.function = (self.function + 1) % DEMO_FUNCTIONS.len();
                self.flags = F::AUTO;
                self.held = None;
                self.reference = 0;
            }
            (Button::Range, false) => self.flags &= !F::AUTO,
            (Button::Range, true) => self.flags |= F::AUTO,
            (Button::Hold, false) => {
                self.flags ^= F::HOLD;
                self.held = if self.flags & F::HOLD != 0 { Some(self.raw(t)) } else { None };
            }
            (Button::Rel, _) => {
                self.flags ^= F::REL;
                self.reference = if self.flags & F::REL != 0 { self.raw(t) } else { 0 };
            }
            (Button::MaxMin, false) => {
                self.flags = match self.flags & (F::MAX | F::MIN) {
                    0 => (self.flags & !F::AUTO) | F::MAX,
                    F::MAX => (self.flags & !F::MAX) | F::MIN,
                    _ => self.flags & !(F::MAX | F::MIN),
                }
            }
            (Button::MaxMin, true) => self.flags &= !(F::MAX | F::MIN),
            _ => {}
        }
    }
}

fn demo_log() -> owon::Log {
    let start = chrono::Local::now().naive_local() - chrono::Duration::minutes(10);
    let records = (0..30)
        .map(|k| {
            let raw = (250.0 + 40.0 * (1.0 - (-(k as f64) / 8.0).exp())) as i32;
            owon::LogRecord {
                time: Some(start + chrono::Duration::seconds(20 * (k + 1))),
                reading: owon::Reading {
                    function: owon::Function::Celsius,
                    prefix: owon::Prefix::None,
                    point: owon::Point::Decimals(1),
                    negative: false,
                    raw,
                    flags: owon::Flags::default(),
                },
            }
        })
        .collect();
    owon::Log { start: Some(start), interval_s: Some(20), records }
}

/// A simulated OWON B41T+ measuring a temperature.
async fn run_demo_meter(mut ctl: mpsc::UnboundedReceiver<Ctl>, tx: ConnTx) {
    tx.send(ConnEvent::Connecting { attempt: 1 });
    sleep(Duration::from_millis(500)).await;
    tx.send(ConnEvent::Connected { name: Some("BDM_DEMO".into()), kind: Kind::Meter });
    let mut meter =
        DemoMeter { function: 0, flags: owon::Flags::AUTO, held: None, reference: 0, log: owon::LogState::Idle };
    tx.meter(MeterEvent::Info(meter.info()));
    let start = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(333));
    loop {
        let t = start.elapsed().as_secs_f64();
        tokio::select! {
            _ = tick.tick() => {
                tx.meter(MeterEvent::Reading { at: std::time::Instant::now(), reading: meter.reading(t) });
            }
            c = ctl.recv() => match c {
                Some(Ctl::Rename(n)) => tx.send(ConnEvent::Renamed(n)),
                Some(Ctl::Meter(MeterCmd::Press { button, long })) => meter.press(button, long, t),
                Some(Ctl::Meter(MeterCmd::StartRecording { .. })) => {
                    sleep(Duration::from_secs(1)).await;
                    let error = Some("The multimeter is recording. Connect again when it has finished to download the readings.".into());
                    tx.send(ConnEvent::Disconnected { by_user: false, error, retrying: false });
                    return;
                }
                Some(Ctl::Meter(MeterCmd::StopRecording)) => {
                    meter.log = owon::LogState::Idle;
                    tx.meter(MeterEvent::Info(meter.info()));
                }
                Some(Ctl::Meter(MeterCmd::DownloadLog)) => {
                    let total = 2 * 31 + 16;
                    for k in 0..=10 {
                        tx.meter(MeterEvent::LogProgress { received: total * k / 10, total: Some(total as u32) });
                        sleep(Duration::from_millis(80)).await;
                    }
                    tx.meter(MeterEvent::Log(Arc::new(demo_log())));
                }
                c => {
                    if demo_stop(c, &tx) {
                        return;
                    }
                }
            },
        }
    }
}

// ------------------------------------------------------------ command line

/// `krizik --probe [ADDRESS] [SECONDS]`: scans for testers and
/// multimeters, or connects to one and prints decoded data.  Useful for
/// troubleshooting without the GUI.
pub fn probe_cli(address: Option<&str>, secs: u64) -> i32 {
    let target = match address.map(str::parse::<DeviceId>) {
        Some(Ok(a)) => Some(a),
        Some(Err(_)) => {
            eprintln!("Invalid device address (Bluetooth address or usb:VID:PID@BUS-PORT)");
            return 2;
        }
        None => None,
    };
    let (handle, events) = spawn();
    let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().expect("runtime");
    match target {
        None => handle.send(Cmd::StartScan),
        Some(address) => handle.send(Cmd::Connect { address, token: 1 }),
    }
    let mut code = 0;
    rt.block_on(async {
        let end = Instant::now() + Duration::from_secs(secs);
        let mut seen = HashSet::new();
        loop {
            let Ok(Ok(ev)) = timeout(end.saturating_duration_since(Instant::now()), events.recv()).await else {
                break;
            };
            match ev {
                Event::DeviceSeen(d) if d.guess.is_candidate() && seen.insert(d.address) => {
                    let what = match d.guess {
                        Guess::Tester => "tester",
                        Guess::Meter => "multimeter",
                        _ => "tester or multimeter",
                    };
                    println!("{}  {:<14} {:<21} RSSI {:?}", d.address, d.name.unwrap_or_default(), what, d.rssi)
                }
                Event::DeviceSeen(_) | Event::Scanning(_) => {}
                Event::AdapterUnavailable(e) | Event::Error(e) => {
                    eprintln!("Error: {e}");
                    code = 1;
                }
                Event::Conn { ev, .. } => match ev {
                    ConnEvent::Rssi(_) => {}
                    ConnEvent::Tester(TesterEvent::Data(b)) => print_block(&b),
                    ConnEvent::Meter(MeterEvent::Reading { reading: r, .. }) => println!(
                        "{:>9} {:<4} {:<20} {}",
                        r.display(),
                        r.unit(),
                        r.function.label(),
                        r.flags.names().join(" ")
                    ),
                    ConnEvent::Meter(MeterEvent::Info(i)) => println!(
                        "{} · firmware {} · battery {} · log {:?}",
                        i.model_name(),
                        i.firmware_text().unwrap_or_else(|| "?".into()),
                        i.battery.map_or_else(|| "?".into(), |b| format!("{b} %")),
                        i.log
                    ),
                    ConnEvent::Disconnected { error: Some(e), .. } => eprintln!("Disconnected: {e}"),
                    ConnEvent::Error(e) => {
                        eprintln!("Error: {e}");
                        code = 1;
                    }
                    other => println!("{other:?}"),
                },
                other => println!("{other:?}"),
            }
        }
    });
    handle.shutdown(Duration::from_secs(3));
    code
}

fn print_block(b: &RealtimeBlock) {
    let n = b.samples.len() as f32;
    let avg = |f: fn(&protocol::RawSample) -> f32| b.samples.iter().map(f).sum::<f32>() / n;
    let (v, i) = (avg(|s| s.voltage), avg(|s| s.current));
    println!(
        "{:7.4} V {:7.4} A {:7.3} W | D+ {:.3} D- {:.3} CC1 {:.3} CC2 {:.3} | {} ms | {} °C | {} {} {} {}",
        v,
        i,
        v * i,
        avg(|s| s.d_plus),
        avg(|s| s.d_minus),
        avg(|s| s.cc1),
        avg(|s| s.cc2),
        b.interval_ms,
        b.temperature_c,
        b.protocol.name,
        b.protocol.power,
        b.protocol.gear,
        b.protocol.request
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const FFF: fn(u16) -> u128 = |n| 0x00000000_0000_1000_8000_00805f9b34fb | ((n as u128) << 96);

    #[test]
    fn classifies_gatt() {
        let c2 = [(FFF(0xFFF1), true), (FFF(0xFFF2), true), (FFF(0xFFF3), false)];
        assert_eq!(classify_gatt(&c2), Ok(Kind::Tester));
        let meter = [
            (FFF(0xFFF1), false),
            (FFF(0xFFF2), false),
            (FFF(0xFFF3), false),
            (FFF(0xFFF4), true),
            (FFF(0xFFF5), false),
        ];
        assert_eq!(classify_gatt(&meter), Ok(Kind::Meter));
        // FFF2 that can't notify and no FFF4: neither.
        assert!(classify_gatt(&[(FFF(0xFFF2), false), (FFF(0xFFF3), false)]).is_err());
        assert_eq!(FFF(0xFFF4), owon::DATA_UUID);
    }

    #[test]
    fn classifies_adverts() {
        let addr = Address([0xCF, 0x2E, 0xA0, 0x42, 0xBD, 0x10]);
        let none = HashMap::new();
        let fff0 = HashSet::from([Uuid::from_u128(protocol::SERVICE_UUID)]);
        let mac = HashMap::from([(0x2ECF, vec![0xA0, 0x42, 0xBD, 0x10])]);
        assert_eq!(classify_advert(Some("C2_BLE03"), addr, &none, &HashSet::new()), Guess::Tester);
        assert_eq!(classify_advert(Some("BDM"), addr, &mac, &fff0), Guess::Meter);
        assert_eq!(classify_advert(Some("Renamed"), addr, &mac, &fff0), Guess::Tester);
        assert_eq!(classify_advert(Some("Renamed"), addr, &none, &fff0), Guess::Unknown);
        assert_eq!(classify_advert(None, addr, &none, &HashSet::new()), Guess::Other);
        assert!(Guess::Unknown.is_candidate() && !Guess::Other.is_candidate());
    }
}
