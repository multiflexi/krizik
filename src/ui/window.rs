//! Main window: navigation, responsive layout and event handling.
//!
//! One tester and one multimeter can be connected at the same time.  Both
//! feed the same session (time line, graph and recording).  What a device is
//! only becomes certain once it is connected, so the "one device per kind"
//! rule is applied when the connection reports its kind.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gio, glib};

use super::chart::Lane;
use super::device_page::DevicePage;
use super::devices::{AdapterState, DevicesPage};
use super::format;
use super::graph::GraphPage;
use super::live::LivePage;
use super::session_page::SessionPage;
use crate::ble::{
    self, Cmd, ConnEvent, DeviceId, Event, Guess, Kind, MeterCmd, MeterEvent, SeenDevice, TesterEvent, Token,
};
use crate::config::Config;
use crate::export::{self, Recorder};
use crate::owon::{self, LogState};
use crate::protocol;
use crate::session::{AlarmKind, Alarms, AutoStop, Session};
use crate::APP_ID;

#[derive(Clone, Debug, PartialEq)]
enum Link {
    Connecting { attempt: u32 },
    Connected,
    Lost(String),
}

/// A device the user asked to connect to.
struct Conn {
    token: Token,
    name: Option<String>,
    /// A guess until `confirmed`.
    kind: Option<Kind>,
    confirmed: bool,
    link: Link,
}

impl Conn {
    fn label(&self, address: DeviceId) -> String {
        self.name.clone().unwrap_or_else(|| address.to_string())
    }
}

/// The "Add Device" dialog while it is open.
struct AddDialog {
    dialog: adw::Dialog,
    page: Rc<DevicesPage>,
}

pub struct Window {
    pub app: adw::Application,
    pub window: adw::ApplicationWindow,
    nav: adw::NavigationView,
    toasts: adw::ToastOverlay,
    devices: Rc<DevicesPage>,
    workspace: adw::NavigationPage,
    stack: adw::ViewStack,
    title: adw::WindowTitle,
    status_label: gtk::Label,
    banner: adw::Banner,
    rec_indicator: gtk::Image,
    add_button: gtk::Button,
    live: LivePage,
    graph: GraphPage,
    sess: SessionPage,
    device: DevicePage,
    session: Rc<RefCell<Session>>,
    pub config: Rc<RefCell<Config>>,
    pub ble: ble::Handle,
    conns: RefCell<BTreeMap<DeviceId, Conn>>,
    /// The device whose data of each kind is in the session.
    sources: RefCell<HashMap<Kind, DeviceId>>,
    next_token: Cell<Token>,
    banner_target: Cell<Option<DeviceId>>,
    add_dialog: RefCell<Option<AddDialog>>,
    meter_info: RefCell<Option<owon::Info>>,
    alarms: RefCell<Alarms>,
    autostop: RefCell<AutoStop>,
    recorder: RefCell<Option<Recorder>>,
    inhibit_cookie: Cell<Option<u32>>,
    started: Cell<bool>,
    demo: bool,
    last_beep: Cell<Option<Instant>>,
}

impl Window {
    pub fn new(app: &adw::Application, demo: bool) -> Rc<Window> {
        let config = Rc::new(RefCell::new(Config::load()));
        let (ble, events) = ble::spawn();
        let session = Rc::new(RefCell::new(Session::new(config.borrow().history_minutes as f64 * 60.0)));

        let menu = gio::Menu::new();
        menu.append(Some("_Update Tester Firmware…"), Some("app.firmware"));
        menu.append(Some("_Preferences"), Some("app.preferences"));
        menu.append(Some("_About Křižík"), Some("app.about"));

        let devices = DevicesPage::new(Some(&menu), config.borrow().show_all_devices);

        let live = LivePage::new();
        let lanes: Vec<Lane> = config.borrow().graph_lanes.iter().filter_map(|k| Lane::from_key(k)).collect();
        let graph = GraphPage::new(session.clone(), lanes, config.borrow().graph_window_secs);
        let sess = SessionPage::new(config.clone());
        let device = DevicePage::new();

        let stack = adw::ViewStack::new();
        stack.add_titled_with_icon(&live.root, Some("live"), "Live", "power-profile-performance-symbolic");
        stack.add_titled_with_icon(&graph.root, Some("graph"), "Graph", "krizik-chart-symbolic");
        stack.add_titled_with_icon(&sess.root, Some("session"), "Session", "krizik-session-symbolic");
        stack.add_titled_with_icon(&device.root, Some("device"), "Devices", "preferences-system-details-symbolic");

        let title = adw::WindowTitle::new("Křižík", "");
        let switcher = adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build();
        let header = adw::HeaderBar::builder().title_widget(&switcher).build();
        let status_label =
            gtk::Label::builder().css_classes(["dim-label"]).ellipsize(gtk::pango::EllipsizeMode::End).build();
        header.pack_start(&status_label);
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .primary(true)
            .tooltip_text("Main Menu")
            .build();
        header.pack_end(&menu_button);
        let add_button =
            gtk::Button::builder().icon_name("list-add-symbolic").tooltip_text("Connect Another Device").build();
        header.pack_end(&add_button);
        let rec_indicator = gtk::Image::builder()
            .icon_name("media-record-symbolic")
            .css_classes(["recording-dot"])
            .tooltip_text("Recording")
            .visible(false)
            .build();
        header.pack_end(&rec_indicator);
        let switcher_bar = adw::ViewSwitcherBar::builder().stack(&stack).build();
        let banner = adw::Banner::builder().button_label("Disconnect").build();

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.add_top_bar(&banner);
        toolbar.set_content(Some(&stack));
        toolbar.add_bottom_bar(&switcher_bar);
        let workspace = adw::NavigationPage::builder().title("Křižík").tag("workspace").child(&toolbar).build();

        let nav = adw::NavigationView::new();
        nav.add(&devices.page);
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&nav));

        let (w, h) = std::env::var("KRIZIK_SIZE")
            .ok()
            .and_then(|s| {
                let (a, b) = s.split_once('x')?;
                Some((a.parse().ok()?, b.parse().ok()?))
            })
            .unwrap_or((1000, 720));
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Křižík")
            .icon_name(APP_ID)
            .default_width(w)
            .default_height(h)
            .width_request(360)
            .height_request(294)
            .content(&toasts)
            .build();

        // Phones and narrow windows: title in the header, view switcher at
        // the bottom and graph controls stacked.
        let narrow =
            adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 640sp").expect("valid condition"));
        narrow.add_setter(&header, "title-widget", Some(&title.to_value()));
        narrow.add_setter(&status_label, "visible", Some(&false.to_value()));
        narrow.add_setter(&switcher_bar, "reveal", Some(&true.to_value()));
        narrow.add_setter(&graph.controls, "orientation", Some(&gtk::Orientation::Vertical.to_value()));
        window.add_breakpoint(narrow);

        let win = Rc::new(Window {
            app: app.clone(),
            window,
            nav,
            toasts,
            devices,
            workspace,
            stack,
            title,
            status_label,
            banner,
            rec_indicator,
            add_button,
            live,
            graph,
            sess,
            device,
            session,
            config,
            ble,
            conns: RefCell::default(),
            sources: RefCell::default(),
            next_token: Cell::new(1),
            banner_target: Cell::new(None),
            add_dialog: RefCell::new(None),
            meter_info: RefCell::new(None),
            alarms: RefCell::default(),
            autostop: RefCell::default(),
            recorder: RefCell::new(None),
            inhibit_cookie: Cell::new(None),
            started: Cell::new(false),
            demo,
            last_beep: Cell::new(None),
        });
        win.connect_signals();
        win.setup_actions();

        let weak = Rc::downgrade(&win);
        glib::spawn_future_local(async move {
            while let Ok(ev) = events.recv().await {
                let Some(w) = weak.upgrade() else { break };
                w.handle(ev);
            }
        });

        if demo {
            win.add_demo_devices();
            for (address, name, guess) in demo_devices() {
                win.connect_to(address, Some(name.into()), guess);
            }
        } else {
            // USB devices don't need Bluetooth; Bluetooth ones wait for the adapter.
            for (address, name, kind) in win.remembered() {
                if address.is_usb() {
                    win.connect_to(address, name, if kind == Kind::Meter { Guess::Meter } else { Guess::Tester });
                }
            }
        }
        win.maybe_screenshot();
        win
    }

    fn connect_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let w = move || weak.upgrade();

        self.wire_devices_page(&self.devices);
        self.devices.scan_button.connect_clicked(glib::clone!(
            #[strong]
            w,
            move |_| {
                if let Some(w) = w() {
                    w.start_scan();
                }
            }
        ));
        self.add_button.connect_clicked(glib::clone!(
            #[strong]
            w,
            move |_| {
                if let Some(w) = w() {
                    w.show_add_device();
                }
            }
        ));

        // Leaving the workspace (back button, last device disconnected) disconnects everything.
        self.nav.connect_popped(glib::clone!(
            #[strong]
            w,
            move |_, page| {
                if let Some(w) = w() {
                    if page == &w.workspace {
                        w.disconnect_all();
                    }
                }
            }
        ));
        self.banner.connect_button_clicked(glib::clone!(
            #[strong]
            w,
            move |_| {
                if let Some(w) = w() {
                    match w.banner_target.get() {
                        Some(a) => w.disconnect_device(a),
                        None => w.leave_workspace(),
                    }
                }
            }
        ));
        self.device.disconnect.connect_clicked(glib::clone!(
            #[strong]
            w,
            move |_| {
                if let Some(w) = w() {
                    if let Some(a) = w.address_of(Kind::Tester) {
                        w.disconnect_device(a);
                    }
                }
            }
        ));
        self.device.meter_disconnect.connect_clicked(glib::clone!(
            #[strong]
            w,
            move |_| {
                if let Some(w) = w() {
                    if let Some(a) = w.address_of(Kind::Meter) {
                        w.disconnect_device(a);
                    }
                }
            }
        ));

        self.stack.connect_visible_child_name_notify(glib::clone!(
            #[strong]
            w,
            move |_| {
                if let Some(w) = w() {
                    w.graph.chart.redraw();
                }
            }
        ));

        // Statistics controls.
        let reset = glib::clone!(
            #[strong]
            w,
            move || {
                if let Some(w) = w() {
                    {
                        let mut s = w.session.borrow_mut();
                        s.reset_stats();
                        s.accumulating = true;
                    }
                    w.autostop.borrow_mut().reset();
                    let s = w.session.borrow();
                    w.live.update_stats(&s);
                    w.sess.update(&s);
                }
            }
        );
        let r2 = reset.clone();
        self.live.reset_button.connect_clicked(move |_| reset());
        self.sess.reset.connect_clicked(move |_| r2());
        self.sess.counting.connect_toggled(glib::clone!(
            #[strong]
            w,
            move |b| {
                let Some(w) = w() else { return };
                let resume = !b.is_active();
                let changed = {
                    let mut s = w.session.borrow_mut();
                    let changed = s.accumulating != resume;
                    s.accumulating = resume;
                    changed
                };
                if changed {
                    if resume {
                        w.autostop.borrow_mut().reset();
                    }
                    let s = w.session.borrow();
                    w.live.update_stats(&s);
                    w.sess.update(&s);
                }
            }
        ));

        // Recording and export.
        self.sess.record_button.connect_clicked(glib::clone!(
            #[strong]
            w,
            move |_| {
                if let Some(w) = w() {
                    w.toggle_recording();
                }
            }
        ));
        let export = glib::clone!(
            #[strong]
            w,
            move || {
                if let Some(w) = w() {
                    w.export_history();
                }
            }
        );
        let e2 = export.clone();
        self.sess.export_button.connect_clicked(move |_| export());
        self.graph.export.connect_clicked(move |_| e2());

        // Graph settings are remembered.
        self.graph.clear.connect_clicked(glib::clone!(
            #[strong]
            w,
            move |_| {
                if let Some(w) = w() {
                    w.session.borrow_mut().clear_history();
                    w.graph.chart.redraw();
                }
            }
        ));
        let save_graph = glib::clone!(
            #[strong]
            w,
            move || {
                if let Some(w) = w() {
                    let mut c = w.config.borrow_mut();
                    c.graph_lanes = w.graph.lanes().iter().map(|l| l.key().to_string()).collect();
                    c.graph_window_secs = w.graph.window_secs();
                    c.save();
                }
            }
        );
        for (_, b) in &self.graph.lane_buttons {
            let s = save_graph.clone();
            b.connect_toggled(move |_| s());
        }
        self.graph.window_dropdown.connect_selected_notify(move |_| save_graph());

        // Renaming.
        self.device.name_row.connect_apply(glib::clone!(
            #[strong]
            w,
            move |row| {
                if let Some(w) = w() {
                    w.rename(Kind::Tester, row.text().to_string());
                }
            }
        ));
        self.device.meter_name_row.connect_apply(glib::clone!(
            #[strong]
            w,
            move |row| {
                if let Some(w) = w() {
                    w.rename(Kind::Meter, row.text().to_string());
                }
            }
        ));

        // Multimeter remote control and offline recording.
        *self.live.on_meter_button.borrow_mut() = Some(Box::new(glib::clone!(
            #[strong]
            w,
            move |button, long| {
                if let Some(w) = w() {
                    w.meter_command(MeterCmd::Press { button, long });
                }
            }
        )));
        self.device.log_start.connect_activated(glib::clone!(
            #[strong]
            w,
            move |_| {
                if let Some(w) = w() {
                    w.start_meter_recording();
                }
            }
        ));
        self.device.log_stop.connect_activated(glib::clone!(
            #[strong]
            w,
            move |_| {
                if let Some(w) = w() {
                    w.meter_command(MeterCmd::StopRecording);
                }
            }
        ));
        self.device.log_download.connect_activated(glib::clone!(
            #[strong]
            w,
            move |_| {
                if let Some(w) = w() {
                    w.download_meter_log();
                }
            }
        ));
    }

    /// Hooks up the buttons of a device list (start page or "Add Device").
    fn wire_devices_page(self: &Rc<Self>, page: &Rc<DevicesPage>) {
        let weak = Rc::downgrade(self);
        *page.on_status_button.borrow_mut() = Some(Box::new(glib::clone!(
            #[strong]
            weak,
            move |state| {
                let Some(w) = weak.upgrade() else { return };
                match state {
                    AdapterState::Off => w.ble.send(Cmd::PowerOn),
                    _ => w.ble.send(Cmd::StartScan),
                }
            }
        )));
        *page.on_activate.borrow_mut() = Some(Box::new(glib::clone!(
            #[strong]
            weak,
            move |dev: SeenDevice| {
                let Some(w) = weak.upgrade() else { return };
                // Closing runs the "closed" handler, which clears `add_dialog`.
                let dialog = w.add_dialog.borrow().as_ref().map(|d| d.dialog.clone());
                if let Some(d) = dialog {
                    d.close();
                }
                w.connect_to(dev.address, dev.name, dev.guess);
            }
        )));
    }

    fn setup_actions(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let prefs = gio::SimpleAction::new("preferences", None);
        prefs.connect_activate(glib::clone!(
            #[strong]
            weak,
            move |_, _| {
                if let Some(w) = weak.upgrade() {
                    super::prefs::show(&w);
                }
            }
        ));
        self.app.add_action(&prefs);

        let firmware = gio::SimpleAction::new("firmware", None);
        firmware.connect_activate(glib::clone!(
            #[strong]
            weak,
            move |_, _| {
                if let Some(w) = weak.upgrade() {
                    super::firmware::show(&w);
                }
            }
        ));
        self.app.add_action(&firmware);

        let about = gio::SimpleAction::new("about", None);
        about.connect_activate(glib::clone!(
            #[strong]
            weak,
            move |_, _| {
                if let Some(w) = weak.upgrade() {
                    w.show_about();
                }
            }
        ));
        self.app.add_action(&about);

        let quit = gio::SimpleAction::new("quit", None);
        quit.connect_activate(glib::clone!(
            #[strong]
            weak,
            move |_, _| {
                if let Some(w) = weak.upgrade() {
                    w.window.close();
                }
            }
        ));
        self.app.add_action(&quit);
        self.app.set_accels_for_action("app.preferences", &["<Control>comma"]);
        self.app.set_accels_for_action("app.quit", &["<Control>q"]);
        self.app.set_accels_for_action("window.close", &["<Control>w"]);
    }

    pub fn present(&self) {
        self.window.present();
    }

    /// Finishes a running recording before the application exits.
    pub fn shutdown(&self) {
        self.stop_recording();
        self.config.borrow().save();
    }

    pub fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::builder().title(glib::markup_escape_text(text)).timeout(4).build());
    }

    // ------------------------------------------------------------ events

    fn handle(self: &Rc<Self>, ev: Event) {
        match ev {
            Event::AdapterReady { name, powered } => {
                log::info!("Using Bluetooth adapter {name} (powered: {powered})");
                self.set_adapter(if powered { AdapterState::On } else { AdapterState::Off }, None);
                if powered {
                    self.on_adapter_on();
                }
            }
            Event::AdapterUnavailable(reason) => self.set_adapter(AdapterState::Unavailable, Some(reason)),
            Event::AdapterPowered(p) => {
                self.set_adapter(if p { AdapterState::On } else { AdapterState::Off }, None);
                if p {
                    self.on_adapter_on();
                }
            }
            Event::Scanning(s) => {
                self.devices.set_scanning(s);
                if let Some(d) = self.add_dialog.borrow().as_ref() {
                    d.page.set_scanning(s);
                }
            }
            Event::DeviceSeen(d) => self.add_device(d),
            Event::DeviceGone(a) => {
                self.devices.remove(a);
                if let Some(d) = self.add_dialog.borrow().as_ref() {
                    d.page.remove(a);
                }
            }
            Event::Conn { address, token, ev } => {
                // Events of connections that were given up are stale.
                if self.conns.borrow().get(&address).is_some_and(|c| c.token == token) {
                    self.on_conn_event(address, ev);
                }
            }
            Event::Error(e) => self.toast(&e),
        }
    }

    fn on_conn_event(self: &Rc<Self>, address: DeviceId, ev: ConnEvent) {
        match ev {
            ConnEvent::Connecting { attempt } => {
                // A lost connection stays "lost" (with its banner) until it is back.
                if let Some(c) = self.conns.borrow_mut().get_mut(&address) {
                    if !matches!(c.link, Link::Lost(_)) {
                        c.link = Link::Connecting { attempt };
                    }
                }
                self.refresh_chrome();
            }
            ConnEvent::Connected { name, kind } => self.on_connected(address, name, kind),
            ConnEvent::Disconnected { by_user, error, retrying } => {
                if by_user {
                    return;
                }
                if retrying {
                    if let Some(c) = self.conns.borrow_mut().get_mut(&address) {
                        c.link = Link::Lost(error.unwrap_or_else(|| "Connection lost".into()));
                    }
                    self.refresh_chrome();
                    return;
                }
                let removed = self.conns.borrow_mut().remove(&address);
                let label = removed.map(|c| c.label(address)).unwrap_or_default();
                self.toast(&format!("{label}: {}", error.unwrap_or_else(|| "Disconnected".into())));
                self.after_removal();
            }
            ConnEvent::Rssi(r) => match self.kind_of(address) {
                Some(Kind::Meter) => self.device.set_meter_rssi(r),
                _ => self.device.set_rssi(r),
            },
            ConnEvent::Renamed(name) => {
                let kind = self.kind_of(address);
                if let Some(c) = self.conns.borrow_mut().get_mut(&address) {
                    c.name = Some(name.clone());
                }
                match kind {
                    Some(Kind::Meter) => {
                        self.device.set_meter_name(&name);
                        self.toast("Name saved on the multimeter");
                    }
                    _ => {
                        self.device.set_name(&name);
                        self.toast("Name saved. The tester's Bluetooth restarts now.");
                    }
                }
                if let Some(kind) = kind {
                    if !ble::is_demo(address) {
                        let mut c = self.config.borrow_mut();
                        c.set_last(kind, Some(address.to_string()), Some(name));
                        c.save();
                    }
                }
                self.refresh_chrome();
            }
            ConnEvent::Error(e) => {
                let label = self.conns.borrow().get(&address).map(|c| c.label(address)).unwrap_or_default();
                self.toast(&format!("{label}: {e}"));
            }
            ConnEvent::Tester(ev) => match ev {
                TesterEvent::Identified(m) => self.device.set_model(m),
                TesterEvent::Info(i) => self.device.set_info(&i),
                TesterEvent::BleName(n) => self.device.set_name(&n),
                TesterEvent::Data(block) => self.on_data(block),
                TesterEvent::Features(f) => {
                    self.device.set_features(f);
                    self.live.set_features(f);
                }
                TesterEvent::Details(d) => self.device.set_details(&d),
            },
            ConnEvent::Meter(ev) => match ev {
                MeterEvent::Info(i) => {
                    self.device.set_meter_info(&i);
                    self.live.set_meter_info(&i);
                    *self.meter_info.borrow_mut() = Some(i);
                }
                MeterEvent::Reading { at, reading } => self.on_meter_reading(reading, at),
                MeterEvent::LogProgress { received, total } => self.device.set_log_progress(Some((received, total))),
                MeterEvent::Log(log) => {
                    self.device.set_log_progress(None);
                    self.save_meter_log(log);
                }
                MeterEvent::LogFailed(e) => {
                    self.device.set_log_progress(None);
                    self.toast(&format!("Download failed: {e}"));
                }
            },
        }
    }

    fn on_connected(self: &Rc<Self>, address: DeviceId, name: Option<String>, kind: Kind) {
        let (first, label) = {
            let mut conns = self.conns.borrow_mut();
            let Some(c) = conns.get_mut(&address) else { return };
            c.link = Link::Connected;
            if name.is_some() {
                c.name = name;
            }
            c.kind = Some(kind);
            let first = !c.confirmed;
            c.confirmed = true;
            (first, c.name.clone())
        };
        if first {
            // One device per kind: the new one replaces the old one.
            let others: Vec<DeviceId> = self
                .conns
                .borrow()
                .iter()
                .filter(|(a, c)| **a != address && c.kind == Some(kind))
                .map(|(a, _)| *a)
                .collect();
            for a in others {
                self.conns.borrow_mut().remove(&a);
                self.ble.send(Cmd::Disconnect(a));
            }
            let same_source = self.sources.borrow().get(&kind) == Some(&address);
            if same_source {
                self.session.borrow_mut().mark_gap(kind);
            } else {
                self.session.borrow_mut().reset(kind);
                self.sources.borrow_mut().insert(kind, address);
                match kind {
                    Kind::Tester => {
                        self.alarms.borrow_mut().clear();
                        self.autostop.borrow_mut().reset();
                        self.live.clear();
                        self.device.clear();
                    }
                    Kind::Meter => {
                        *self.meter_info.borrow_mut() = None;
                        self.live.update_meter(None);
                        self.device.clear_meter();
                    }
                }
                self.graph.chart.redraw();
            }
            match kind {
                Kind::Tester => {
                    self.device.set_connection(address);
                    self.live.set_features(ble::Features::ALL);
                }
                Kind::Meter => {
                    self.device.set_meter_address(&address.to_string());
                    if let Some(n) = &label {
                        self.device.set_meter_name(n);
                    }
                }
            }
        } else {
            self.session.borrow_mut().mark_gap(kind);
        }
        if !ble::is_demo(address) {
            let mut c = self.config.borrow_mut();
            c.set_last(kind, Some(address.to_string()), label);
            c.save();
        }
        self.refresh_chrome();
    }

    /// Tidies up after a device was removed from `conns`.
    fn after_removal(self: &Rc<Self>) {
        if self.conns.borrow().is_empty() {
            self.leave_workspace();
        } else {
            self.refresh_chrome();
        }
    }

    fn set_adapter(&self, state: AdapterState, reason: Option<String>) {
        self.devices.set_adapter(state, reason.clone());
        if let Some(d) = self.add_dialog.borrow().as_ref() {
            d.page.set_adapter(state, reason);
        }
    }

    fn on_adapter_on(self: &Rc<Self>) {
        if self.started.replace(true) {
            if self.nav.visible_page().as_ref() != Some(&self.workspace) {
                self.start_scan();
            }
            return;
        }
        if self.demo {
            return;
        }
        let remembered: Vec<(DeviceId, Option<String>, Kind)> =
            self.remembered().into_iter().filter(|(a, ..)| !a.is_usb()).collect();
        if remembered.is_empty() {
            self.start_scan();
        }
        for (addr, name, kind) in remembered {
            let guess = if kind == Kind::Meter { Guess::Meter } else { Guess::Tester };
            self.connect_to(addr, name, guess);
        }
    }

    /// Devices to reconnect to on start.
    fn remembered(&self) -> Vec<(DeviceId, Option<String>, Kind)> {
        let c = self.config.borrow();
        if !c.auto_connect {
            return Vec::new();
        }
        [Kind::Tester, Kind::Meter]
            .into_iter()
            .filter_map(|kind| {
                let (addr, name) = c.last(kind);
                Some((addr?.parse::<DeviceId>().ok()?, name.cloned(), kind))
            })
            .collect()
    }

    fn start_scan(self: &Rc<Self>) {
        if self.demo {
            self.add_demo_devices();
        }
        self.ble.send(Cmd::StartScan);
    }

    /// The kind a device is known or remembered to be.
    fn hint_for(&self, address: DeviceId, guess: Guess) -> Guess {
        let a = address.to_string();
        let c = self.config.borrow();
        if c.last_meter.as_deref() == Some(&a) {
            Guess::Meter
        } else if c.last_device.as_deref() == Some(&a) {
            Guess::Tester
        } else {
            guess
        }
    }

    fn add_device(self: &Rc<Self>, mut d: SeenDevice) {
        d.guess = self.hint_for(d.address, d.guess);
        if let Some(dlg) = self.add_dialog.borrow().as_ref() {
            dlg.page.upsert(d.clone());
        }
        self.devices.upsert(d);
    }

    fn add_demo_devices(self: &Rc<Self>) {
        for (address, name, guess) in demo_devices() {
            self.add_device(SeenDevice { address, name: Some(format!("{name} (simulated)")), rssi: Some(-50), guess });
        }
    }

    fn kind_of(&self, address: DeviceId) -> Option<Kind> {
        self.conns.borrow().get(&address).and_then(|c| c.kind)
    }

    /// The connected (or connecting) device of a kind.
    fn address_of(&self, kind: Kind) -> Option<DeviceId> {
        let conns = self.conns.borrow();
        conns.iter().filter(|(_, c)| c.kind == Some(kind)).max_by_key(|(_, c)| c.confirmed).map(|(a, _)| *a)
    }

    pub fn connect_to(self: &Rc<Self>, address: DeviceId, name: Option<String>, guess: Guess) {
        if self.nav.visible_page().as_ref() != Some(&self.workspace) {
            self.nav.push(&self.workspace);
        }
        if self.conns.borrow().contains_key(&address) {
            return;
        }
        // Coming back to the same device continues the session; anything
        // else starts afresh.
        let fresh = self.conns.borrow().is_empty() && !self.sources.borrow().values().any(|a| *a == address);
        if fresh {
            self.stop_recording();
            let hist = self.config.borrow().history_minutes as f64 * 60.0;
            *self.session.borrow_mut() = Session::new(hist);
            self.sources.borrow_mut().clear();
            self.alarms.borrow_mut().clear();
            self.autostop.borrow_mut().reset();
            *self.meter_info.borrow_mut() = None;
            self.live.clear();
            self.live.update_meter(None);
            self.device.clear();
            self.device.clear_meter();
            let s = self.session.borrow();
            self.live.update_stats(&s);
            self.sess.update(&s);
            drop(s);
            self.graph.chart.redraw();
        }
        let token = self.next_token.get();
        self.next_token.set(token.wrapping_add(1));
        let kind = self.hint_for(address, guess).kind();
        self.conns
            .borrow_mut()
            .insert(address, Conn { token, name, kind, confirmed: false, link: Link::Connecting { attempt: 1 } });
        self.ble.send(Cmd::Connect { address, token });
        self.refresh_chrome();
    }

    /// Disconnects one device; the last one also leaves the workspace.
    fn disconnect_device(self: &Rc<Self>, address: DeviceId) {
        if self.conns.borrow().len() <= 1 {
            self.leave_workspace();
            return;
        }
        let removed = self.conns.borrow_mut().remove(&address);
        self.ble.send(Cmd::Disconnect(address));
        // Don't reconnect to it on the next start either.
        if let Some(kind) = removed.and_then(|c| c.kind) {
            let mut c = self.config.borrow_mut();
            if c.last(kind).0.map(String::as_str) == Some(address.to_string().as_str()) {
                c.set_last(kind, None, None);
                c.save();
            }
        }
        self.refresh_chrome();
    }

    fn leave_workspace(self: &Rc<Self>) {
        if self.nav.visible_page().as_ref() == Some(&self.workspace) {
            // The popped handler disconnects.
            self.nav.pop();
        } else {
            self.disconnect_all();
        }
    }

    fn disconnect_all(self: &Rc<Self>) {
        self.stop_recording();
        self.ble.send(Cmd::DisconnectAll);
        self.conns.borrow_mut().clear();
        self.refresh_chrome();
        self.devices.clear();
        if self.devices.adapter() == AdapterState::On {
            self.start_scan();
        }
    }

    /// Updates the title, status, banner, inhibitor and the visible parts
    /// of the pages from the connections.
    fn refresh_chrome(&self) {
        let conns = self.conns.borrow();
        let order = |c: &Conn| match c.kind {
            Some(Kind::Tester) => 0,
            Some(Kind::Meter) => 1,
            None => 2,
        };
        let mut list: Vec<(&DeviceId, &Conn)> = conns.iter().collect();
        list.sort_by_key(|(_, c)| order(c));

        let names: Vec<String> = list.iter().map(|(a, c)| c.label(**a)).collect();
        let title = if names.is_empty() { "Křižík".to_string() } else { names.join(" + ") };
        self.title.set_title(&title);
        self.workspace.set_title(&title);

        let state = |c: &Conn| match &c.link {
            Link::Connecting { attempt } if *attempt > 1 => "Reconnecting…",
            Link::Connecting { .. } => "Connecting…",
            Link::Connected => "Connected",
            Link::Lost(_) => "Reconnecting…",
        };
        let status = if list.iter().all(|(_, c)| c.link == Link::Connected) {
            if list.is_empty() { "" } else { "Connected" }.to_string()
        } else if list.len() == 1 {
            state(list[0].1).to_string()
        } else {
            list.iter()
                .filter(|(_, c)| c.link != Link::Connected)
                .map(|(a, c)| format!("{}: {}", c.label(**a), state(c)))
                .collect::<Vec<_>>()
                .join(" · ")
        };
        self.title.set_subtitle(&status);
        self.status_label.set_label(&if title == "Křižík" || status.is_empty() {
            status.clone()
        } else {
            format!("{title} · {status}")
        });

        let lost = list.iter().find_map(|(a, c)| match &c.link {
            Link::Lost(msg) => Some((**a, c.label(**a), msg.clone())),
            _ => None,
        });
        match lost {
            Some((addr, label, msg)) => {
                let text = if list.len() > 1 {
                    format!("{label}: {msg} — reconnecting…")
                } else {
                    format!("{msg} — reconnecting…")
                };
                self.banner.set_title(&glib::markup_escape_text(&text));
                self.banner.set_revealed(true);
                self.banner_target.set(Some(addr));
            }
            None => {
                self.banner.set_revealed(false);
                self.banner_target.set(None);
            }
        }

        let has = |k: Kind| conns.values().any(|c| c.kind == Some(k));
        let (tester, meter) = (has(Kind::Tester), has(Kind::Meter));
        self.live.set_parts(tester, meter);
        self.device.set_parts(tester, meter);
        self.sess.set_parts(tester, meter);
        if meter {
            if let Some(i) = self.meter_info.borrow().as_ref() {
                self.device.set_meter_info(i);
            }
        }
        drop(conns);
        if let Some(d) = self.add_dialog.borrow().as_ref() {
            d.page.set_excluded(self.conns.borrow().keys().copied().collect());
        }
        self.update_inhibit();
    }

    fn on_data(self: &Rc<Self>, block: protocol::RealtimeBlock) {
        let samples = self.session.borrow_mut().push_block(&block, Instant::now());

        let write_err = match self.recorder.borrow_mut().as_mut() {
            Some(r) => r.write(&self.session.borrow(), &samples).err(),
            None => None,
        };
        if let Some(e) = write_err {
            self.toast(&format!("Recording stopped: {e}"));
            self.stop_recording();
        }

        let limits = self.config.borrow().alarms.clone();
        let fired: Vec<(AlarmKind, f32)> =
            samples.iter().flat_map(|s| self.alarms.borrow_mut().check(&limits, s)).collect();
        for (kind, value) in fired {
            self.raise_alarm(kind, value);
        }

        if self.session.borrow().accumulating {
            let cfg = self.config.borrow().autostop;
            for s in &samples {
                let reason = {
                    let sess = self.session.borrow();
                    self.autostop.borrow_mut().check(&cfg, &sess.stats, s)
                };
                if let Some(r) = reason {
                    self.session.borrow_mut().accumulating = false;
                    let st = self.session.borrow().stats.clone();
                    let body = format!(
                        "Counting stopped: {}. {} · {}",
                        r.describe(),
                        format::capacity(st.amp_hours),
                        format::energy(st.watt_hours)
                    );
                    self.toast(&body);
                    self.notify("autostop", "Measurement finished", &body);
                    break;
                }
            }
        }

        let sess = self.session.borrow();
        self.live.update(&sess);
        self.sess.update(&sess);
        drop(sess);
        self.device.update(&block);
        self.after_data();
    }

    fn on_meter_reading(self: &Rc<Self>, reading: owon::Reading, at: Instant) {
        let point = self.session.borrow_mut().push_meter(reading, at);
        let write_err = match self.recorder.borrow_mut().as_mut() {
            Some(r) => r.write_meter(&self.session.borrow(), &point).err(),
            None => None,
        };
        if let Some(e) = write_err {
            self.toast(&format!("Recording stopped: {e}"));
            self.stop_recording();
        }
        self.live.update_meter(Some(&point));
        self.sess.update(&self.session.borrow());
        self.after_data();
    }

    fn after_data(&self) {
        if self.stack.visible_child_name().as_deref() == Some("graph") {
            self.graph.chart.redraw();
        }
        if let Some(r) = self.recorder.borrow().as_ref() {
            self.sess.set_recording(Some(&r.path.display().to_string()), r.rows);
        }
    }

    fn raise_alarm(&self, kind: AlarmKind, value: f32) {
        let text = format!("{}: {:.3} {}", kind.title(), value, kind.unit());
        self.toast(&text);
        let (sound, notify) = {
            let c = self.config.borrow();
            (c.alarm_sound, c.alarm_notify)
        };
        if sound && self.last_beep.get().is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            self.last_beep.set(Some(Instant::now()));
            WidgetExt::display(&self.window).beep();
        }
        if notify {
            self.notify(&format!("alarm-{}", kind.key()), "Křižík Alarm", &text);
        }
    }

    fn notify(&self, id: &str, title: &str, body: &str) {
        let n = gio::Notification::new(title);
        n.set_body(Some(body));
        n.set_priority(gio::NotificationPriority::High);
        self.app.send_notification(Some(id), &n);
    }

    pub fn update_inhibit(&self) {
        let connected = self.conns.borrow().values().any(|c| c.link == Link::Connected);
        let want = connected && self.config.borrow().keep_awake;
        match (want, self.inhibit_cookie.get()) {
            (true, None) => {
                let cookie = self.app.inhibit(
                    Some(&self.window),
                    gtk::ApplicationInhibitFlags::IDLE | gtk::ApplicationInhibitFlags::SUSPEND,
                    Some("Monitoring measurements"),
                );
                if cookie != 0 {
                    self.inhibit_cookie.set(Some(cookie));
                }
            }
            (false, Some(cookie)) => {
                self.app.uninhibit(cookie);
                self.inhibit_cookie.set(None);
            }
            _ => {}
        }
    }

    /// Applies settings changed in the preferences dialog.
    pub fn settings_changed(&self) {
        let c = self.config.borrow().clone();
        c.save();
        self.session.borrow_mut().set_max_history_secs(c.history_minutes as f64 * 60.0);
        self.devices.set_show_all(c.show_all_devices);
        self.update_inhibit();
    }

    // ------------------------------------------------------ user actions

    fn show_add_device(self: &Rc<Self>) {
        if self.add_dialog.borrow().is_some() {
            return;
        }
        let page = DevicesPage::new(None, self.config.borrow().show_all_devices);
        page.set_adapter(self.devices.adapter(), None);
        page.set_excluded(self.conns.borrow().keys().copied().collect::<HashSet<_>>());
        for d in self.devices.devices() {
            page.upsert(d);
        }
        self.wire_devices_page(&page);
        let weak = Rc::downgrade(self);
        page.scan_button.connect_clicked(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(w) = weak.upgrade() {
                    w.start_scan();
                }
            }
        ));
        let nav = adw::NavigationView::new();
        nav.add(&page.page);
        let dialog =
            adw::Dialog::builder().title("Add Device").content_width(440).content_height(560).child(&nav).build();
        dialog.connect_closed(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(w) = weak.upgrade() {
                    w.add_dialog.borrow_mut().take();
                    w.ble.send(Cmd::StopScan);
                }
            }
        ));
        *self.add_dialog.borrow_mut() = Some(AddDialog { dialog: dialog.clone(), page });
        dialog.present(Some(&self.window));
        self.start_scan();
    }

    fn rename(self: &Rc<Self>, kind: Kind, name: String) {
        let Some(address) = self.address_of(kind) else { return };
        let (check, heading, body) = match kind {
            Kind::Tester => (
                protocol::validate_name(&name).map_err(|e| e.to_string()),
                "Rename Tester?",
                format!(
                    "The tester will be called “{name}”. Its Bluetooth restarts and Křižík reconnects automatically."
                ),
            ),
            Kind::Meter => (
                owon::validate_name(&name).map_err(|e| e.to_string()),
                "Rename Multimeter?",
                format!("The multimeter will be called “{name}”."),
            ),
        };
        if let Err(e) = check {
            self.toast(&e);
            return;
        }
        let dialog = adw::AlertDialog::new(Some(heading), Some(&body));
        dialog.add_responses(&[("cancel", "_Cancel"), ("rename", "_Rename")]);
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");
        let w = self.clone();
        glib::spawn_future_local(async move {
            if dialog.choose_future(Some(&w.window)).await == "rename" {
                w.ble.send(Cmd::Rename(address, name));
            }
        });
    }

    fn meter_command(&self, cmd: MeterCmd) {
        match self.address_of(Kind::Meter) {
            Some(a) => self.ble.send(Cmd::Meter(a, cmd)),
            None => self.toast("The multimeter is not connected"),
        }
    }

    fn start_meter_recording(self: &Rc<Self>) {
        let interval = adw::SpinRow::with_range(1.0, owon::MAX_RECORD_INTERVAL as f64, 1.0);
        interval.set_title("Interval");
        interval.set_subtitle("Seconds between readings");
        interval.set_value(1.0);
        let count = adw::SpinRow::with_range(1.0, owon::MAX_RECORD_COUNT as f64, 10.0);
        count.set_title("Readings");
        count.set_subtitle(&format!("At most {}", owon::MAX_RECORD_COUNT));
        count.set_value(100.0);
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
        list.append(&interval);
        list.append(&count);
        let duration = gtk::Label::builder().wrap(true).css_classes(["dim-label", "caption"]).margin_top(6).build();
        let update_duration = glib::clone!(
            #[weak]
            interval,
            #[weak]
            count,
            #[weak]
            duration,
            move || {
                let secs = interval.value() * (count.value() - 1.0).max(0.0);
                duration.set_label(&format!("Takes about {}", format::duration(secs)));
            }
        );
        update_duration();
        let u = update_duration.clone();
        interval.connect_value_notify(move |_| u());
        count.connect_value_notify(move |_| update_duration());
        let extra = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        extra.append(&list);
        extra.append(&duration);

        let dialog = adw::AlertDialog::new(
            Some("Start Recording on the Multimeter?"),
            Some("The multimeter records by itself and switches its Bluetooth off until it has finished; Křižík disconnects from it. Readings recorded before are replaced."),
        );
        dialog.set_extra_child(Some(&extra));
        dialog.add_responses(&[("cancel", "_Cancel"), ("start", "_Start")]);
        dialog.set_response_appearance("start", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("start"));
        dialog.set_close_response("cancel");
        let w = self.clone();
        glib::spawn_future_local(async move {
            if dialog.choose_future(Some(&w.window)).await == "start" {
                let cmd = MeterCmd::StartRecording { interval_s: interval.value() as u32, count: count.value() as u32 };
                w.meter_command(cmd);
            }
        });
    }

    fn download_meter_log(self: &Rc<Self>) {
        let recording = self.meter_info.borrow().as_ref().is_some_and(|i| i.log == LogState::Recording);
        if !recording {
            self.meter_command(MeterCmd::DownloadLog);
            return;
        }
        let dialog = adw::AlertDialog::new(
            Some("Stop Recording?"),
            Some("The multimeter is still recording. Stop the recording to download the readings recorded so far."),
        );
        dialog.add_responses(&[("cancel", "_Cancel"), ("stop", "_Stop and Download")]);
        dialog.set_response_appearance("stop", adw::ResponseAppearance::Suggested);
        dialog.set_close_response("cancel");
        let w = self.clone();
        glib::spawn_future_local(async move {
            if dialog.choose_future(Some(&w.window)).await == "stop" {
                w.meter_command(MeterCmd::StopRecording);
                w.meter_command(MeterCmd::DownloadLog);
            }
        });
    }

    fn save_meter_log(self: &Rc<Self>, log: Arc<owon::Log>) {
        if log.records.is_empty() {
            self.toast("The multimeter has no recorded readings");
            return;
        }
        let dialog = gtk::FileDialog::builder()
            .title("Save Multimeter Readings")
            .initial_name(export::default_file_name("krizik-multimeter-log"))
            .modal(true)
            .build();
        let w = self.clone();
        glib::spawn_future_local(async move {
            let Ok(file) = dialog.save_future(Some(&w.window)).await else { return };
            let Some(path) = file.path() else {
                w.toast("Please choose a local file");
                return;
            };
            let result = std::fs::File::create(&path).and_then(|f| {
                let mut out = std::io::BufWriter::new(f);
                log.write_csv(&mut out)?;
                std::io::Write::flush(&mut out)
            });
            match result {
                Ok(()) => w.toast(&format!("Saved {} readings", log.records.len())),
                Err(e) => w.toast(&format!("Could not save the readings: {e}")),
            }
        });
    }

    fn stop_recording(&self) {
        let Some(r) = self.recorder.borrow_mut().take() else { return };
        match r.finish() {
            Ok((path, rows)) => {
                let file = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                self.toast(&format!("Saved {rows} rows to {file}"));
            }
            Err(e) => self.toast(&format!("Could not finish the recording: {e}")),
        }
        self.sess.set_recording(None, 0);
        self.rec_indicator.set_visible(false);
    }

    fn toggle_recording(self: &Rc<Self>) {
        if self.recorder.borrow().is_some() {
            self.stop_recording();
            return;
        }
        let dialog = gtk::FileDialog::builder()
            .title("Record to CSV File")
            .initial_name(export::default_file_name("krizik-recording"))
            .modal(true)
            .build();
        let w = self.clone();
        glib::spawn_future_local(async move {
            let file = match dialog.save_future(Some(&w.window)).await {
                Ok(f) => f,
                Err(e) => {
                    if !e.matches(gtk::DialogError::Dismissed) && !e.matches(gtk::DialogError::Cancelled) {
                        w.toast(&format!("Could not choose a file: {e}"));
                    }
                    return;
                }
            };
            let Some(path) = file.path() else {
                w.toast("Please choose a local file");
                return;
            };
            match Recorder::create(&path) {
                Ok(r) => {
                    w.sess.set_recording(Some(&path.display().to_string()), 0);
                    *w.recorder.borrow_mut() = Some(r);
                    w.rec_indicator.set_visible(true);
                    w.toast("Recording started");
                }
                Err(e) => w.toast(&format!("Could not create the file: {e}")),
            }
        });
    }

    fn export_history(self: &Rc<Self>) {
        if self.session.borrow().is_empty() {
            self.toast("There is no data to export yet");
            return;
        }
        let dialog = gtk::FileDialog::builder()
            .title("Export Graph Data")
            .initial_name(export::default_file_name("krizik-export"))
            .modal(true)
            .build();
        let w = self.clone();
        glib::spawn_future_local(async move {
            let Ok(file) = dialog.save_future(Some(&w.window)).await else { return };
            let Some(path) = file.path() else {
                w.toast("Please choose a local file");
                return;
            };
            match export::export_history(&path, &w.session.borrow()) {
                Ok(n) => w.toast(&format!("Exported {n} rows")),
                Err(e) => w.toast(&format!("Export failed: {e}")),
            }
        });
    }

    fn show_about(&self) {
        let about = adw::AboutDialog::builder()
            .application_name("Křižík")
            .application_icon(APP_ID)
            .version(env!("CARGO_PKG_VERSION"))
            .developer_name("Křižík contributors")
            .license(
                "This program comes with absolutely no warranty. See the <a href=\"https://interoperable-europe.ec.europa.eu/collection/eupl/eupl-text-eupl-12\">European Union Public Licence, version 1.2</a> for details.",
            )
            .comments(
                "Monitor USB testers and multimeters over Bluetooth and USB.\n\nNamed after František Křižík (1847–1941), the Czech electrical engineer and inventor.\n\nIndependent project, not affiliated with ALIENTEK, OWON or FNIRSI.",
            )
            .build();
        about.present(Some(&self.window));
    }

    // -------------------------------------------------------- debugging

    /// `KRIZIK_SCREENSHOT=file.png [KRIZIK_SCREENSHOT_PAGE=graph]
    /// [KRIZIK_SCREENSHOT_DELAY=ms]` renders the window to a PNG and quits.
    /// Pages: live, graph, session, device, firmware, add-device.
    fn maybe_screenshot(self: &Rc<Self>) {
        let Ok(path) = std::env::var("KRIZIK_SCREENSHOT") else { return };
        let delay = std::env::var("KRIZIK_SCREENSHOT_DELAY").ok().and_then(|d| d.parse().ok()).unwrap_or(4000);
        if let Ok(page) = std::env::var("KRIZIK_SCREENSHOT_PAGE") {
            let w = self.clone();
            glib::timeout_add_local_once(Duration::from_millis(500), move || match page.as_str() {
                "firmware" => gio::prelude::ActionGroupExt::activate_action(&w.app, "firmware", None),
                "add-device" => w.show_add_device(),
                _ => w.stack.set_visible_child_name(&page),
            });
        }
        let w = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(delay), move || {
            let paintable = gtk::WidgetPaintable::new(Some(&w.window));
            let snapshot = gtk::Snapshot::new();
            paintable.snapshot(&snapshot, w.window.width() as f64, w.window.height() as f64);
            match (snapshot.to_node(), w.window.renderer()) {
                (Some(node), Some(renderer)) => {
                    let texture = renderer.render_texture(&node, None);
                    if let Err(e) = texture.save_to_png(&path) {
                        log::error!("Saving screenshot failed: {e}");
                    }
                }
                (node, renderer) => log::error!(
                    "Nothing to render (mapped: {}, visible: {}, node: {}, renderer: {}, size {}x{})",
                    w.window.is_mapped(),
                    w.window.is_visible(),
                    node.is_some(),
                    renderer.is_some(),
                    w.window.width(),
                    w.window.height()
                ),
            }
            w.window.close();
        });
    }
}

/// The simulated devices of `--demo`; `KRIZIK_DEMO=tester|meter|both|usb`
/// (`usb`: a USB tester and the multimeter).
fn demo_devices() -> Vec<(DeviceId, &'static str, Guess)> {
    let which = std::env::var("KRIZIK_DEMO").unwrap_or_default();
    let mut out = Vec::new();
    if which == "usb" {
        out.push((ble::DEMO_USB_ADDRESS, "FNIRSI FNB58", Guess::Tester));
    } else if which != "meter" {
        out.push((ble::DEMO_ADDRESS, "C2_DEMO", Guess::Tester));
    }
    if which != "tester" {
        out.push((ble::DEMO_METER_ADDRESS, "BDM_DEMO", Guess::Meter));
    }
    out
}
