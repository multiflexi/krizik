//! CSV recording and export.
//!
//! The tester and the multimeter deliver data independently, so one file
//! holds both: every tester sample gives a row with the latest multimeter
//! reading next to it, and multimeter readings get rows of their own while
//! no tester data arrives.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};

use crate::session::{MeterPoint, Sample, Session};

const HEADER: &str = "time_s,timestamp,voltage_V,current_A,power_W,dplus_V,dminus_V,cc1_V,cc2_V,\
meter_value,meter_unit,meter_function,meter_display,meter_flags\n";

/// A meter reading is repeated next to tester samples for at most this long.
const METER_HOLD_SECS: f64 = 3.0;
/// Meter readings get their own rows once the tester has been silent this long.
const TESTER_QUIET_SECS: f64 = 1.0;

fn write_row(
    w: &mut impl Write,
    t: f64,
    wall: DateTime<Local>,
    s: Option<&Sample>,
    m: Option<&MeterPoint>,
) -> io::Result<()> {
    write!(w, "{t:.3},{}", wall.format("%Y-%m-%dT%H:%M:%S%.3f%:z"))?;
    match s {
        Some(s) => write!(w, ",{:.5},{:.5},{:.5},{:.3},{:.3},{:.3},{:.3}", s.v, s.i, s.p, s.dp, s.dm, s.cc1, s.cc2)?,
        None => write!(w, ",,,,,,,")?,
    }
    match m {
        Some(m) => {
            let r = &m.reading;
            let shown = format!("{} {}", r.display(), r.unit());
            writeln!(
                w,
                ",{},{},{},{},{}",
                r.value_si_text().unwrap_or_default(),
                r.function.base_unit(),
                r.function.key(),
                shown.trim_end(),
                r.flags.names().join(" ")
            )
        }
        None => writeln!(w, ",,,,,"),
    }
}

/// Decides which rows to write as data of both devices comes in.
#[derive(Debug, Default)]
pub struct RowMerger {
    last_meter: Option<MeterPoint>,
    last_tester_t: Option<f64>,
}

impl RowMerger {
    /// A tester sample always gets a row; returns the meter reading to put
    /// next to it.
    pub fn tester(&mut self, s: &Sample) -> Option<MeterPoint> {
        self.last_tester_t = Some(s.t);
        self.last_meter.filter(|m| m.t <= s.t && s.t - m.t <= METER_HOLD_SECS)
    }

    /// Returns whether a meter reading needs a row of its own.
    pub fn meter(&mut self, m: &MeterPoint) -> bool {
        self.last_meter = Some(*m);
        self.last_tester_t.is_none_or(|t| m.t - t > TESTER_QUIET_SECS)
    }
}

/// Streams data to a CSV file while recording.
pub struct Recorder {
    writer: BufWriter<File>,
    merger: RowMerger,
    pub path: PathBuf,
    pub rows: u64,
}

impl Recorder {
    pub fn create(path: &Path) -> io::Result<Recorder> {
        let mut writer = BufWriter::new(File::create(path)?);
        writer.write_all(HEADER.as_bytes())?;
        Ok(Recorder { writer, merger: RowMerger::default(), path: path.to_path_buf(), rows: 0 })
    }

    pub fn write(&mut self, session: &Session, samples: &[Sample]) -> io::Result<()> {
        for s in samples {
            let m = self.merger.tester(s);
            write_row(&mut self.writer, s.t, session.wall_time(s.t), Some(s), m.as_ref())?;
            self.rows += 1;
        }
        // Keep the file useful even if the app is killed.
        if self.rows % 500 < samples.len() as u64 {
            self.writer.flush()?;
        }
        Ok(())
    }

    pub fn write_meter(&mut self, session: &Session, m: &MeterPoint) -> io::Result<()> {
        if self.merger.meter(m) {
            write_row(&mut self.writer, m.t, session.wall_time(m.t), None, Some(m))?;
            self.rows += 1;
            if self.rows.is_multiple_of(20) {
                self.writer.flush()?;
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<(PathBuf, u64)> {
        self.writer.flush()?;
        Ok((self.path, self.rows))
    }
}

/// Writes the in-memory history of both devices.
pub fn export_history(path: &Path, session: &Session) -> io::Result<u64> {
    let mut w = BufWriter::new(File::create(path)?);
    w.write_all(HEADER.as_bytes())?;
    let rows = write_history(&mut w, session)?;
    w.flush()?;
    Ok(rows)
}

fn write_history(w: &mut impl Write, session: &Session) -> io::Result<u64> {
    let mut merger = RowMerger::default();
    let mut rows = 0;
    let mut samples = session.history.iter().peekable();
    let mut meter = session.meter.iter().peekable();
    loop {
        let meter_first = match (samples.peek(), meter.peek()) {
            (Some(s), Some(m)) => m.t <= s.t,
            (None, Some(_)) => true,
            (Some(_), None) => false,
            (None, None) => break,
        };
        if meter_first {
            let m = meter.next().expect("peeked");
            if merger.meter(m) {
                write_row(w, m.t, session.wall_time(m.t), None, Some(m))?;
                rows += 1;
            }
        } else {
            let s = samples.next().expect("peeked");
            let m = merger.tester(s);
            write_row(w, s.t, session.wall_time(s.t), Some(s), m.as_ref())?;
            rows += 1;
        }
    }
    Ok(rows)
}

pub fn default_file_name(prefix: &str) -> String {
    format!("{prefix}-{}.csv", Local::now().format("%Y-%m-%d_%H-%M-%S"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owon::{Flags, Function, Point, Prefix, Reading};
    use crate::protocol::{ProtocolInfo, RawSample, RealtimeBlock};
    use std::time::Duration;

    fn reading(raw: i32) -> Reading {
        Reading {
            function: Function::DcVoltage,
            prefix: Prefix::Milli,
            point: Point::Decimals(1),
            negative: false,
            raw,
            flags: Flags(Flags::AUTO),
        }
    }

    fn block() -> RealtimeBlock {
        RealtimeBlock {
            protocol: ProtocolInfo::default(),
            power_bank_mode: false,
            interval_ms: 100,
            samples: vec![RawSample { voltage: 5.0, current: 1.0, ..Default::default() }; 10],
            status: 0,
            temperature_c: 25,
        }
    }

    #[test]
    fn merges_tester_and_meter_rows() {
        let mut s = Session::new(600.0);
        let t0 = s.started;
        // Meter alone: one row per reading.
        s.push_meter(reading(1234), t0 + Duration::from_millis(500));
        s.push_meter(reading(1235), t0 + Duration::from_millis(800));
        // Tester from 1 s to 2 s; the meter reading at 1.3 s is held.
        s.push_block(&block(), t0 + Duration::from_millis(2000));
        s.push_meter(reading(1236), t0 + Duration::from_millis(1300));
        // Tester silent: the meter writes again.
        s.push_meter(reading(1237), t0 + Duration::from_millis(3500));
        // A reading older than the hold time is not repeated.
        s.push_block(&block(), t0 + Duration::from_millis(8000));

        let mut out = Vec::new();
        let rows = write_history(&mut out, &s).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(rows as usize, lines.len());
        assert_eq!(rows, 2 + 10 + 1 + 10);
        let cols = |l: &str| l.split(',').map(str::to_string).collect::<Vec<_>>();
        let first = cols(lines[0]);
        assert_eq!(first.len(), HEADER.trim_end().split(',').count());
        assert_eq!(&first[2..9], &["", "", "", "", "", "", ""]);
        assert_eq!(&first[9..], &["0.1234", "V", "DCV", "123.4 mV", "AUTO"]);
        // Tester samples up to 1.3 s carry the 0.8 s reading, later ones the 1.3 s one.
        let tester: Vec<Vec<String>> = lines[2..12].iter().map(|l| cols(l)).collect();
        assert_eq!(tester[0][2], "5.00000");
        assert_eq!(tester[0][9], "0.1235");
        assert_eq!(tester[9][9], "0.1236");
        assert_eq!(cols(lines[12])[9], "0.1237");
        assert_eq!(cols(lines[12])[2], "");
        assert!(lines[13..].iter().all(|l| cols(l)[9].is_empty()));
    }
}
