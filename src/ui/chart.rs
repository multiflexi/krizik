//! Real-time graph: stacked lanes sharing a time axis, each auto-scaled.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{cairo, glib, pango};

use crate::owon::{Function, Prefix};
use crate::session::{MeterPoint, Sample, Session};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Lane {
    Voltage,
    Current,
    Power,
    DataLines,
    CcLines,
    /// The multimeter's reading.
    Meter,
}

#[derive(Copy, Clone)]
pub struct Series {
    pub name: &'static str,
    pub color: (f64, f64, f64),
    pub get: fn(&Sample) -> f32,
}

const fn rgb(hex: u32) -> (f64, f64, f64) {
    (((hex >> 16) & 0xff) as f64 / 255.0, ((hex >> 8) & 0xff) as f64 / 255.0, (hex & 0xff) as f64 / 255.0)
}

fn get_v(s: &Sample) -> f32 {
    s.v
}
fn get_i(s: &Sample) -> f32 {
    s.i
}
fn get_p(s: &Sample) -> f32 {
    s.p
}
fn get_dp(s: &Sample) -> f32 {
    s.dp
}
fn get_dm(s: &Sample) -> f32 {
    s.dm
}
fn get_cc1(s: &Sample) -> f32 {
    s.cc1
}
fn get_cc2(s: &Sample) -> f32 {
    s.cc2
}

const VOLTAGE: [Series; 1] = [Series { name: "VBUS", color: rgb(0x3584e4), get: get_v }];
const CURRENT: [Series; 1] = [Series { name: "I", color: rgb(0xe5a50a), get: get_i }];
const POWER: [Series; 1] = [Series { name: "P", color: rgb(0xc061cb), get: get_p }];
const DATA: [Series; 2] = [
    Series { name: "D+", color: rgb(0x2ec27e), get: get_dp },
    Series { name: "D−", color: rgb(0x1c71d8), get: get_dm },
];
const CC: [Series; 2] = [
    Series { name: "CC1", color: rgb(0xff7800), get: get_cc1 },
    Series { name: "CC2", color: rgb(0xe01b24), get: get_cc2 },
];
/// Colour of the multimeter lane (also in style.css).
const METER_COLOR: (f64, f64, f64) = rgb(0xb5835a);

impl Lane {
    pub const ALL: [Lane; 6] = [Lane::Voltage, Lane::Current, Lane::Power, Lane::DataLines, Lane::CcLines, Lane::Meter];

    pub fn key(self) -> &'static str {
        match self {
            Lane::Voltage => "voltage",
            Lane::Current => "current",
            Lane::Power => "power",
            Lane::DataLines => "data",
            Lane::CcLines => "cc",
            Lane::Meter => "meter",
        }
    }

    pub fn from_key(key: &str) -> Option<Lane> {
        Lane::ALL.into_iter().find(|l| l.key() == key)
    }

    pub fn short(self) -> &'static str {
        match self {
            Lane::Voltage => "V",
            Lane::Current => "A",
            Lane::Power => "W",
            Lane::DataLines => "D±",
            Lane::CcLines => "CC",
            Lane::Meter => "DMM",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Lane::Voltage => "Voltage",
            Lane::Current => "Current",
            Lane::Power => "Power",
            Lane::DataLines => "D+ and D−",
            Lane::CcLines => "CC1 and CC2",
            Lane::Meter => "Multimeter",
        }
    }

    fn unit(self) -> &'static str {
        match self {
            Lane::Voltage | Lane::DataLines | Lane::CcLines => "V",
            Lane::Current => "A",
            Lane::Power => "W",
            Lane::Meter => "",
        }
    }

    pub fn css_class(self) -> &'static str {
        match self {
            Lane::Voltage => "series-voltage",
            Lane::Current => "series-current",
            Lane::Power => "series-power",
            Lane::DataLines => "series-data",
            Lane::CcLines => "series-cc",
            Lane::Meter => "series-meter",
        }
    }

    pub fn series(self) -> &'static [Series] {
        match self {
            Lane::Voltage => &VOLTAGE,
            Lane::Current => &CURRENT,
            Lane::Power => &POWER,
            Lane::DataLines => &DATA,
            Lane::CcLines => &CC,
            Lane::Meter => &[],
        }
    }

    fn min_span(self) -> f64 {
        match self {
            Lane::Current => 0.02,
            _ => 0.2,
        }
    }

    fn decimals(self) -> usize {
        match self {
            Lane::Current => 3,
            Lane::Power => 2,
            Lane::Meter => 4,
            _ => 3,
        }
    }
}

pub struct ChartState {
    pub lanes: Vec<Lane>,
    /// Visible time span in seconds; 0 shows the whole history.
    pub window_secs: f64,
    /// End of the visible range while paused.
    pub frozen_end: Option<f64>,
    cursor_x: Option<f64>,
}

#[derive(Clone)]
pub struct Chart {
    pub area: gtk::DrawingArea,
    pub state: Rc<RefCell<ChartState>>,
}

impl Chart {
    pub fn new(session: Rc<RefCell<Session>>, lanes: Vec<Lane>, window_secs: f64) -> Chart {
        let area = gtk::DrawingArea::builder().hexpand(true).vexpand(true).content_height(220).build();
        let state = Rc::new(RefCell::new(ChartState { lanes, window_secs, frozen_end: None, cursor_x: None }));

        area.set_draw_func(glib::clone!(
            #[strong]
            state,
            move |area, cr, w, h| {
                let st = state.borrow();
                let session = session.borrow();
                if let Err(e) = draw(area, cr, w as f64, h as f64, &st, &session) {
                    log::warn!("Chart drawing failed: {e}");
                }
            }
        ));

        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(glib::clone!(
            #[strong]
            state,
            move |c, x, _| {
                state.borrow_mut().cursor_x = Some(x);
                if let Some(w) = c.widget() {
                    w.queue_draw();
                }
            }
        ));
        motion.connect_leave(glib::clone!(
            #[strong]
            state,
            move |c| {
                state.borrow_mut().cursor_x = None;
                if let Some(w) = c.widget() {
                    w.queue_draw();
                }
            }
        ));
        area.add_controller(motion);

        // Touch: drag a finger to read values; the cursor stays afterwards.
        let drag = gtk::GestureDrag::new();
        drag.set_touch_only(true);
        drag.connect_drag_begin(glib::clone!(
            #[strong]
            state,
            move |g, x, _| {
                state.borrow_mut().cursor_x = Some(x);
                if let Some(w) = g.widget() {
                    w.queue_draw();
                }
            }
        ));
        drag.connect_drag_update(glib::clone!(
            #[strong]
            state,
            move |g, dx, _| {
                if let Some((x, _)) = g.start_point() {
                    state.borrow_mut().cursor_x = Some(x + dx);
                    if let Some(w) = g.widget() {
                        w.queue_draw();
                    }
                }
            }
        ));
        area.add_controller(drag);

        Chart { area, state }
    }

    pub fn redraw(&self) {
        self.area.queue_draw();
    }
}

const LEFT: f64 = 58.0;
const RIGHT: f64 = 10.0;
const TOP: f64 = 4.0;
const BOTTOM: f64 = 24.0;
const LANE_GAP: f64 = 12.0;

fn nice_step(raw: f64) -> f64 {
    if raw <= 0.0 || !raw.is_finite() {
        return 1.0;
    }
    let mag = 10f64.powf(raw.log10().floor());
    let f = raw / mag;
    let nice = if f <= 1.0 {
        1.0
    } else if f <= 2.0 {
        2.0
    } else if f <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * mag
}

const TIME_STEPS: [f64; 16] =
    [0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0];

struct Painter<'a> {
    area: &'a gtk::DrawingArea,
    cr: &'a cairo::Context,
    font: pango::FontDescription,
    fg: (f64, f64, f64),
}

impl Painter<'_> {
    fn fg(&self, alpha: f64) {
        self.cr.set_source_rgba(self.fg.0, self.fg.1, self.fg.2, alpha);
    }

    /// Draws text; `ax`/`ay` align the box (0 = left/top, 0.5 = centre, 1 = right/bottom).
    fn text(&self, s: &str, x: f64, y: f64, ax: f64, ay: f64) -> (f64, f64) {
        let layout = self.area.create_pango_layout(Some(s));
        layout.set_font_description(Some(&self.font));
        let (w, h) = layout.pixel_size();
        let (w, h) = (w as f64, h as f64);
        self.cr.move_to((x - ax * w).round(), (y - ay * h).round());
        pangocairo::functions::show_layout(self.cr, &layout);
        (w, h)
    }

    fn text_size(&self, s: &str) -> (f64, f64) {
        let layout = self.area.create_pango_layout(Some(s));
        layout.set_font_description(Some(&self.font));
        let (w, h) = layout.pixel_size();
        (w as f64, h as f64)
    }
}

/// What the multimeter lane shows: one function, scaled to a common prefix.
struct MeterView<'a> {
    points: Vec<&'a MeterPoint>,
    function: Function,
    /// Values are divided by this (e.g. 1e-3 to show mV).
    scale: f64,
    unit: String,
    min_span: f64,
}

impl<'a> MeterView<'a> {
    fn new(visible: Vec<&'a MeterPoint>, latest: &MeterPoint) -> Self {
        // Show the function of the newest visible reading.
        let function = visible.last().map_or(latest.reading.function, |p| p.reading.function);
        let base = function.base_unit();
        let max = visible
            .iter()
            .filter(|p| p.reading.function == function)
            .filter_map(|p| p.reading.value_si())
            .fold(0.0f64, |m, v| m.max(v.abs()));
        let resolution =
            visible.iter().rev().find(|p| p.reading.function == function).map_or(1.0, |p| p.reading.resolution_si());
        let (scale, symbol) = if function.uses_prefix() {
            let exp = if max > 0.0 { ((max.log10() / 3.0).floor() as i32 * 3).clamp(-12, 9) } else { 0 };
            let prefix = [
                Prefix::Pico,
                Prefix::Nano,
                Prefix::Micro,
                Prefix::Milli,
                Prefix::None,
                Prefix::Kilo,
                Prefix::Mega,
                Prefix::Giga,
            ]
            .into_iter()
            .find(|p| p.exponent() == exp)
            .unwrap_or(Prefix::None);
            (10f64.powi(exp), prefix.symbol())
        } else {
            (1.0, "")
        };
        MeterView {
            points: visible,
            function,
            scale,
            unit: format!("{symbol}{base}"),
            min_span: 10.0 * resolution / scale,
        }
    }

    /// (t, value, gap): readings of other functions and overloads break the line.
    fn plot_points(&self) -> impl Iterator<Item = (f64, f64, bool)> + '_ {
        let mut skipped = false;
        self.points.iter().filter_map(move |p| {
            if p.reading.function != self.function {
                skipped = true;
                return None;
            }
            let v = p.reading.value_si().map_or(f64::NAN, |v| v / self.scale);
            let gap = p.gap || std::mem::take(&mut skipped);
            Some((p.t, v, gap))
        })
    }
}

fn draw(
    area: &gtk::DrawingArea,
    cr: &cairo::Context,
    w: f64,
    h: f64,
    st: &ChartState,
    session: &Session,
) -> Result<(), cairo::Error> {
    let c = area.color();
    let mut font = area.pango_context().font_description().unwrap_or_default();
    font.set_size((font.size() as f64 * 0.82) as i32);
    let p = Painter { area, cr, font, fg: (c.red() as f64, c.green() as f64, c.blue() as f64) };

    let hist = &session.history;
    let meter = &session.meter;
    let plot_w = (w - LEFT - RIGHT).max(10.0);
    // Lanes of a device that has sent nothing are left out.
    let lanes: Vec<Lane> = st
        .lanes
        .iter()
        .copied()
        .filter(|l| if *l == Lane::Meter { !meter.is_empty() } else { !hist.is_empty() })
        .collect();
    let (Some(first_t), Some(last_t), false) = (session.first_t(), session.latest_t(), lanes.is_empty()) else {
        p.fg(0.55);
        let msg = if st.lanes.is_empty() {
            "Select at least one series"
        } else if session.is_empty() {
            "Waiting for data…"
        } else {
            "No data for the selected series"
        };
        p.text(msg, w / 2.0, h / 2.0, 0.5, 0.5);
        return Ok(());
    };

    let t_end = st.frozen_end.unwrap_or(last_t);
    // Until enough data exists, stretch what there is over the full width.
    let first_t = first_t.min(t_end - 1.0);
    let t_start = if st.window_secs > 0.0 { (t_end - st.window_secs).max(first_t) } else { first_t };
    let span = (t_end - t_start).max(1e-6);
    let i0 = hist.partition_point(|s| s.t < t_start).saturating_sub(1);
    let i1 = hist.partition_point(|s| s.t <= t_end);
    let visible: Vec<&Sample> = hist.range(i0..i1).collect();
    let m0 = meter.partition_point(|s| s.t < t_start).saturating_sub(1);
    let m1 = meter.partition_point(|s| s.t <= t_end);
    let meter_view = meter.back().map(|latest| MeterView::new(meter.range(m0..m1).collect(), latest));
    let x_of = |t: f64| LEFT + (t - t_start) / span * plot_w;

    let n = lanes.len() as f64;
    let lane_h = ((h - TOP - BOTTOM - LANE_GAP * (n - 1.0)) / n).max(20.0);

    // Cursor position → nearest tester sample and nearest meter reading.
    let cursor_t =
        st.cursor_x.filter(|x| *x >= LEFT && *x <= LEFT + plot_w).map(|x| t_start + (x - LEFT) / plot_w * span);
    let tester_cursor = cursor_t.and_then(|t| nearest(&visible, |s| s.t, t)).copied();
    let meter_cursor = cursor_t.and_then(|t| {
        let pts = &meter_view.as_ref()?.points;
        // Readings arrive a few times a second; don't snap across gaps.
        nearest(pts, |p| p.t, t).copied().filter(|p| (p.t - t).abs() <= (span / plot_w * 12.0).max(1.5))
    });
    let cursor_line_t =
        if lanes.iter().any(|l| *l != Lane::Meter) { tester_cursor.map(|s| s.t) } else { meter_cursor.map(|p| p.t) };

    for (li, lane) in lanes.iter().enumerate() {
        let top = TOP + li as f64 * (lane_h + LANE_GAP);
        let bottom = top + lane_h;
        let meter_lane = if *lane == Lane::Meter { meter_view.as_ref() } else { None };

        // Range over visible data.
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        let mut extend = |v: f64| {
            if v.is_finite() {
                lo = lo.min(v);
                hi = hi.max(v);
            }
        };
        match meter_lane {
            Some(m) => m.plot_points().for_each(|(_, v, _)| extend(v)),
            None => {
                for s in &visible {
                    for ser in lane.series() {
                        extend((ser.get)(s) as f64);
                    }
                }
            }
        }
        if !lo.is_finite() {
            lo = 0.0;
            hi = 1.0;
        }
        let min_span = meter_lane.map_or(lane.min_span(), |m| m.min_span);
        if hi - lo < min_span {
            let mid = (hi + lo) / 2.0;
            lo = mid - min_span / 2.0;
            hi = mid + min_span / 2.0;
        }
        let ticks = ((lane_h / 28.0).floor()).clamp(1.0, 5.0);
        let step = nice_step((hi - lo) / ticks);
        let lo = (lo / step).floor() * step;
        let hi = ((hi / step).ceil() * step).max(lo + step);
        let y_of = |v: f64| bottom - (v - lo) / (hi - lo) * lane_h;

        // Frame and grid.
        p.fg(0.05);
        cr.rectangle(LEFT, top, plot_w, lane_h);
        cr.fill()?;
        cr.set_line_width(1.0);
        let decimals = if step >= 1.0 { 0 } else { (-step.log10().floor()) as usize }.min(lane.decimals());
        let mut v = lo;
        while v <= hi + step * 0.01 {
            let y = y_of(v).round() + 0.5;
            p.fg(0.12);
            cr.move_to(LEFT, y);
            cr.line_to(LEFT + plot_w, y);
            cr.stroke()?;
            if lane_h >= 40.0 || (v - lo).abs() < 1e-9 || (v - hi).abs() < 1e-9 {
                // Keep edge labels inside the lane so neighbours don't collide.
                let label = format!("{v:.decimals$}");
                let half = p.text_size(&label).1 / 2.0;
                let ly = y.clamp(top + half, bottom - half);
                p.fg(0.65);
                p.text(&label, LEFT - 6.0, ly, 1.0, 0.5);
            }
            v += step;
        }

        // Series, decimated to min/max per pixel column.
        cr.save()?;
        cr.rectangle(LEFT, top, plot_w, lane_h);
        cr.clip();
        cr.set_line_width(1.6);
        cr.set_line_join(cairo::LineJoin::Round);
        match meter_lane {
            Some(m) => {
                let (r, g, b) = METER_COLOR;
                cr.set_source_rgb(r, g, b);
                let count = m.points.len();
                plot_points(cr, m.plot_points(), count, &x_of, &y_of, plot_w)?;
            }
            None => {
                for ser in lane.series() {
                    let (r, g, b) = ser.color;
                    cr.set_source_rgb(r, g, b);
                    let points = visible.iter().map(|s| (s.t, (ser.get)(s) as f64, s.gap));
                    plot_points(cr, points, visible.len(), &x_of, &y_of, plot_w)?;
                }
            }
        }
        cr.restore()?;

        // Lane title (top-left inside the plot).
        let title = match meter_lane {
            Some(m) if m.unit.is_empty() => format!("{} · {}", lane.title(), m.function.label()),
            Some(m) => format!("{} · {} ({})", lane.title(), m.function.label(), m.unit),
            None => format!("{} ({})", lane.title(), lane.unit()),
        };
        let (tw, th) = p.text_size(&title);
        p.fg(0.08);
        rounded_rect(cr, LEFT + 4.0, top + 3.0, tw + 10.0, th + 2.0, 4.0);
        cr.fill()?;
        p.fg(0.8);
        p.text(&title, LEFT + 9.0, top + 4.0, 0.0, 0.0);

        // Cursor read-out for this lane.
        let readout = match meter_lane {
            Some(_) => {
                meter_cursor.map(|m| format!("{} {}", m.reading.display(), m.reading.unit()).trim_end().to_string())
            }
            None => tester_cursor.map(|s| {
                let parts: Vec<String> = lane
                    .series()
                    .iter()
                    .map(|ser| {
                        let v = (ser.get)(s);
                        if lane.series().len() > 1 {
                            format!("{} {:.*}", ser.name, lane.decimals() + 1, v)
                        } else {
                            format!("{:.*} {}", lane.decimals() + 1, v, lane.unit())
                        }
                    })
                    .collect();
                parts.join("  ")
            }),
        };
        if let Some(txt) = readout {
            let (tw, th) = p.text_size(&txt);
            let x = (LEFT + plot_w - tw - 10.0).max(LEFT + 4.0);
            p.fg(0.85);
            rounded_rect(cr, x - 5.0, top + 3.0, tw + 10.0, th + 2.0, 4.0);
            cr.fill()?;
            // Inverted text colour on the solid box.
            let bgc = if p.fg.0 + p.fg.1 + p.fg.2 > 1.5 { (0.1, 0.1, 0.1) } else { (1.0, 1.0, 1.0) };
            cr.set_source_rgb(bgc.0, bgc.1, bgc.2);
            p.text(&txt, x, top + 4.0, 0.0, 0.0);
        }
    }

    // Time axis.
    let axis_y = h - BOTTOM;
    let min_px = p.text_size("00:00:00").0 + 24.0;
    let tstep = TIME_STEPS.iter().copied().find(|s| s / span * plot_w >= min_px).unwrap_or(7200.0);
    let fmt = if tstep >= 60.0 { "%H:%M" } else { "%H:%M:%S" };
    let mut t = (t_start / tstep).ceil() * tstep;
    while t <= t_end + 1e-9 {
        let x = x_of(t).round() + 0.5;
        p.fg(0.12);
        cr.move_to(x, TOP);
        cr.line_to(x, axis_y);
        cr.stroke()?;
        p.fg(0.65);
        let label = session.wall_time(t).format(fmt).to_string();
        p.text(&label, x, axis_y + 4.0, 0.5, 0.0);
        t += tstep;
    }

    // Cursor line and time.
    if let Some(ct) = cursor_line_t {
        let x = x_of(ct).round() + 0.5;
        p.fg(0.6);
        cr.set_line_width(1.0);
        cr.set_dash(&[3.0, 3.0], 0.0);
        cr.move_to(x, TOP);
        cr.line_to(x, axis_y);
        cr.stroke()?;
        cr.set_dash(&[], 0.0);
        let label = session.wall_time(ct).format("%H:%M:%S%.3f").to_string();
        let (tw, th) = p.text_size(&label);
        let bx = (x - tw / 2.0 - 5.0).clamp(LEFT, LEFT + plot_w - tw - 10.0);
        p.fg(0.85);
        rounded_rect(cr, bx, axis_y + 2.0, tw + 10.0, th + 2.0, 4.0);
        cr.fill()?;
        let bgc = if p.fg.0 + p.fg.1 + p.fg.2 > 1.5 { (0.1, 0.1, 0.1) } else { (1.0, 1.0, 1.0) };
        cr.set_source_rgb(bgc.0, bgc.1, bgc.2);
        p.text(&label, bx + 5.0, axis_y + 3.0, 0.0, 0.0);
    }

    if st.frozen_end.is_some() {
        p.fg(0.55);
        p.text("Paused", LEFT + plot_w - 4.0, h - 2.0, 1.0, 1.0);
    }
    Ok(())
}

/// The item of a time-sorted slice closest to `t`.
fn nearest<T>(items: &[T], time: impl Fn(&T) -> f64, t: f64) -> Option<&T> {
    let k = items.partition_point(|i| time(i) < t);
    [k.checked_sub(1), Some(k)]
        .into_iter()
        .flatten()
        .filter_map(|k| items.get(k))
        .min_by(|a, b| (time(a) - t).abs().total_cmp(&(time(b) - t).abs()))
}

/// Draws (t, value, gap) points; NaN values break the line.
fn plot_points(
    cr: &cairo::Context,
    points: impl Iterator<Item = (f64, f64, bool)>,
    count: usize,
    x_of: &dyn Fn(f64) -> f64,
    y_of: &dyn Fn(f64) -> f64,
    plot_w: f64,
) -> Result<(), cairo::Error> {
    let mut pen_down = false;
    if count as f64 <= plot_w * 2.0 {
        for (t, v, gap) in points {
            if !v.is_finite() {
                pen_down = false;
                continue;
            }
            let (x, y) = (x_of(t), y_of(v));
            if pen_down && !gap {
                cr.line_to(x, y);
            } else {
                cr.move_to(x, y);
                pen_down = true;
            }
        }
        return cr.stroke();
    }
    // One min/max pair per pixel column keeps spikes visible.
    let mut col: Option<(i64, f64, f64, f64, f64, bool)> = None; // (column, first, min, max, last, gap)
    let flush = |c: (i64, f64, f64, f64, f64, bool), pen_down: &mut bool| {
        let (cx, first, mn, mx, last, gap) = c;
        let x = cx as f64 + 0.5;
        if *pen_down && !gap {
            cr.line_to(x, y_of(first));
        } else {
            cr.move_to(x, y_of(first));
            *pen_down = true;
        }
        cr.line_to(x, y_of(mn));
        cr.line_to(x, y_of(mx));
        cr.line_to(x, y_of(last));
    };
    let mut broken = false;
    for (t, v, gap) in points {
        if !v.is_finite() {
            broken = true;
            continue;
        }
        let gap = gap || std::mem::take(&mut broken);
        let cx = x_of(t).floor() as i64;
        match col.as_mut() {
            Some(c) if c.0 == cx => {
                c.2 = c.2.min(v);
                c.3 = c.3.max(v);
                c.4 = v;
                c.5 |= gap;
            }
            _ => {
                if let Some(c) = col.take() {
                    flush(c, &mut pen_down);
                }
                col = Some((cx, v, v, v, v, gap));
            }
        }
    }
    if let Some(c) = col {
        flush(c, &mut pen_down);
    }
    cr.stroke()
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}
