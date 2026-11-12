//! "Live" page: large readouts that reflow from one to three columns.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use super::format;
use crate::ble::Features;
use crate::owon::{Button, Flags, Info};
use crate::session::{MeterPoint, Session};

/// Called with the button and whether it was a long press.
pub type ButtonCallback = Rc<RefCell<Option<Box<dyn Fn(Button, bool)>>>>;

const METER_BUTTONS: [Button; 6] =
    [Button::Select, Button::Range, Button::Hold, Button::Rel, Button::HzDuty, Button::MaxMin];

/// The multimeter's reading and its remote control.
struct MeterCard {
    caption: gtk::Label,
    value: gtk::Label,
    unit: gtk::Label,
    flags: gtk::Label,
    max_min: gtk::Button,
}

impl MeterCard {
    fn new(on_button: &ButtonCallback) -> (gtk::Box, MeterCard) {
        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .css_classes(["card", "reading-card"])
            .build();
        let caption = gtk::Label::builder()
            .label("MULTIMETER")
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["series-caption", "series-meter"])
            .build();
        let row = gtk::Box::builder().spacing(6).build();
        let value = gtk::Label::builder()
            .label(format::DASH)
            .xalign(0.0)
            .valign(gtk::Align::Baseline)
            .css_classes(["reading-value"])
            .build();
        let unit = gtk::Label::builder().valign(gtk::Align::Baseline).css_classes(["reading-unit"]).build();
        row.append(&value);
        row.append(&unit);
        let flags =
            gtk::Label::builder().label("\u{a0}").xalign(0.0).css_classes(["dim-label", "caption-heading"]).build();
        card.append(&caption);
        card.append(&row);
        card.append(&flags);

        let buttons = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(true)
            .min_children_per_line(2)
            .max_children_per_line(6)
            .column_spacing(6)
            .row_spacing(6)
            .margin_top(10)
            .build();
        let mut max_min = None;
        for button in METER_BUTTONS {
            let b = gtk::Button::builder()
                .label(button.label())
                .tooltip_text(format!("{} on the multimeter; press and hold for a long press", button.label()))
                .build();
            // A long press claims the touch or click so that the button
            // doesn't also report a short one.
            let long = gtk::GestureLongPress::new();
            long.set_propagation_phase(gtk::PropagationPhase::Capture);
            let cb = on_button.clone();
            long.connect_pressed(move |g, _, _| {
                g.set_state(gtk::EventSequenceState::Claimed);
                if let Some(f) = cb.borrow().as_ref() {
                    f(button, true);
                }
            });
            b.add_controller(long);
            let cb = on_button.clone();
            b.connect_clicked(move |_| {
                if let Some(f) = cb.borrow().as_ref() {
                    f(button, false);
                }
            });
            let child = gtk::FlowBoxChild::builder().child(&b).focusable(false).build();
            buttons.append(&child);
            if button == Button::MaxMin {
                max_min = Some(b);
            }
        }
        card.append(&buttons);
        (card, MeterCard { caption, value, unit, flags, max_min: max_min.expect("Max/Min button") })
    }
}

fn flag_text(flags: Flags) -> String {
    let names: Vec<&str> = flags.names().into_iter().map(|n| if n == "BAT" { "LOW BATTERY" } else { n }).collect();
    if names.is_empty() {
        "\u{a0}".into()
    } else {
        names.join(" · ")
    }
}

struct Reading {
    value: gtk::Label,
}

fn big_reading(caption: &str, unit: &str, css: &str) -> (gtk::Box, Reading) {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .css_classes(["card", "reading-card"])
        .build();
    let cap =
        gtk::Label::builder().label(caption.to_uppercase()).xalign(0.0).css_classes(["series-caption", css]).build();
    let row = gtk::Box::builder().spacing(6).build();
    let value = gtk::Label::builder().label(format::DASH).xalign(0.0).css_classes(["reading-value"]).build();
    let unit_l = gtk::Label::builder().label(unit).valign(gtk::Align::Baseline).css_classes(["reading-unit"]).build();
    value.set_valign(gtk::Align::Baseline);
    row.append(&value);
    row.append(&unit_l);
    card.append(&cap);
    card.append(&row);
    (card, Reading { value })
}

fn small_reading(caption: &str, css: Option<&str>) -> (gtk::Box, gtk::Label) {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .css_classes(["card", "small-card"])
        .build();
    let cap = gtk::Label::builder().label(caption).xalign(0.0).css_classes(["caption-heading"]).build();
    match css {
        Some(c) => cap.add_css_class(c),
        None => cap.add_css_class("dim-label"),
    }
    let value = gtk::Label::builder().label(format::DASH).xalign(0.0).css_classes(["small-value"]).build();
    card.append(&cap);
    card.append(&value);
    (card, value)
}

fn flow(max_per_line: u32, min_per_line: u32) -> gtk::FlowBox {
    gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .min_children_per_line(min_per_line)
        .max_children_per_line(max_per_line)
        .column_spacing(12)
        .row_spacing(12)
        .build()
}

fn add_to_flow(flow: &gtk::FlowBox, w: &impl IsA<gtk::Widget>) -> gtk::Widget {
    let child = gtk::FlowBoxChild::builder().child(w).focusable(false).build();
    flow.append(&child);
    child.upcast()
}

pub fn value_row(title: &str) -> (adw::ActionRow, gtk::Label) {
    let row = adw::ActionRow::builder().title(title).build();
    let label = gtk::Label::builder().label(format::DASH).xalign(1.0).css_classes(["numeric-value"]).build();
    row.add_suffix(&label);
    (row, label)
}

/// A row showing its value below the title, for long values on narrow screens.
pub struct StackedRow {
    pub row: adw::ActionRow,
}

impl StackedRow {
    pub fn new(title: &str) -> StackedRow {
        let row = adw::ActionRow::builder().title(title).subtitle(format::DASH).css_classes(["property"]).build();
        row.set_subtitle_selectable(true);
        StackedRow { row }
    }

    pub fn set(&self, text: &str) {
        self.row.set_subtitle(text);
    }
}

pub struct LivePage {
    pub root: gtk::ScrolledWindow,
    tester_box: gtk::Box,
    meter_box: gtk::Box,
    placeholder: adw::StatusPage,
    meter: MeterCard,
    pub on_meter_button: ButtonCallback,
    volt: Reading,
    amp: Reading,
    watt: Reading,
    dp: gtk::Label,
    dm: gtk::Label,
    cc1: gtk::Label,
    cc2: gtk::Label,
    res: gtk::Label,
    temp: gtk::Label,
    protocol: gtk::Label,
    gear: gtk::Label,
    request: gtk::Label,
    data_cards: Vec<gtk::Widget>,
    cc_cards: Vec<gtk::Widget>,
    temp_card: gtk::Widget,
    fast_group: adw::PreferencesGroup,
    capacity: gtk::Label,
    energy: gtk::Label,
    duration: gtk::Label,
    pub reset_button: gtk::Button,
}

impl LivePage {
    pub fn new() -> LivePage {
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();

        let on_meter_button: ButtonCallback = Rc::default();
        let (meter_card, meter) = MeterCard::new(&on_meter_button);
        let meter_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).visible(false).build();
        meter_box.append(&meter_card);
        content.append(&meter_box);

        let placeholder = adw::StatusPage::builder()
            .icon_name("bluetooth-acquiring-symbolic")
            .title("Connecting…")
            .vexpand(true)
            .build();
        content.append(&placeholder);

        let tester_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(18).visible(false).build();
        content.append(&tester_box);
        let content_all = content;
        let content = &tester_box;

        let big = flow(3, 1);
        let (c, volt) = big_reading("Voltage", "V", "series-voltage");
        c.set_width_request(220);
        add_to_flow(&big, &c);
        let (c, amp) = big_reading("Current", "A", "series-current");
        add_to_flow(&big, &c);
        let (c, watt) = big_reading("Power", "W", "series-power");
        add_to_flow(&big, &c);
        content.append(&big);

        let small = flow(6, 2);
        let (c, dp) = small_reading("D+", Some("series-data"));
        c.set_width_request(96);
        let dp_card = add_to_flow(&small, &c);
        let (c, dm) = small_reading("D−", Some("series-data"));
        let dm_card = add_to_flow(&small, &c);
        let (c, cc1) = small_reading("CC1", Some("series-cc"));
        let cc1_card = add_to_flow(&small, &c);
        let (c, cc2) = small_reading("CC2", Some("series-cc"));
        let cc2_card = add_to_flow(&small, &c);
        let (c, res) = small_reading("Load", None);
        add_to_flow(&small, &c);
        let (c, temp) = small_reading("Temperature", None);
        let temp_card = add_to_flow(&small, &c);
        content.append(&small);

        let groups = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(18).build();
        let fast = adw::PreferencesGroup::builder()
            .title("Fast Charging")
            .description("As detected by the tester while monitoring")
            .build();
        let (r, protocol) = value_row("Protocol");
        fast.add(&r);
        let (r, gear) = value_row("Gear");
        fast.add(&r);
        let (r, request) = value_row("Requested");
        fast.add(&r);
        groups.append(&fast);
        let fast_group = fast;

        let reset_button = gtk::Button::builder()
            .icon_name("edit-clear-all-symbolic")
            .tooltip_text("Reset Capacity and Energy")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let session_group = adw::PreferencesGroup::builder().title("Session").header_suffix(&reset_button).build();
        let (r, capacity) = value_row("Capacity");
        session_group.add(&r);
        let (r, energy) = value_row("Energy");
        session_group.add(&r);
        let (r, duration) = value_row("Duration");
        session_group.add(&r);
        groups.append(&session_group);
        content.append(&groups);

        let clamp = adw::Clamp::builder().maximum_size(960).tightening_threshold(600).child(&content_all).build();
        let root = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();

        LivePage {
            root,
            tester_box,
            meter_box,
            placeholder,
            meter,
            on_meter_button,
            volt,
            amp,
            watt,
            dp,
            dm,
            cc1,
            cc2,
            res,
            temp,
            protocol,
            gear,
            request,
            data_cards: vec![dp_card, dm_card],
            cc_cards: vec![cc1_card, cc2_card],
            temp_card,
            fast_group,
            capacity,
            energy,
            duration,
            reset_button,
        }
    }

    pub fn update(&self, session: &Session) {
        if let Some(s) = session.latest {
            self.volt.value.set_label(&format::volts(s.v));
            self.amp.value.set_label(&format::amps(s.i));
            self.watt.value.set_label(&format::watts(s.p));
            self.dp.set_label(&format!("{} V", format::data_volts(s.dp)));
            self.dm.set_label(&format!("{} V", format::data_volts(s.dm)));
            self.cc1.set_label(&format!("{} V", format::data_volts(s.cc1)));
            self.cc2.set_label(&format!("{} V", format::data_volts(s.cc2)));
            let (r, unit) = format::ohms(s.resistance());
            self.res.set_label(&if r == format::DASH { r } else { format!("{r} {unit}") });
        }
        if let Some(b) = &session.last_block {
            let p = &b.protocol;
            self.protocol.set_label(&format::protocol_text(&[&p.name, &p.power]));
            self.gear.set_label(&format::protocol_text(&[&p.gear, &p.gear_detail]));
            self.request.set_label(&format::protocol_text(&[&p.request, &p.request_detail]));
            self.temp.set_label(&if b.temperature_c > 0 {
                format!("{} °C", b.temperature_c)
            } else {
                format::DASH.to_string()
            });
        }
        self.update_stats(session);
    }

    pub fn update_stats(&self, session: &Session) {
        let st = &session.stats;
        self.capacity.set_label(&format::capacity(st.amp_hours));
        self.energy.set_label(&format::energy(st.watt_hours));
        let mut d = format::duration(st.duration);
        if !session.accumulating {
            d.push_str(" (stopped)");
        }
        self.duration.set_label(&d);
    }

    /// Shows the parts for the connected devices.
    pub fn set_parts(&self, tester: bool, meter: bool) {
        self.tester_box.set_visible(tester);
        self.meter_box.set_visible(meter);
        self.placeholder.set_visible(!tester && !meter);
    }

    /// Hides what the connected tester doesn't measure.
    pub fn set_features(&self, f: Features) {
        for c in &self.data_cards {
            c.set_visible(f.data_lines);
        }
        for c in &self.cc_cards {
            c.set_visible(f.cc_lines);
        }
        self.temp_card.set_visible(f.temperature);
        self.fast_group.set_visible(f.protocol);
    }

    pub fn set_meter_info(&self, info: &Info) {
        self.meter.max_min.set_visible(info.series.has_max_min());
    }

    pub fn update_meter(&self, point: Option<&MeterPoint>) {
        let m = &self.meter;
        match point {
            Some(p) => {
                let r = &p.reading;
                m.caption.set_label(&format!("Multimeter · {}", r.function.label()).to_uppercase());
                m.value.set_label(&r.display());
                let unit = r.unit();
                m.unit.set_label(if unit.is_empty() { r.function.short() } else { &unit });
                m.flags.set_label(&flag_text(r.flags));
            }
            None => {
                m.caption.set_label("MULTIMETER");
                m.value.set_label(format::DASH);
                m.unit.set_label("");
                m.flags.set_label("\u{a0}");
            }
        }
    }

    pub fn clear(&self) {
        for l in [&self.volt.value, &self.amp.value, &self.watt.value] {
            l.set_label(format::DASH);
        }
        for l in
            [&self.dp, &self.dm, &self.cc1, &self.cc2, &self.res, &self.temp, &self.protocol, &self.gear, &self.request]
        {
            l.set_label(format::DASH);
        }
    }
}
