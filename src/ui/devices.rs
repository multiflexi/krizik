//! Start page: Bluetooth scan results.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::ble::{DeviceId, Guess, SeenDevice};

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum AdapterState {
    Unknown,
    Unavailable,
    Off,
    On,
}

type StatusCallback = Box<dyn Fn(AdapterState)>;
type ActivateCallback = Rc<RefCell<Option<Box<dyn Fn(SeenDevice)>>>>;

struct Entry {
    row: adw::ActionRow,
    icon: gtk::Image,
    signal: gtk::Image,
    dbm: gtk::Label,
    dev: SeenDevice,
}

pub struct DevicesPage {
    pub page: adw::NavigationPage,
    stack: gtk::Stack,
    status: adw::StatusPage,
    status_button: gtk::Button,
    list: gtk::ListBox,
    pub scan_button: gtk::Button,
    spinner: gtk::Spinner,
    entries: Rc<RefCell<HashMap<DeviceId, Entry>>>,
    show_all: Rc<Cell<bool>>,
    /// Devices not to list (already connected).
    excluded: Rc<RefCell<HashSet<DeviceId>>>,
    scanning: Cell<bool>,
    adapter: Cell<AdapterState>,
    unavailable_reason: RefCell<String>,
    pub on_status_button: RefCell<Option<StatusCallback>>,
    /// Called when a device is chosen.
    pub on_activate: ActivateCallback,
}

fn guess_icon(g: Guess) -> &'static str {
    match g {
        Guess::Tester => "krizik-tester-symbolic",
        Guess::Meter => "krizik-multimeter-symbolic",
        Guess::Unknown | Guess::Other => "bluetooth-active-symbolic",
    }
}

fn guess_subtitle(d: &SeenDevice) -> String {
    if let DeviceId::Usb(p) = d.address {
        let what = match d.guess {
            Guess::Meter => "Multimeter",
            _ => "USB tester",
        };
        return format!("{what} · USB port {}", p.location());
    }
    match d.guess {
        Guess::Tester => format!("USB tester · {}", d.address),
        Guess::Meter => format!("Multimeter · {}", d.address),
        Guess::Unknown => format!("Tester or multimeter · {}", d.address),
        Guess::Other => d.address.to_string(),
    }
}

/// Sort rank: known devices first, then possible ones.
fn guess_rank(g: Guess) -> u8 {
    match g {
        Guess::Tester | Guess::Meter => 0,
        Guess::Unknown => 1,
        Guess::Other => 2,
    }
}

fn signal_icon(rssi: Option<i16>) -> &'static str {
    match rssi {
        None => "network-cellular-signal-none-symbolic",
        Some(r) if r >= -60 => "network-cellular-signal-excellent-symbolic",
        Some(r) if r >= -70 => "network-cellular-signal-good-symbolic",
        Some(r) if r >= -80 => "network-cellular-signal-ok-symbolic",
        Some(_) => "network-cellular-signal-weak-symbolic",
    }
}

impl DevicesPage {
    /// The start page has the main menu; the "Add Device" dialog has none.
    pub fn new(menu: Option<&gio::Menu>, show_all: bool) -> Rc<DevicesPage> {
        let header = adw::HeaderBar::new();
        let spinner = gtk::Spinner::builder().visible(false).build();
        let scan_button =
            gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Search Again").build();
        header.pack_start(&scan_button);
        header.pack_start(&spinner);
        if let Some(menu) = menu {
            let menu_button = gtk::MenuButton::builder()
                .icon_name("open-menu-symbolic")
                .menu_model(menu)
                .tooltip_text("Main Menu")
                .primary(true)
                .build();
            header.pack_end(&menu_button);
        }

        let status_button = gtk::Button::builder()
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .visible(false)
            .build();
        let status = adw::StatusPage::builder().icon_name("bluetooth-active-symbolic").child(&status_button).build();

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .valign(gtk::Align::Start)
            .build();
        let hint = gtk::Label::builder()
            .label("Turn on Bluetooth on the tester in Settings → Others, or on the multimeter. Křižík remembers your tester and multimeter and reconnects to them automatically; both can be connected at the same time.")
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .build();
        let title = gtk::Label::builder().label("Nearby Devices").xalign(0.0).css_classes(["heading"]).build();
        let col = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(18)
            .margin_bottom(18)
            .margin_start(12)
            .margin_end(12)
            .build();
        col.append(&title);
        col.append(&list);
        col.append(&hint);
        let clamp = adw::Clamp::builder().maximum_size(600).child(&col).build();
        let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamp).build();

        let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
        stack.add_named(&status, Some("status"));
        stack.add_named(&scroller, Some("list"));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&stack));
        let title = if menu.is_some() { "Křižík" } else { "Add Device" };
        let page = adw::NavigationPage::builder().title(title).tag("devices").child(&toolbar).build();

        let entries: Rc<RefCell<HashMap<DeviceId, Entry>>> = Rc::default();
        let show_all = Rc::new(Cell::new(show_all));
        let excluded: Rc<RefCell<HashSet<DeviceId>>> = Rc::default();

        // Testers and multimeters first, then by signal strength.
        list.set_sort_func(glib::clone!(
            #[strong]
            entries,
            move |a, b| {
                let e = entries.borrow();
                let key = |r: &gtk::ListBoxRow| {
                    e.values()
                        .find(|x| x.row.upcast_ref::<gtk::ListBoxRow>() == r)
                        .map(|x| (guess_rank(x.dev.guess), -(x.dev.rssi.unwrap_or(-127) as i32), x.dev.address))
                };
                key(a).cmp(&key(b)).into()
            }
        ));
        list.set_filter_func(glib::clone!(
            #[strong]
            entries,
            #[strong]
            show_all,
            #[strong]
            excluded,
            move |r| {
                entries.borrow().values().find(|x| x.row.upcast_ref::<gtk::ListBoxRow>() == r).is_some_and(|x| {
                    (show_all.get() || x.dev.guess.is_candidate()) && !excluded.borrow().contains(&x.dev.address)
                })
            }
        ));

        let this = Rc::new(DevicesPage {
            page,
            stack,
            status,
            status_button,
            list,
            scan_button,
            spinner,
            entries,
            show_all,
            excluded,
            scanning: Cell::new(false),
            adapter: Cell::new(AdapterState::Unknown),
            unavailable_reason: RefCell::new(String::new()),
            on_status_button: RefCell::new(None),
            on_activate: Rc::default(),
        });
        this.status_button.connect_clicked(glib::clone!(
            #[weak]
            this,
            move |_| {
                if let Some(cb) = this.on_status_button.borrow().as_ref() {
                    cb(this.adapter.get());
                }
            }
        ));
        this.refresh_state();
        this
    }

    fn visible_count(&self) -> usize {
        let all = self.show_all.get();
        let excluded = self.excluded.borrow();
        self.entries
            .borrow()
            .values()
            .filter(|e| (all || e.dev.guess.is_candidate()) && !excluded.contains(&e.dev.address))
            .count()
    }

    fn refresh_state(&self) {
        let adapter = self.adapter.get();
        self.scan_button.set_sensitive(adapter == AdapterState::On && !self.scanning.get());
        self.spinner.set_visible(self.scanning.get());
        self.spinner.set_spinning(self.scanning.get());
        self.status_button.set_visible(false);

        // USB devices work without Bluetooth.
        if self.visible_count() > 0 {
            self.stack.set_visible_child_name("list");
            return;
        }
        let (icon, title, desc, button): (&str, &str, String, Option<&str>) = match adapter {
            AdapterState::Unknown => ("bluetooth-acquiring-symbolic", "Starting Bluetooth…", String::new(), None),
            AdapterState::Unavailable => (
                "bluetooth-disabled-symbolic",
                "Bluetooth Unavailable",
                self.unavailable_reason.borrow().clone(),
                Some("Try Again"),
            ),
            AdapterState::Off => (
                "bluetooth-disabled-symbolic",
                "Bluetooth Is Off",
                "Turn on Bluetooth to connect to a tester or multimeter.".into(),
                Some("Turn On"),
            ),
            AdapterState::On if self.visible_count() > 0 => {
                self.stack.set_visible_child_name("list");
                return;
            }
            AdapterState::On if self.scanning.get() => (
                "bluetooth-acquiring-symbolic",
                "Searching for Devices…",
                "Make sure Bluetooth is on in the tester's settings (Settings → Others → Bluetooth) or on the multimeter.".into(),
                None,
            ),
            AdapterState::On => (
                "bluetooth-active-symbolic",
                "No Devices Found",
                "Make sure Bluetooth is on in the tester's settings (Settings → Others → Bluetooth) or on the multimeter.".into(),
                Some("Search Again"),
            ),
        };
        self.status.set_icon_name(Some(icon));
        self.status.set_title(title);
        self.status.set_description(if desc.is_empty() { None } else { Some(&desc) });
        if let Some(label) = button {
            self.status_button.set_label(label);
            self.status_button.set_visible(true);
        }
        self.stack.set_visible_child_name("status");
    }

    pub fn set_adapter(&self, state: AdapterState, reason: Option<String>) {
        if let Some(r) = reason {
            *self.unavailable_reason.borrow_mut() = r;
        }
        self.adapter.set(state);
        if state != AdapterState::On {
            self.scanning.set(false);
        }
        self.refresh_state();
    }

    pub fn adapter(&self) -> AdapterState {
        self.adapter.get()
    }

    pub fn set_scanning(&self, scanning: bool) {
        self.scanning.set(scanning);
        self.refresh_state();
    }

    /// Hides devices that are already connected.
    pub fn set_excluded(&self, addresses: HashSet<DeviceId>) {
        *self.excluded.borrow_mut() = addresses;
        self.list.invalidate_filter();
        self.refresh_state();
    }

    /// Everything seen so far.
    pub fn devices(&self) -> Vec<SeenDevice> {
        self.entries.borrow().values().map(|e| e.dev.clone()).collect()
    }

    pub fn set_show_all(&self, all: bool) {
        self.show_all.set(all);
        self.list.invalidate_filter();
        self.refresh_state();
    }

    // The list box calls the sort and filter functions (which read `entries`)
    // synchronously, so `entries` must never be borrowed while touching it.

    pub fn clear(&self) {
        let rows: Vec<adw::ActionRow> = self.entries.borrow_mut().drain().map(|(_, e)| e.row).collect();
        for row in rows {
            self.list.remove(&row);
        }
        self.refresh_state();
    }

    pub fn remove(&self, addr: DeviceId) {
        let removed = self.entries.borrow_mut().remove(&addr);
        if let Some(e) = removed {
            self.list.remove(&e.row);
        }
        self.refresh_state();
    }

    /// Adds or updates a device row.
    pub fn upsert(&self, dev: SeenDevice) {
        let title = dev.name.clone().unwrap_or_else(|| "Unknown Device".into());
        let updated = {
            let mut entries = self.entries.borrow_mut();
            entries.get_mut(&dev.address).map(|e| {
                e.row.set_title(&glib::markup_escape_text(&title));
                e.row.set_subtitle(&guess_subtitle(&dev));
                e.icon.set_icon_name(Some(guess_icon(dev.guess)));
                e.signal.set_icon_name(Some(signal_icon(dev.rssi)));
                e.dbm.set_label(&dev.rssi.map_or(String::new(), |r| format!("{r} dBm")));
                e.dev = dev.clone();
                e.row.clone()
            })
        };
        if let Some(row) = updated {
            row.changed();
        } else {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&title))
                .subtitle(guess_subtitle(&dev))
                .activatable(true)
                .build();
            let icon = gtk::Image::from_icon_name(guess_icon(dev.guess));
            row.add_prefix(&icon);
            let dbm = gtk::Label::builder()
                .label(dev.rssi.map_or(String::new(), |r| format!("{r} dBm")))
                .css_classes(["dim-label", "caption", "numeric-value"])
                .build();
            let signal = gtk::Image::from_icon_name(signal_icon(dev.rssi));
            row.add_suffix(&dbm);
            row.add_suffix(&signal);
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let addr = dev.address;
            let entries = self.entries.clone();
            let callback = self.on_activate.clone();
            row.connect_activated(move |_| {
                let dev = entries.borrow().get(&addr).map(|e| e.dev.clone());
                if let (Some(dev), Some(cb)) = (dev, callback.borrow().as_ref()) {
                    cb(dev);
                }
            });
            self.entries.borrow_mut().insert(dev.address, Entry { row: row.clone(), icon, signal, dbm, dev });
            self.list.append(&row);
        }
        self.list.invalidate_sort();
        self.list.invalidate_filter();
        self.refresh_state();
    }
}
