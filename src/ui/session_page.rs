//! "Session" page: statistics, recording, automatic stop and alarms.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use super::format;
use super::live::{value_row, StackedRow};
use crate::config::Config;
use crate::session::{AlarmKind, AlarmLimit, MinMax, Session};

pub struct SessionPage {
    pub root: adw::PreferencesPage,
    duration: gtk::Label,
    capacity: gtk::Label,
    energy: gtk::Label,
    voltage: StackedRow,
    current: StackedRow,
    power: StackedRow,
    meter: StackedRow,
    tester_rows: Vec<gtk::Widget>,
    pub reset: gtk::Button,
    pub counting: gtk::ToggleButton,
    pub record_row: adw::ActionRow,
    pub record_button: gtk::Button,
    pub export_button: gtk::Button,
}

fn spin(title: &str, subtitle: Option<&str>, min: f64, max: f64, step: f64, digits: u32, value: f64) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(min, max, step);
    row.set_title(title);
    if let Some(s) = subtitle {
        row.set_subtitle(s);
    }
    row.set_digits(digits);
    row.set_value(value);
    row
}

fn minmax_text(m: &MinMax, unit: &str, f: fn(f32) -> String) -> String {
    match m.avg() {
        None => format::DASH.into(),
        Some(avg) => format!("{} / {} / {} {unit}", f(m.min), f(avg), f(m.max)),
    }
}

impl SessionPage {
    pub fn new(config: Rc<RefCell<Config>>) -> SessionPage {
        let root = adw::PreferencesPage::new();

        // Statistics
        let reset = gtk::Button::builder()
            .icon_name("edit-clear-all-symbolic")
            .tooltip_text("Reset Statistics")
            .css_classes(["flat"])
            .build();
        let counting = gtk::ToggleButton::builder()
            .icon_name("media-playback-pause-symbolic")
            .tooltip_text("Pause Counting")
            .css_classes(["flat"])
            .build();
        let suffix = gtk::Box::builder().spacing(6).valign(gtk::Align::Center).build();
        suffix.append(&counting);
        suffix.append(&reset);
        let stats = adw::PreferencesGroup::builder()
            .title("Statistics")
            .description("Minimum / average / maximum since the last reset")
            .header_suffix(&suffix)
            .build();
        let (r, duration) = value_row("Duration");
        stats.add(&r);
        let (r, capacity) = value_row("Capacity");
        stats.add(&r);
        let (r, energy) = value_row("Energy");
        stats.add(&r);
        let voltage = StackedRow::new("Voltage");
        stats.add(&voltage.row);
        let current = StackedRow::new("Current");
        stats.add(&current.row);
        let power = StackedRow::new("Power");
        stats.add(&power.row);
        let meter = StackedRow::new("Multimeter");
        meter.row.set_visible(false);
        stats.add(&meter.row);
        root.add(&stats);
        let tester_rows: Vec<gtk::Widget> = [&voltage.row, &current.row, &power.row]
            .into_iter()
            .map(|r| r.clone().upcast())
            .chain(
                [&duration, &capacity, &energy].into_iter().filter_map(|l| l.ancestor(adw::ActionRow::static_type())),
            )
            .collect();

        // Recording
        let rec = adw::PreferencesGroup::builder().title("Recording").build();
        let record_button =
            gtk::Button::builder().label("Start").valign(gtk::Align::Center).css_classes(["suggested-action"]).build();
        let record_row = adw::ActionRow::builder().title("Record to File").subtitle(RECORD_HINT).build();
        record_row.add_prefix(&gtk::Image::from_icon_name("media-record-symbolic"));
        record_row.add_suffix(&record_button);
        rec.add(&record_row);
        let export_button = gtk::Button::builder()
            .icon_name("document-save-as-symbolic")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .tooltip_text("Export…")
            .build();
        let export_row = adw::ActionRow::builder()
            .title("Export Graph Data")
            .subtitle("Saves the samples kept in memory as CSV")
            .activatable_widget(&export_button)
            .build();
        export_row.add_suffix(&export_button);
        rec.add(&export_row);
        root.add(&rec);

        // Automatic stop
        let a = config.borrow().autostop;
        let auto = adw::PreferencesGroup::builder()
            .title("Automatic Stop")
            .description("Stops counting capacity and energy, for example when charging has finished")
            .build();
        let exp = adw::ExpanderRow::builder()
            .title("Stop Automatically")
            .show_enable_switch(true)
            .enable_expansion(a.enabled)
            .expanded(a.enabled)
            .build();
        let below = spin("Current Below", Some("mA, 0 = off"), 0.0, 10_000.0, 10.0, 0, a.current_below_ma);
        let below_for = spin("For", Some("seconds"), 1.0, 3600.0, 5.0, 0, a.below_for_secs);
        let minutes = spin("Time Limit", Some("minutes, 0 = off"), 0.0, 100_000.0, 10.0, 0, a.max_minutes);
        let mah = spin("Capacity Limit", Some("mAh, 0 = off"), 0.0, 1_000_000.0, 100.0, 0, a.max_mah);
        let wh = spin("Energy Limit", Some("Wh, 0 = off"), 0.0, 100_000.0, 1.0, 2, a.max_wh);
        for r in [&below, &below_for, &minutes, &mah, &wh] {
            exp.add_row(r);
        }
        auto.add(&exp);
        root.add(&auto);

        let save_auto = Rc::new(glib::clone!(
            #[strong]
            config,
            #[weak]
            exp,
            #[weak]
            below,
            #[weak]
            below_for,
            #[weak]
            minutes,
            #[weak]
            mah,
            #[weak]
            wh,
            move || {
                let mut c = config.borrow_mut();
                c.autostop.enabled = exp.enables_expansion();
                c.autostop.current_below_ma = below.value();
                c.autostop.below_for_secs = below_for.value();
                c.autostop.max_minutes = minutes.value();
                c.autostop.max_mah = mah.value();
                c.autostop.max_wh = wh.value();
                c.save();
            }
        ));
        exp.connect_enable_expansion_notify(glib::clone!(
            #[strong]
            save_auto,
            move |_| save_auto()
        ));
        for r in [&below, &below_for, &minutes, &mah, &wh] {
            r.connect_value_notify(glib::clone!(
                #[strong]
                save_auto,
                move |_| save_auto()
            ));
        }

        // Alarms
        let alarms = adw::PreferencesGroup::builder()
            .title("Alarms")
            .description("Warn when the tester's voltage, current or power goes outside a limit")
            .build();
        for kind in AlarmKind::ALL {
            let lim = config.borrow().alarm(kind);
            let (max, step, digits) = match kind {
                AlarmKind::OverCurrent => (12.0, 0.1, 2),
                AlarmKind::OverPower => (300.0, 1.0, 1),
                _ => (60.0, 0.1, 2),
            };
            let exp = adw::ExpanderRow::builder()
                .title(kind.title())
                .subtitle(alarm_subtitle(kind, lim))
                .show_enable_switch(true)
                .enable_expansion(lim.enabled)
                .expanded(false)
                .build();
            let limit = spin("Limit", Some(kind.unit()), 0.0, max, step, digits, lim.limit as f64);
            exp.add_row(&limit);
            let save = Rc::new(glib::clone!(
                #[strong]
                config,
                #[weak]
                exp,
                #[weak]
                limit,
                move || {
                    let lim = AlarmLimit { enabled: exp.enables_expansion(), limit: limit.value() as f32 };
                    exp.set_subtitle(&alarm_subtitle(kind, lim));
                    let mut c = config.borrow_mut();
                    c.set_alarm(kind, lim);
                    c.save();
                }
            ));
            exp.connect_enable_expansion_notify(glib::clone!(
                #[strong]
                save,
                move |_| save()
            ));
            limit.connect_value_notify(glib::clone!(
                #[strong]
                save,
                move |_| save()
            ));
            alarms.add(&exp);
        }
        let (sound_on, notify_on) = {
            let c = config.borrow();
            (c.alarm_sound, c.alarm_notify)
        };
        let sound = adw::SwitchRow::builder().title("Sound").active(sound_on).build();
        let notify = adw::SwitchRow::builder()
            .title("Notifications")
            .subtitle("Also notify when Křižík is in the background")
            .active(notify_on)
            .build();
        sound.connect_active_notify(glib::clone!(
            #[strong]
            config,
            move |r| {
                let mut c = config.borrow_mut();
                c.alarm_sound = r.is_active();
                c.save();
            }
        ));
        notify.connect_active_notify(glib::clone!(
            #[strong]
            config,
            move |r| {
                let mut c = config.borrow_mut();
                c.alarm_notify = r.is_active();
                c.save();
            }
        ));
        alarms.add(&sound);
        alarms.add(&notify);
        root.add(&alarms);

        SessionPage {
            root,
            duration,
            capacity,
            energy,
            voltage,
            current,
            power,
            meter,
            tester_rows,
            reset,
            counting,
            record_row,
            record_button,
            export_button,
        }
    }

    pub fn update(&self, session: &Session) {
        let st = &session.stats;
        let mut d = format::duration(st.duration);
        if !session.accumulating {
            d.push_str(" (stopped)");
        }
        self.duration.set_label(&d);
        self.capacity.set_label(&format::capacity(st.amp_hours));
        self.energy.set_label(&format::energy(st.watt_hours));
        self.voltage.set(&minmax_text(&st.voltage, "V", format::volts));
        self.current.set(&minmax_text(&st.current, "A", format::amps));
        self.power.set(&minmax_text(&st.power, "W", format::watts));
        let ms = &session.meter_stats;
        match ms.function {
            Some(f) => {
                self.meter.row.set_title(&format!("Multimeter · {}", f.label()));
                self.meter.set(&match ms.values.avg() {
                    Some(avg) => {
                        let fmt = |v: f32| format::si(v as f64, f.base_unit(), f.uses_prefix());
                        format!("{} / {} / {}", fmt(ms.values.min), fmt(avg), fmt(ms.values.max))
                    }
                    None => format::DASH.into(),
                });
            }
            None => {
                self.meter.row.set_title("Multimeter");
                self.meter.set(format::DASH);
            }
        }
        let paused = !session.accumulating;
        if self.counting.is_active() != paused {
            self.counting.set_active(paused);
        }
        self.counting.set_icon_name(if paused {
            "media-playback-start-symbolic"
        } else {
            "media-playback-pause-symbolic"
        });
        self.counting.set_tooltip_text(Some(if paused { "Resume Counting" } else { "Pause Counting" }));
    }

    pub fn set_recording(&self, path: Option<&str>, rows: u64) {
        match path {
            Some(p) => {
                self.record_button.set_label("Stop");
                self.record_button.remove_css_class("suggested-action");
                self.record_button.add_css_class("destructive-action");
                self.record_row.set_subtitle(&format!("{p} · {rows} rows"));
            }
            None => {
                self.record_button.set_label("Start");
                self.record_button.remove_css_class("destructive-action");
                self.record_button.add_css_class("suggested-action");
                self.record_row.set_subtitle(RECORD_HINT);
            }
        }
    }

    /// Shows the statistics of the connected devices.
    pub fn set_parts(&self, tester: bool, meter: bool) {
        for r in &self.tester_rows {
            r.set_visible(tester);
        }
        self.meter.row.set_visible(meter);
    }
}

const RECORD_HINT: &str = "Writes the tester's samples and the multimeter's readings to a CSV file";

fn alarm_subtitle(kind: AlarmKind, lim: AlarmLimit) -> String {
    let rel = if kind == AlarmKind::UnderVoltage { "Below" } else { "Above" };
    let state = if lim.enabled { "" } else { " (off)" };
    format!("{rel} {:.2} {}{state}", lim.limit, kind.unit())
}
