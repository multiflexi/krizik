//! "Graph" page: lane toggles, time window, pause and the chart.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use super::chart::{Chart, Lane};
use crate::session::Session;

pub const WINDOWS: [(u32, &str); 7] =
    [(10, "10 s"), (30, "30 s"), (60, "1 min"), (300, "5 min"), (900, "15 min"), (3600, "1 h"), (0, "All")];

pub struct GraphPage {
    pub root: gtk::Box,
    pub controls: gtk::Box,
    pub chart: Chart,
    pub lane_buttons: Vec<(Lane, gtk::ToggleButton)>,
    pub window_dropdown: gtk::DropDown,
    pub pause: gtk::ToggleButton,
    pub clear: gtk::Button,
    pub export: gtk::Button,
}

impl GraphPage {
    pub fn new(session: Rc<RefCell<Session>>, lanes: Vec<Lane>, window_secs: u32) -> GraphPage {
        let chart = Chart::new(session.clone(), lanes.clone(), window_secs as f64);

        let toggles = gtk::Box::builder().css_classes(["linked"]).halign(gtk::Align::Start).build();
        let mut lane_buttons = Vec::new();
        for lane in Lane::ALL {
            let b = gtk::ToggleButton::builder()
                .label(lane.short())
                .tooltip_text(lane.title())
                .active(lanes.contains(&lane))
                .css_classes(["lane-toggle", lane.css_class()])
                .build();
            toggles.append(&b);
            lane_buttons.push((lane, b));
        }

        let labels: Vec<&str> = WINDOWS.iter().map(|(_, l)| *l).collect();
        let window_dropdown = gtk::DropDown::from_strings(&labels);
        window_dropdown.set_tooltip_text(Some("Visible Time Span"));
        let idx = WINDOWS.iter().position(|(s, _)| *s == window_secs).unwrap_or(2);
        window_dropdown.set_selected(idx as u32);

        let pause =
            gtk::ToggleButton::builder().icon_name("media-playback-pause-symbolic").tooltip_text("Pause Graph").build();
        let clear = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text("Clear Graph").build();
        let export = gtk::Button::builder()
            .icon_name("document-save-as-symbolic")
            .tooltip_text("Export Graph Data as CSV…")
            .build();

        let right = gtk::Box::builder().spacing(6).build();
        right.append(&window_dropdown);
        right.append(&pause);
        right.append(&clear);
        right.append(&export);

        let controls = gtk::Box::builder().spacing(8).margin_top(8).margin_start(12).margin_end(12).build();
        controls.append(&toggles);
        controls.append(&gtk::Box::builder().hexpand(true).vexpand(false).build());
        controls.append(&right);

        let frame = gtk::Frame::builder()
            .child(&chart.area)
            .margin_start(8)
            .margin_end(8)
            .margin_bottom(8)
            .vexpand(true)
            .css_classes(["view", "chart-frame"])
            .build();
        chart.area.set_margin_top(6);
        chart.area.set_margin_bottom(4);
        chart.area.set_margin_end(4);

        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
        root.append(&controls);
        root.append(&frame);

        let page = GraphPage { root, controls, chart, lane_buttons, window_dropdown, pause, clear, export };
        page.connect_internal(session);
        page
    }

    fn connect_internal(&self, session: Rc<RefCell<Session>>) {
        let chart = self.chart.clone();
        self.window_dropdown.connect_selected_notify(glib::clone!(
            #[strong]
            chart,
            move |dd| {
                let secs = WINDOWS.get(dd.selected() as usize).map_or(60, |w| w.0);
                chart.state.borrow_mut().window_secs = secs as f64;
                chart.redraw();
            }
        ));
        self.pause.connect_toggled(glib::clone!(
            #[strong]
            chart,
            #[strong]
            session,
            move |b| {
                let end = if b.is_active() { Some(session.borrow().latest_t().unwrap_or(0.0)) } else { None };
                chart.state.borrow_mut().frozen_end = end;
                b.set_icon_name(if b.is_active() {
                    "media-playback-start-symbolic"
                } else {
                    "media-playback-pause-symbolic"
                });
                b.set_tooltip_text(Some(if b.is_active() { "Resume Graph" } else { "Pause Graph" }));
                chart.redraw();
            }
        ));
        for (_, b) in &self.lane_buttons {
            let buttons = self.lane_buttons.clone();
            b.connect_toggled(glib::clone!(
                #[strong]
                chart,
                move |_| {
                    chart.state.borrow_mut().lanes =
                        buttons.iter().filter(|(_, b)| b.is_active()).map(|(l, _)| *l).collect();
                    chart.redraw();
                }
            ));
        }
    }

    pub fn lanes(&self) -> Vec<Lane> {
        self.chart.state.borrow().lanes.clone()
    }

    pub fn window_secs(&self) -> u32 {
        WINDOWS.get(self.window_dropdown.selected() as usize).map_or(60, |w| w.0)
    }
}
