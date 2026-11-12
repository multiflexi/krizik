//! Measurement history, statistics, alarms and auto-stop logic.
//! Independent of GTK so it can be unit tested.

use std::collections::VecDeque;
use std::time::Instant;

use chrono::{DateTime, Local};

use crate::ble::Kind;
use crate::owon::{Function, Reading};
use crate::protocol::{RawSample, RealtimeBlock};

/// Readings further apart than this are not joined in the graph.
const METER_GAP_SECS: f64 = 2.5;

/// A sample placed on the session time line.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Sample {
    /// Seconds since the session started.
    pub t: f64,
    pub v: f32,
    pub i: f32,
    pub p: f32,
    pub dp: f32,
    pub dm: f32,
    pub cc1: f32,
    pub cc2: f32,
    /// The previous sample is not contiguous (reconnect / gap).
    pub gap: bool,
}

impl Sample {
    pub fn from_raw(t: f64, r: &RawSample, gap: bool) -> Self {
        Sample {
            t,
            v: r.voltage,
            i: r.current,
            p: r.voltage * r.current,
            dp: r.d_plus,
            dm: r.d_minus,
            cc1: r.cc1,
            cc2: r.cc2,
            gap,
        }
    }

    /// Load resistance in ohms, if the current is large enough to be meaningful.
    pub fn resistance(&self) -> Option<f32> {
        (self.i.abs() >= 0.001).then(|| self.v / self.i.abs())
    }
}

/// A multimeter reading placed on the session time line.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct MeterPoint {
    /// Seconds since the session started.
    pub t: f64,
    pub reading: Reading,
    /// The previous reading is not contiguous (reconnect, pause in the data).
    pub gap: bool,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct MinMax {
    pub min: f32,
    pub max: f32,
    sum: f64,
    pub count: u64,
}

impl MinMax {
    fn add(&mut self, x: f32) {
        if self.count == 0 {
            self.min = x;
            self.max = x;
        } else {
            self.min = self.min.min(x);
            self.max = self.max.max(x);
        }
        self.sum += x as f64;
        self.count += 1;
    }

    pub fn avg(&self) -> Option<f32> {
        (self.count > 0).then(|| (self.sum / self.count as f64) as f32)
    }
}

/// Accumulated statistics; reset independently of the history.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub voltage: MinMax,
    pub current: MinMax,
    pub power: MinMax,
    pub amp_hours: f64,
    pub watt_hours: f64,
    /// Measured time in seconds (sum of sample intervals).
    pub duration: f64,
}

/// Statistics of the multimeter; they start over when the function changes.
#[derive(Clone, Debug, Default)]
pub struct MeterStats {
    pub function: Option<Function>,
    /// Values in the SI base unit.
    pub values: MinMax,
}

impl MeterStats {
    fn add(&mut self, r: &Reading) {
        if self.function != Some(r.function) {
            *self = MeterStats { function: Some(r.function), ..Default::default() };
        }
        if let Some(v) = r.value_si() {
            self.values.add(v as f32);
        }
    }
}

impl Stats {
    fn add(&mut self, s: &Sample, dt: f64) {
        self.voltage.add(s.v);
        self.current.add(s.i);
        self.power.add(s.p);
        self.amp_hours += s.i.abs() as f64 * dt / 3600.0;
        self.watt_hours += s.p.abs() as f64 * dt / 3600.0;
        self.duration += dt;
    }
}

pub struct Session {
    pub history: VecDeque<Sample>,
    pub max_history_secs: f64,
    pub stats: Stats,
    /// Statistics are accumulated only while this is true.
    pub accumulating: bool,
    pub started: Instant,
    pub started_wall: DateTime<Local>,
    pub last_block: Option<RealtimeBlock>,
    /// Average of the most recent block, used for the big readouts.
    pub latest: Option<Sample>,
    /// Multimeter readings, on the same time line as `history`.
    pub meter: VecDeque<MeterPoint>,
    pub meter_stats: MeterStats,
    cursor: Option<f64>,
    pending_gap: bool,
    meter_pending_gap: bool,
}

impl Session {
    pub fn new(max_history_secs: f64) -> Self {
        Session {
            history: VecDeque::new(),
            max_history_secs,
            stats: Stats::default(),
            accumulating: true,
            started: Instant::now(),
            started_wall: Local::now(),
            last_block: None,
            latest: None,
            meter: VecDeque::new(),
            meter_stats: MeterStats::default(),
            cursor: None,
            pending_gap: false,
            meter_pending_gap: false,
        }
    }

    /// Marks a discontinuity in one device's data (e.g. after a reconnect).
    pub fn mark_gap(&mut self, kind: Kind) {
        match kind {
            Kind::Tester => self.pending_gap = true,
            Kind::Meter => self.meter_pending_gap = true,
        }
    }

    /// Forgets one device's data, e.g. when a different device of that kind
    /// is connected.  The time line and the other device's data stay.
    pub fn reset(&mut self, kind: Kind) {
        match kind {
            Kind::Tester => {
                self.history.clear();
                self.stats = Stats::default();
                self.last_block = None;
                self.latest = None;
                self.cursor = None;
                self.pending_gap = false;
            }
            Kind::Meter => {
                self.meter.clear();
                self.meter_stats = MeterStats::default();
                self.meter_pending_gap = false;
            }
        }
    }

    /// Adds a multimeter reading received at `at`.
    pub fn push_meter(&mut self, reading: Reading, at: Instant) -> MeterPoint {
        let mut t = at.saturating_duration_since(self.started).as_secs_f64();
        let gap = match self.meter.back() {
            Some(prev) => {
                t = t.max(prev.t);
                self.meter_pending_gap || t - prev.t > METER_GAP_SECS || prev.reading.function != reading.function
            }
            None => true,
        };
        self.meter_pending_gap = false;
        if self.accumulating {
            self.meter_stats.add(&reading);
        }
        let point = MeterPoint { t, reading, gap };
        self.meter.push_back(point);
        self.trim();
        point
    }

    /// Adds a realtime block received at `now` and returns the new samples.
    ///
    /// Samples are spaced by the tester's own interval, which is far more
    /// accurate than Bluetooth arrival times; the time line is re-synchronised
    /// with the wall clock only after gaps.
    pub fn push_block(&mut self, block: &RealtimeBlock, now: Instant) -> Vec<Sample> {
        let n = block.samples.len();
        if n == 0 {
            return Vec::new();
        }
        let dt = block.interval_ms.max(1) as f64 / 1000.0;
        let now_t = now.saturating_duration_since(self.started).as_secs_f64();
        let span = n as f64 * dt;
        let mut gap = self.pending_gap;
        let start = match self.cursor {
            Some(c) if !gap && (now_t - (c + span)).abs() <= (3.0 * span).max(1.5) => c,
            Some(_) => {
                gap = true;
                now_t - span
            }
            None => now_t - span,
        };
        self.pending_gap = false;

        let mut out = Vec::with_capacity(n);
        for (k, raw) in block.samples.iter().enumerate() {
            let s = Sample::from_raw(start + (k + 1) as f64 * dt, raw, gap && k == 0);
            if self.accumulating {
                self.stats.add(&s, dt);
            }
            out.push(s);
        }
        self.cursor = out.last().map(|s| s.t);
        self.history.extend(out.iter().copied());
        self.trim();
        self.latest = Some(average(&out));
        self.last_block = Some(block.clone());
        out
    }

    fn trim(&mut self) {
        if let Some(last) = self.latest_t() {
            let limit = last - self.max_history_secs;
            while self.history.front().is_some_and(|s| s.t < limit) {
                self.history.pop_front();
            }
            while self.meter.front().is_some_and(|p| p.t < limit) {
                self.meter.pop_front();
            }
        }
    }

    pub fn set_max_history_secs(&mut self, secs: f64) {
        self.max_history_secs = secs.max(10.0);
        self.trim();
    }

    pub fn reset_stats(&mut self) {
        self.stats = Stats::default();
        self.meter_stats = MeterStats::default();
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
        self.meter.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.history.is_empty() && self.meter.is_empty()
    }

    pub fn wall_time(&self, t: f64) -> DateTime<Local> {
        self.started_wall + chrono::Duration::milliseconds((t * 1000.0) as i64)
    }

    /// Time of the newest data of either device.
    pub fn latest_t(&self) -> Option<f64> {
        match (self.history.back().map(|s| s.t), self.meter.back().map(|p| p.t)) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    /// Time of the oldest data of either device.
    pub fn first_t(&self) -> Option<f64> {
        match (self.history.front().map(|s| s.t), self.meter.front().map(|p| p.t)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

fn average(samples: &[Sample]) -> Sample {
    let n = samples.len() as f32;
    let mut a =
        Sample { t: samples.last().map_or(0.0, |s| s.t), ..Sample::from_raw(0.0, &RawSample::default(), false) };
    for s in samples {
        a.v += s.v / n;
        a.i += s.i / n;
        a.p += s.p / n;
        a.dp += s.dp / n;
        a.dm += s.dm / n;
        a.cc1 += s.cc1 / n;
        a.cc2 += s.cc2 / n;
    }
    a
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum AlarmKind {
    OverVoltage,
    UnderVoltage,
    OverCurrent,
    OverPower,
}

impl AlarmKind {
    pub const ALL: [AlarmKind; 4] =
        [AlarmKind::OverVoltage, AlarmKind::UnderVoltage, AlarmKind::OverCurrent, AlarmKind::OverPower];

    pub fn title(self) -> &'static str {
        match self {
            AlarmKind::OverVoltage => "Over-voltage",
            AlarmKind::UnderVoltage => "Under-voltage",
            AlarmKind::OverCurrent => "Over-current",
            AlarmKind::OverPower => "Over-power",
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            AlarmKind::OverVoltage | AlarmKind::UnderVoltage => "V",
            AlarmKind::OverCurrent => "A",
            AlarmKind::OverPower => "W",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            AlarmKind::OverVoltage => "over_voltage",
            AlarmKind::UnderVoltage => "under_voltage",
            AlarmKind::OverCurrent => "over_current",
            AlarmKind::OverPower => "over_power",
        }
    }

    fn value(self, s: &Sample) -> f32 {
        match self {
            AlarmKind::OverVoltage | AlarmKind::UnderVoltage => s.v,
            AlarmKind::OverCurrent => s.i.abs(),
            AlarmKind::OverPower => s.p.abs(),
        }
    }

    fn violated(self, value: f32, limit: f32) -> bool {
        match self {
            AlarmKind::UnderVoltage => value < limit,
            _ => value > limit,
        }
    }

    /// Value must come back past the limit by this much to re-arm.
    fn hysteresis(self, limit: f32) -> f32 {
        (limit.abs() * 0.01).max(0.005)
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct AlarmLimit {
    pub enabled: bool,
    pub limit: f32,
}

#[derive(Clone, Debug, Default)]
pub struct Alarms {
    active: Vec<AlarmKind>,
}

impl Alarms {
    /// Returns alarms that became active with this sample, with the value.
    pub fn check(&mut self, limits: &[(AlarmKind, AlarmLimit)], s: &Sample) -> Vec<(AlarmKind, f32)> {
        let mut fired = Vec::new();
        for &(kind, lim) in limits {
            let value = kind.value(s);
            let is_active = self.active.contains(&kind);
            if !lim.enabled {
                self.active.retain(|k| *k != kind);
                continue;
            }
            if !is_active && kind.violated(value, lim.limit) {
                self.active.push(kind);
                fired.push((kind, value));
            } else if is_active {
                let h = kind.hysteresis(lim.limit);
                let cleared = match kind {
                    AlarmKind::UnderVoltage => value >= lim.limit + h,
                    _ => value <= lim.limit - h,
                };
                if cleared {
                    self.active.retain(|k| *k != kind);
                }
            }
        }
        fired
    }

    pub fn clear(&mut self) {
        self.active.clear();
    }
}

/// Stops the statistics (e.g. when charging finished).  A limit of 0 disables
/// that condition.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct AutoStopConfig {
    pub enabled: bool,
    pub current_below_ma: f64,
    pub below_for_secs: f64,
    pub max_minutes: f64,
    pub max_mah: f64,
    pub max_wh: f64,
}

impl Default for AutoStopConfig {
    fn default() -> Self {
        AutoStopConfig {
            enabled: false,
            current_below_ma: 50.0,
            below_for_secs: 30.0,
            max_minutes: 0.0,
            max_mah: 0.0,
            max_wh: 0.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum StopReason {
    CurrentBelow { ma: f64, secs: f64 },
    Duration,
    Capacity,
    Energy,
}

impl StopReason {
    pub fn describe(&self) -> String {
        match self {
            StopReason::CurrentBelow { ma, secs } => {
                format!("current stayed below {ma:.0} mA for {secs:.0} s")
            }
            StopReason::Duration => "time limit reached".into(),
            StopReason::Capacity => "capacity limit reached".into(),
            StopReason::Energy => "energy limit reached".into(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct AutoStop {
    armed: bool,
    below_since: Option<f64>,
}

impl AutoStop {
    pub fn reset(&mut self) {
        *self = AutoStop::default();
    }

    pub fn check(&mut self, cfg: &AutoStopConfig, stats: &Stats, s: &Sample) -> Option<StopReason> {
        if !cfg.enabled {
            self.reset();
            return None;
        }
        if cfg.max_minutes > 0.0 && stats.duration >= cfg.max_minutes * 60.0 {
            return Some(StopReason::Duration);
        }
        if cfg.max_mah > 0.0 && stats.amp_hours * 1000.0 >= cfg.max_mah {
            return Some(StopReason::Capacity);
        }
        if cfg.max_wh > 0.0 && stats.watt_hours >= cfg.max_wh {
            return Some(StopReason::Energy);
        }
        if cfg.current_below_ma > 0.0 {
            let ma = s.i.abs() as f64 * 1000.0;
            if ma >= cfg.current_below_ma {
                // Only start watching once a load has actually been seen.
                self.armed = true;
                self.below_since = None;
            } else if self.armed {
                let since = *self.below_since.get_or_insert(s.t);
                if s.t - since >= cfg.below_for_secs {
                    return Some(StopReason::CurrentBelow { ma: cfg.current_below_ma, secs: cfg.below_for_secs });
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ProtocolInfo;
    use std::time::Duration;

    fn block(v: f32, i: f32, interval_ms: u16) -> RealtimeBlock {
        RealtimeBlock {
            protocol: ProtocolInfo::default(),
            power_bank_mode: false,
            interval_ms,
            samples: vec![RawSample { voltage: v, current: i, ..Default::default() }; 10],
            status: 0,
            temperature_c: 25,
        }
    }

    #[test]
    fn integrates_capacity_and_energy() {
        let mut s = Session::new(3600.0);
        let t0 = s.started;
        // 1 hour of 5 V / 2 A in 200 ms blocks.
        for k in 1..=18_000u64 {
            s.push_block(&block(5.0, 2.0, 20), t0 + Duration::from_millis(200 * k));
        }
        assert!((s.stats.amp_hours - 2.0).abs() < 1e-3, "{}", s.stats.amp_hours);
        assert!((s.stats.watt_hours - 10.0).abs() < 1e-2);
        assert!((s.stats.duration - 3600.0).abs() < 1e-3);
        assert_eq!(s.stats.voltage.avg(), Some(5.0));
        // History is trimmed to an hour.
        assert!(s.history.back().unwrap().t - s.history.front().unwrap().t <= 3600.0);
    }

    #[test]
    fn samples_are_evenly_spaced_and_gaps_resync() {
        let mut s = Session::new(600.0);
        let t0 = s.started;
        let a = s.push_block(&block(5.0, 0.0, 20), t0 + Duration::from_millis(1000));
        let b = s.push_block(&block(5.0, 0.0, 20), t0 + Duration::from_millis(1230));
        assert!((b[0].t - a[9].t - 0.02).abs() < 1e-9);
        assert!(!b[0].gap);
        // 30 s without data → resync and mark the gap.
        let c = s.push_block(&block(5.0, 0.0, 20), t0 + Duration::from_millis(31_000));
        assert!(c[0].gap);
        assert!((c[9].t - 31.0).abs() < 1e-9);
    }

    fn reading(function: Function, raw: i32) -> Reading {
        Reading {
            function,
            prefix: crate::owon::Prefix::None,
            point: crate::owon::Point::Decimals(1),
            negative: false,
            raw,
            flags: Default::default(),
        }
    }

    #[test]
    fn meter_readings_share_the_time_line() {
        let mut s = Session::new(60.0);
        let t0 = s.started;
        s.push_block(&block(5.0, 1.0, 20), t0 + Duration::from_millis(1000));
        let a = s.push_meter(reading(Function::Celsius, 250), t0 + Duration::from_millis(1100));
        assert!(a.gap && (a.t - 1.1).abs() < 1e-9);
        let b = s.push_meter(reading(Function::Celsius, 260), t0 + Duration::from_millis(1400));
        assert!(!b.gap);
        assert_eq!(s.latest_t(), Some(b.t));
        assert!((s.first_t().unwrap() - 0.82).abs() < 1e-9);
        assert_eq!(s.meter_stats.values.count, 2);
        assert!((s.meter_stats.values.max - 26.0).abs() < 1e-6);

        // A new function starts the statistics over and breaks the line.
        let c = s.push_meter(reading(Function::DcVoltage, 50), t0 + Duration::from_millis(1700));
        assert!(c.gap);
        assert_eq!(s.meter_stats.function, Some(Function::DcVoltage));
        assert_eq!(s.meter_stats.values.count, 1);
        // So does a pause, or a reconnect.
        let d = s.push_meter(reading(Function::DcVoltage, 50), t0 + Duration::from_millis(5000));
        assert!(d.gap);
        s.mark_gap(Kind::Meter);
        assert!(s.push_meter(reading(Function::DcVoltage, 50), t0 + Duration::from_millis(5300)).gap);

        // Resetting the meter keeps the tester's data, and vice versa.
        s.reset(Kind::Meter);
        assert!(s.meter.is_empty() && !s.history.is_empty());
        s.push_meter(reading(Function::DcVoltage, 50), t0 + Duration::from_millis(5600));
        s.reset(Kind::Tester);
        assert!(s.history.is_empty() && s.meter.len() == 1);

        // Both are trimmed to the history length.
        s.push_meter(reading(Function::DcVoltage, 50), t0 + Duration::from_secs(120));
        assert_eq!(s.meter.len(), 1);
    }

    #[test]
    fn alarms_fire_once_with_hysteresis() {
        let limits = [(AlarmKind::OverVoltage, AlarmLimit { enabled: true, limit: 5.5 })];
        let mut a = Alarms::default();
        let mk = |v| Sample::from_raw(0.0, &RawSample { voltage: v, ..Default::default() }, false);
        assert!(a.check(&limits, &mk(5.0)).is_empty());
        assert_eq!(a.check(&limits, &mk(9.0)).len(), 1);
        assert!(a.check(&limits, &mk(9.1)).is_empty());
        assert!(a.check(&limits, &mk(5.49)).is_empty(), "still within hysteresis");
        assert!(a.check(&limits, &mk(5.0)).is_empty());
        assert_eq!(a.check(&limits, &mk(9.0)).len(), 1, "re-armed");
    }

    #[test]
    fn autostop_waits_for_load_then_low_current() {
        let cfg = AutoStopConfig { enabled: true, current_below_ma: 100.0, below_for_secs: 10.0, ..Default::default() };
        let mut st = AutoStop::default();
        let stats = Stats::default();
        let mk = |t, i| Sample::from_raw(t, &RawSample { voltage: 5.0, current: i, ..Default::default() }, false);
        // Nothing connected yet: never stops.
        assert_eq!(st.check(&cfg, &stats, &mk(0.0, 0.0)), None);
        assert_eq!(st.check(&cfg, &stats, &mk(20.0, 0.0)), None);
        // Charging, then the current drops.
        assert_eq!(st.check(&cfg, &stats, &mk(21.0, 1.0)), None);
        assert_eq!(st.check(&cfg, &stats, &mk(22.0, 0.05)), None);
        assert_eq!(st.check(&cfg, &stats, &mk(31.0, 0.05)), None);
        assert!(matches!(st.check(&cfg, &stats, &mk(32.5, 0.05)), Some(StopReason::CurrentBelow { .. })));
    }
}
