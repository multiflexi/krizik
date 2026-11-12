//! Preferences dialog.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use super::window::Window;

pub fn show(win: &Rc<Window>) {
    let c = win.config.borrow().clone();
    let dialog = adw::PreferencesDialog::new();
    let page = adw::PreferencesPage::builder().title("General").icon_name("emblem-system-symbolic").build();

    let conn = adw::PreferencesGroup::builder().title("Connection").build();
    let auto = adw::SwitchRow::builder()
        .title("Reconnect on Start")
        .subtitle("Connect to the last used tester and multimeter when Křižík starts")
        .active(c.auto_connect)
        .build();
    let awake = adw::SwitchRow::builder()
        .title("Keep Screen On")
        .subtitle("While connected, keep the screen on and the device awake")
        .active(c.keep_awake)
        .build();
    let all = adw::SwitchRow::builder()
        .title("Show All Bluetooth Devices")
        .subtitle("Not only devices that look like a tester or multimeter")
        .active(c.show_all_devices)
        .build();
    conn.add(&auto);
    conn.add(&awake);
    conn.add(&all);
    page.add(&conn);

    let graph = adw::PreferencesGroup::builder().title("Graph").build();
    let history = adw::SpinRow::with_range(1.0, 1440.0, 5.0);
    history.set_title("History Length");
    history.set_subtitle("Minutes of samples kept in memory for the graph and export");
    history.set_value(c.history_minutes as f64);
    graph.add(&history);
    page.add(&graph);

    let weak = Rc::downgrade(win);
    let apply = Rc::new(glib::clone!(
        #[weak]
        auto,
        #[weak]
        awake,
        #[weak]
        all,
        #[weak]
        history,
        move || {
            let Some(w) = weak.upgrade() else { return };
            {
                let mut c = w.config.borrow_mut();
                c.auto_connect = auto.is_active();
                c.keep_awake = awake.is_active();
                c.show_all_devices = all.is_active();
                c.history_minutes = history.value() as u32;
            }
            w.settings_changed();
        }
    ));
    for row in [&auto, &awake, &all] {
        let a = apply.clone();
        row.connect_active_notify(move |_| a());
    }
    history.connect_value_notify(move |_| apply());

    dialog.add(&page);
    dialog.present(Some(&win.window));
}
