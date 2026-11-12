//! "Device" page: information, Bluetooth name and connection of the tester
//! and the multimeter, and the multimeter's offline recording.

use std::cell::Cell;

use adw::prelude::*;

use super::format::DASH;
use super::live::value_row;
use crate::ble::{DeviceId, Features, TesterDetails};
use crate::owon::{self, Info, LogState};
use crate::protocol::{format_firmware_series, format_version, DeviceInfo, Model, RealtimeBlock, MAX_NAME_LEN};

fn action_row(title: &str, subtitle: &str, icon: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).subtitle(subtitle).activatable(true).build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    row
}

fn disconnect_button(label: &str) -> (adw::PreferencesGroup, gtk::Button) {
    let button = gtk::Button::builder()
        .label(label)
        .halign(gtk::Align::Center)
        .css_classes(["pill", "destructive-action"])
        .build();
    let g = adw::PreferencesGroup::new();
    g.add(&button);
    (g, button)
}

fn show_signal(label: &gtk::Label, rssi: i16) {
    label.set_label(&format!("{rssi} dBm"));
    if let Some(row) = label.ancestor(adw::ActionRow::static_type()) {
        row.set_visible(true);
    }
}

fn hide_signal(label: &gtk::Label) {
    label.set_label(DASH);
    if let Some(row) = label.ancestor(adw::ActionRow::static_type()) {
        row.set_visible(false);
    }
}

pub struct DevicePage {
    pub root: adw::PreferencesPage,
    tester_groups: Vec<adw::PreferencesGroup>,
    meter_groups: Vec<adw::PreferencesGroup>,
    model: gtk::Label,
    serial: gtk::Label,
    hardware: gtk::Label,
    firmware: gtk::Label,
    bootloader: gtk::Label,
    address: gtk::Label,
    address_row: adw::ActionRow,
    signal: gtk::Label,
    bt: adw::PreferencesGroup,
    meas: adw::PreferencesGroup,
    power_bank_row: adw::ActionRow,
    temperature_row: adw::ActionRow,
    hardware_row: adw::ActionRow,
    bootloader_row: adw::ActionRow,
    firmware_row: adw::ActionRow,
    serial_row: adw::ActionRow,
    fw: adw::PreferencesGroup,
    pub name_row: adw::EntryRow,
    interval: gtk::Label,
    power_bank: gtk::Label,
    temperature: gtk::Label,
    pub disconnect: gtk::Button,

    meter_model: gtk::Label,
    meter_firmware: gtk::Label,
    meter_battery: gtk::Label,
    meter_address: gtk::Label,
    meter_signal: gtk::Label,
    meter_bt: adw::PreferencesGroup,
    pub meter_name_row: adw::EntryRow,
    log_group: adw::PreferencesGroup,
    log_supported: Cell<bool>,
    log_state: gtk::Label,
    pub log_start: adw::ActionRow,
    pub log_stop: adw::ActionRow,
    pub log_download: adw::ActionRow,
    log_spinner: gtk::Spinner,
    pub meter_disconnect: gtk::Button,
}

impl DevicePage {
    pub fn new() -> DevicePage {
        let root = adw::PreferencesPage::new();

        // ------------------------------------------------------------ tester
        let info = adw::PreferencesGroup::builder().title("Tester").build();
        let (r, model) = value_row("Model");
        info.add(&r);
        let (serial_row, serial) = value_row("Serial Number");
        info.add(&serial_row);
        let (hardware_row, hardware) = value_row("Hardware Version");
        info.add(&hardware_row);
        let (firmware_row, firmware) = value_row("Firmware Version");
        firmware_row.set_subtitle("The exact version is on the tester's About screen");
        info.add(&firmware_row);
        let (bootloader_row, bootloader) = value_row("Bootloader Version");
        info.add(&bootloader_row);
        root.add(&info);

        let bt = adw::PreferencesGroup::builder()
            .title("Tester Bluetooth")
            .description(format!(
                "Up to {MAX_NAME_LEN} English letters, digits, “-” or “_”. The tester's Bluetooth restarts after renaming; Křižík reconnects automatically."
            ))
            .build();
        let name_row = adw::EntryRow::builder().title("Name").show_apply_button(true).build();
        if let Some(text) = name_row.delegate().and_downcast::<gtk::Text>() {
            text.set_max_length(MAX_NAME_LEN as i32);
        }
        bt.add(&name_row);
        let (address_row, address) = value_row("Address");
        bt.add(&address_row);
        let (signal_row, signal) = value_row("Signal Strength");
        // BlueZ rarely reports RSSI while connected; show it only when known.
        signal_row.set_visible(false);
        bt.add(&signal_row);
        root.add(&bt);

        let meas = adw::PreferencesGroup::builder()
            .title("Measurement")
            .description(
                "The upload rate is chosen on the tester: on its basic measurement screen, press the ▶ button.",
            )
            .build();
        let (r, interval) = value_row("Sample Rate");
        meas.add(&r);
        let (power_bank_row, power_bank) = value_row("Power Bank Mode");
        meas.add(&power_bank_row);
        let (temperature_row, temperature) = value_row("Temperature");
        meas.add(&temperature_row);
        root.add(&meas);

        let fw = adw::PreferencesGroup::builder().title("Firmware").build();
        let update_row = adw::ActionRow::builder()
            .title("Update Tester Firmware…")
            .subtitle("Over USB")
            .activatable(true)
            .action_name("app.firmware")
            .build();
        update_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        fw.add(&update_row);
        root.add(&fw);

        let (tester_end, disconnect) = disconnect_button("Disconnect Tester");
        root.add(&tester_end);

        // -------------------------------------------------------- multimeter
        let meter = adw::PreferencesGroup::builder().title("Multimeter").build();
        let (r, meter_model) = value_row("Model");
        meter.add(&r);
        let (r, meter_firmware) = value_row("Firmware Version");
        meter.add(&r);
        let (r, meter_battery) = value_row("Battery");
        meter.add(&r);
        root.add(&meter);

        let meter_bt = adw::PreferencesGroup::builder().title("Multimeter Bluetooth").build();
        let meter_name_row = adw::EntryRow::builder().title("Name").show_apply_button(true).build();
        if let Some(text) = meter_name_row.delegate().and_downcast::<gtk::Text>() {
            text.set_max_length(owon::MAX_NAME_LEN as i32);
        }
        meter_bt.add(&meter_name_row);
        let (r, meter_address) = value_row("Address");
        meter_bt.add(&r);
        let (signal_row, meter_signal) = value_row("Signal Strength");
        signal_row.set_visible(false);
        meter_bt.add(&signal_row);
        root.add(&meter_bt);

        let log_group = adw::PreferencesGroup::builder()
            .title("Offline Recording")
            .description("The multimeter can record readings by itself, without a connection, and keep them until they are downloaded.")
            .build();
        let (r, log_state) = value_row("State");
        log_group.add(&r);
        let log_start =
            action_row("Start Recording…", "Replaces the readings recorded before", "media-record-symbolic");
        log_group.add(&log_start);
        let log_stop = action_row("Stop Recording", "", "media-playback-stop-symbolic");
        log_stop.set_visible(false);
        log_group.add(&log_stop);
        let log_download =
            action_row("Download Readings…", "Saves the recorded readings as a CSV file", "document-save-as-symbolic");
        let log_spinner = gtk::Spinner::builder().visible(false).build();
        log_download.add_suffix(&log_spinner);
        log_group.add(&log_download);
        root.add(&log_group);

        let (meter_end, meter_disconnect) = disconnect_button("Disconnect Multimeter");
        root.add(&meter_end);

        let page = DevicePage {
            root,
            tester_groups: vec![info, bt.clone(), meas.clone(), fw.clone(), tester_end],
            meter_groups: vec![meter, meter_bt.clone(), log_group.clone(), meter_end],
            model,
            serial,
            hardware,
            firmware,
            bootloader,
            address,
            address_row,
            signal,
            bt,
            meas,
            power_bank_row,
            temperature_row,
            hardware_row,
            bootloader_row,
            firmware_row,
            serial_row,
            fw,
            name_row,
            interval,
            power_bank,
            temperature,
            disconnect,
            meter_model,
            meter_firmware,
            meter_battery,
            meter_address,
            meter_signal,
            meter_bt,
            meter_name_row,
            log_group,
            log_supported: Cell::new(true),
            log_state,
            log_start,
            log_stop,
            log_download,
            log_spinner,
            meter_disconnect,
        };
        page.set_parts(false, false);
        page
    }

    /// Shows the sections of the connected devices.
    pub fn set_parts(&self, tester: bool, meter: bool) {
        for g in &self.tester_groups {
            g.set_visible(tester && (g != &self.fw || self.address_row.title() != "USB Port"));
        }
        for g in &self.meter_groups {
            g.set_visible(meter && (g != &self.log_group || self.log_supported.get()));
        }
    }

    pub fn clear(&self) {
        for l in [
            &self.model,
            &self.serial,
            &self.hardware,
            &self.firmware,
            &self.bootloader,
            &self.interval,
            &self.power_bank,
            &self.temperature,
        ] {
            l.set_label(DASH);
        }
        hide_signal(&self.signal);
        self.name_row.set_text("");
    }

    pub fn clear_meter(&self) {
        for l in [&self.meter_model, &self.meter_firmware, &self.meter_battery, &self.log_state] {
            l.set_label(DASH);
        }
        hide_signal(&self.meter_signal);
        self.meter_name_row.set_text("");
        self.set_log_busy(false);
    }

    /// Shows how the tester is connected and adapts the page to its kind:
    /// the C2 over Bluetooth, or a tester over USB.
    pub fn set_connection(&self, id: DeviceId) {
        let usb = id.is_usb();
        match id {
            DeviceId::Usb(p) => {
                self.address_row.set_title("USB Port");
                self.address.set_label(&format!("{} ({:04X}:{:04X})", p.location(), p.vid, p.pid));
            }
            DeviceId::Ble(a) => {
                self.address_row.set_title("Address");
                self.address.set_label(&a.to_string());
            }
        }
        self.bt.set_title(if usb { "Tester Connection" } else { "Tester Bluetooth" });
        self.firmware_row.set_subtitle(if usb { "" } else { "The exact version is on the tester's About screen" });
        // USB testers report only some of these; they are shown once known.
        for r in [&self.serial_row, &self.hardware_row, &self.firmware_row, &self.bootloader_row] {
            r.set_visible(!usb);
        }
        self.meas.set_description(if usb {
            None
        } else {
            Some("The upload rate is chosen on the tester: on its basic measurement screen, press the ▶ button.")
        });
        self.power_bank_row.set_visible(!usb);
        self.fw.set_visible(!usb && self.tester_groups[0].is_visible());
        self.set_features(Features::ALL);
    }

    pub fn set_features(&self, f: Features) {
        self.name_row.set_visible(f.rename);
        let hint = format!(
            "Up to {MAX_NAME_LEN} English letters, digits, “-” or “_”. The tester's Bluetooth restarts after renaming; Křižík reconnects automatically."
        );
        self.bt.set_description(f.rename.then_some(hint.as_str()));
        self.temperature_row.set_visible(f.temperature);
    }

    /// Model, serial number and versions of testers other than the C2 over Bluetooth.
    pub fn set_details(&self, d: &TesterDetails) {
        let show = |row: &adw::ActionRow, label: &gtk::Label, value: &Option<String>| {
            if let Some(v) = value {
                label.set_label(v);
                row.set_visible(true);
            }
        };
        if let Some(m) = &d.model {
            self.model.set_label(m);
        }
        show(&self.serial_row, &self.serial, &d.serial);
        show(&self.firmware_row, &self.firmware, &d.firmware);
        show(&self.hardware_row, &self.hardware, &d.hardware);
    }

    pub fn set_model(&self, m: Model) {
        if self.model.label() == DASH {
            self.model.set_label(&m.name());
        }
    }

    pub fn set_info(&self, i: &DeviceInfo) {
        if !i.model_name.is_empty() {
            self.model.set_label(&i.model_name);
        }
        self.serial.set_label(if i.serial.is_empty() { DASH } else { &i.serial });
        self.hardware.set_label(&format_version(i.hardware_version));
        self.firmware.set_label(&format_firmware_series(i.firmware_series));
        self.bootloader.set_label(&format_version(i.bootloader_version));
    }

    pub fn set_name(&self, name: &str) {
        self.name_row.set_text(name);
    }

    pub fn set_rssi(&self, rssi: i16) {
        show_signal(&self.signal, rssi);
    }

    pub fn update(&self, b: &RealtimeBlock) {
        let ms = b.interval_ms.max(1) as f64;
        self.interval.set_label(&format!("{:.0} samples/s ({} ms)", 1000.0 / ms, b.interval_ms));
        self.power_bank.set_label(if b.power_bank_mode { "On" } else { "Off" });
        self.temperature.set_label(&if b.temperature_c > 0 { format!("{} °C", b.temperature_c) } else { DASH.into() });
    }

    // -------------------------------------------------------- multimeter

    pub fn set_meter_address(&self, address: &str) {
        self.meter_address.set_label(address);
    }

    pub fn set_meter_name(&self, name: &str) {
        self.meter_name_row.set_text(name);
    }

    pub fn set_meter_rssi(&self, rssi: i16) {
        show_signal(&self.meter_signal, rssi);
    }

    pub fn set_meter_info(&self, i: &Info) {
        self.meter_model.set_label(&i.model_name());
        self.meter_firmware.set_label(&i.firmware_text().unwrap_or_else(|| DASH.into()));
        self.meter_battery.set_label(&i.battery.map_or_else(|| DASH.into(), |b| format!("{b} %")));
        self.meter_name_row.set_sensitive(i.can_rename());
        self.meter_bt.set_description(Some(&if i.can_rename() {
            format!("Up to {} English letters, digits or “_”.", owon::MAX_NAME_LEN)
        } else {
            "This multimeter's firmware can't change its name.".into()
        }));
        self.log_supported.set(i.supports_log());
        self.log_group.set_visible(i.supports_log() && self.meter_groups[0].is_visible());
        self.log_state.set_label(match i.log {
            LogState::Unsupported => "Not supported",
            LogState::Idle => "Not recording",
            LogState::Recording => "Recording",
        });
        let recording = i.log == LogState::Recording;
        self.log_stop.set_visible(recording);
        self.log_start.set_sensitive(!recording);
    }

    /// Shows download progress; `None` hides it.
    pub fn set_log_progress(&self, progress: Option<(usize, Option<u32>)>) {
        match progress {
            Some((received, Some(total))) if total > 0 => {
                let pct = (received as f64 / total as f64 * 100.0).min(100.0);
                self.log_download.set_subtitle(&format!("Downloading… {pct:.0} %"));
            }
            Some((received, _)) => self.log_download.set_subtitle(&format!("Downloading… {received} bytes")),
            None => self.log_download.set_subtitle("Saves the recorded readings as a CSV file"),
        }
        self.set_log_busy(progress.is_some());
    }

    fn set_log_busy(&self, busy: bool) {
        self.log_spinner.set_visible(busy);
        self.log_spinner.set_spinning(busy);
        self.log_download.set_sensitive(!busy);
        if !busy {
            self.log_download.set_subtitle("Saves the recorded readings as a CSV file");
        }
    }
}
