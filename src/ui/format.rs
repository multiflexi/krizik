//! Number formatting for readouts.

pub const DASH: &str = "—";

/// Fixed-point with as many decimals as fit in about five significant digits.
pub fn sig(value: f32, max_decimals: usize) -> String {
    if !value.is_finite() {
        return DASH.into();
    }
    let a = value.abs();
    let decimals = if a < 10.0 {
        max_decimals
    } else if a < 100.0 {
        max_decimals.saturating_sub(1)
    } else if a < 1000.0 {
        max_decimals.saturating_sub(2)
    } else {
        max_decimals.saturating_sub(3)
    };
    format!("{value:.decimals$}")
}

pub fn volts(v: f32) -> String {
    sig(v, 4)
}

pub fn amps(i: f32) -> String {
    sig(i, 4)
}

pub fn watts(p: f32) -> String {
    sig(p, 3)
}

pub fn data_volts(v: f32) -> String {
    if v.is_finite() {
        format!("{v:.3}")
    } else {
        DASH.into()
    }
}

pub fn ohms(r: Option<f32>) -> (String, &'static str) {
    match r {
        None => (DASH.into(), "Ω"),
        Some(r) if r >= 1000.0 => (sig(r / 1000.0, 3), "kΩ"),
        Some(r) => (sig(r, 3), "Ω"),
    }
}

/// A value in an SI base unit with a fitting prefix, e.g. 0.001234 V → "1.234 mV".
pub fn si(value: f64, unit: &str, use_prefix: bool) -> String {
    if !value.is_finite() {
        return DASH.into();
    }
    let (scaled, prefix) = if use_prefix && value != 0.0 {
        let exp = ((value.abs().log10() / 3.0).floor() as i32 * 3).clamp(-12, 9);
        let prefix = match exp {
            -12 => "p",
            -9 => "n",
            -6 => "µ",
            -3 => "m",
            3 => "k",
            6 => "M",
            9 => "G",
            _ => "",
        };
        (value / 10f64.powi(exp), prefix)
    } else {
        (value, "")
    };
    format!("{} {prefix}{unit}", sig(scaled as f32, 3)).trim_end().to_string()
}

pub fn duration(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

pub fn capacity(amp_hours: f64) -> String {
    let mah = amp_hours * 1000.0;
    if mah < 10_000.0 {
        format!("{mah:.1} mAh")
    } else {
        format!("{amp_hours:.3} Ah")
    }
}

pub fn energy(wh: f64) -> String {
    if wh < 10.0 {
        format!("{wh:.4} Wh")
    } else if wh < 1000.0 {
        format!("{wh:.3} Wh")
    } else {
        format!("{:.3} kWh", wh / 1000.0)
    }
}

/// Hides the tester's "not available" placeholders.
pub fn protocol_text(parts: &[&str]) -> String {
    let joined = parts
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !s.contains("NA-") && *s != "Protocols")
        .collect::<Vec<_>>()
        .join(" · ");
    if joined.is_empty() {
        DASH.into()
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn significant_digits() {
        assert_eq!(volts(5.01234), "5.0123");
        assert_eq!(volts(20.01234), "20.012");
        assert_eq!(watts(65.4321), "65.43");
        assert_eq!(watts(140.0), "140.0");
        assert_eq!(duration(3725.0), "1:02:05");
        assert_eq!(duration(65.0), "01:05");
        assert_eq!(capacity(1.2345), "1234.5 mAh");
        assert_eq!(protocol_text(&["Protocols", ""]), DASH);
        assert_eq!(protocol_text(&["PD3.0", "65W"]), "PD3.0 · 65W");
        assert_eq!(protocol_text(&["---NA---", ""]), DASH);
        assert_eq!(si(-0.001234, "V", true), "-1.234 mV");
        assert_eq!(si(4702.0, "Ω", true), "4.702 kΩ");
        assert_eq!(si(0.0, "A", true), "0.000 A");
        assert_eq!(si(37.9, "%", false), "37.90 %");
        assert_eq!(si(150.0, "", false), "150.0");
    }
}
