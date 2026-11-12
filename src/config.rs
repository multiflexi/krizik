//! Persistent settings stored as an ini file in the user config directory.

use std::path::PathBuf;

use gtk::glib;

use crate::ble::Kind;
use crate::session::{AlarmKind, AlarmLimit, AutoStopConfig};

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub auto_connect: bool,
    pub last_device: Option<String>,
    pub last_device_name: Option<String>,
    pub last_meter: Option<String>,
    pub last_meter_name: Option<String>,
    pub keep_awake: bool,
    pub history_minutes: u32,
    pub show_all_devices: bool,
    pub graph_window_secs: u32,
    pub graph_lanes: Vec<String>,
    pub alarm_sound: bool,
    pub alarm_notify: bool,
    pub alarms: Vec<(AlarmKind, AlarmLimit)>,
    pub autostop: AutoStopConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            auto_connect: true,
            last_device: None,
            last_device_name: None,
            last_meter: None,
            last_meter_name: None,
            keep_awake: true,
            history_minutes: 60,
            show_all_devices: false,
            graph_window_secs: 60,
            graph_lanes: vec!["voltage".into(), "current".into(), "power".into(), "meter".into()],
            alarm_sound: true,
            alarm_notify: true,
            alarms: vec![
                (AlarmKind::OverVoltage, AlarmLimit { enabled: false, limit: 21.0 }),
                (AlarmKind::UnderVoltage, AlarmLimit { enabled: false, limit: 4.5 }),
                (AlarmKind::OverCurrent, AlarmLimit { enabled: false, limit: 3.0 }),
                (AlarmKind::OverPower, AlarmLimit { enabled: false, limit: 60.0 }),
            ],
            autostop: AutoStopConfig::default(),
        }
    }
}

fn path() -> PathBuf {
    glib::user_config_dir().join("krizik").join("settings.ini")
}

impl Config {
    pub fn load() -> Config {
        let mut c = Config::default();
        let kf = glib::KeyFile::new();
        if kf.load_from_file(path(), glib::KeyFileFlags::NONE).is_err() {
            return c;
        }
        let b = |g: &str, k: &str, d: bool| kf.boolean(g, k).unwrap_or(d);
        let f = |g: &str, k: &str, d: f64| kf.double(g, k).unwrap_or(d);
        let s = |g: &str, k: &str| kf.string(g, k).ok().map(|s| s.to_string()).filter(|s| !s.is_empty());

        c.auto_connect = b("general", "auto_connect", c.auto_connect);
        c.last_device = s("general", "last_device");
        c.last_device_name = s("general", "last_device_name");
        c.last_meter = s("general", "last_meter");
        c.last_meter_name = s("general", "last_meter_name");
        c.keep_awake = b("general", "keep_awake", c.keep_awake);
        c.history_minutes = f("general", "history_minutes", c.history_minutes as f64).clamp(1.0, 1440.0) as u32;
        c.show_all_devices = b("general", "show_all_devices", c.show_all_devices);
        c.graph_window_secs = f("graph", "window_secs", c.graph_window_secs as f64).max(0.0) as u32;
        if let Some(lanes) = s("graph", "lanes") {
            c.graph_lanes = lanes.split(',').map(str::to_string).collect();
            // Settings from before multimeter support: show its lane once.
            if !b("graph", "has_meter_lane", false) && !c.graph_lanes.iter().any(|l| l == "meter") {
                c.graph_lanes.push("meter".into());
            }
        }
        c.alarm_sound = b("alarms", "sound", c.alarm_sound);
        c.alarm_notify = b("alarms", "notify", c.alarm_notify);
        for (kind, lim) in c.alarms.iter_mut() {
            lim.enabled = b("alarms", &format!("{}_enabled", kind.key()), lim.enabled);
            lim.limit = f("alarms", kind.key(), lim.limit as f64) as f32;
        }
        let a = &mut c.autostop;
        a.enabled = b("autostop", "enabled", a.enabled);
        a.current_below_ma = f("autostop", "current_below_ma", a.current_below_ma);
        a.below_for_secs = f("autostop", "below_for_secs", a.below_for_secs);
        a.max_minutes = f("autostop", "max_minutes", a.max_minutes);
        a.max_mah = f("autostop", "max_mah", a.max_mah);
        a.max_wh = f("autostop", "max_wh", a.max_wh);
        c
    }

    pub fn save(&self) {
        let kf = glib::KeyFile::new();
        kf.set_boolean("general", "auto_connect", self.auto_connect);
        kf.set_string("general", "last_device", self.last_device.as_deref().unwrap_or(""));
        kf.set_string("general", "last_device_name", self.last_device_name.as_deref().unwrap_or(""));
        kf.set_string("general", "last_meter", self.last_meter.as_deref().unwrap_or(""));
        kf.set_string("general", "last_meter_name", self.last_meter_name.as_deref().unwrap_or(""));
        kf.set_boolean("general", "keep_awake", self.keep_awake);
        kf.set_double("general", "history_minutes", self.history_minutes as f64);
        kf.set_boolean("general", "show_all_devices", self.show_all_devices);
        kf.set_double("graph", "window_secs", self.graph_window_secs as f64);
        kf.set_string("graph", "lanes", &self.graph_lanes.join(","));
        kf.set_boolean("graph", "has_meter_lane", true);
        kf.set_boolean("alarms", "sound", self.alarm_sound);
        kf.set_boolean("alarms", "notify", self.alarm_notify);
        for (kind, lim) in &self.alarms {
            kf.set_boolean("alarms", &format!("{}_enabled", kind.key()), lim.enabled);
            kf.set_double("alarms", kind.key(), lim.limit as f64);
        }
        let a = &self.autostop;
        kf.set_boolean("autostop", "enabled", a.enabled);
        kf.set_double("autostop", "current_below_ma", a.current_below_ma);
        kf.set_double("autostop", "below_for_secs", a.below_for_secs);
        kf.set_double("autostop", "max_minutes", a.max_minutes);
        kf.set_double("autostop", "max_mah", a.max_mah);
        kf.set_double("autostop", "max_wh", a.max_wh);

        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = kf.save_to_file(&p) {
            log::warn!("Could not save settings to {}: {e}", p.display());
        }
    }

    /// The remembered device of a kind, for reconnecting on start.
    pub fn last(&self, kind: Kind) -> (Option<&String>, Option<&String>) {
        match kind {
            Kind::Tester => (self.last_device.as_ref(), self.last_device_name.as_ref()),
            Kind::Meter => (self.last_meter.as_ref(), self.last_meter_name.as_ref()),
        }
    }

    pub fn set_last(&mut self, kind: Kind, address: Option<String>, name: Option<String>) {
        let (a, n) = match kind {
            Kind::Tester => (&mut self.last_device, &mut self.last_device_name),
            Kind::Meter => (&mut self.last_meter, &mut self.last_meter_name),
        };
        *a = address;
        *n = name;
    }

    pub fn alarm(&self, kind: AlarmKind) -> AlarmLimit {
        self.alarms
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, l)| *l)
            .unwrap_or(AlarmLimit { enabled: false, limit: 0.0 })
    }

    pub fn set_alarm(&mut self, kind: AlarmKind, lim: AlarmLimit) {
        if let Some(entry) = self.alarms.iter_mut().find(|(k, _)| *k == kind) {
            entry.1 = lim;
        } else {
            self.alarms.push((kind, lim));
        }
    }
}
