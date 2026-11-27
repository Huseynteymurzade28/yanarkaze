//! The `log` command, and writing its records in the background.

use core::fmt::Write as _;

use embassy_time::{Duration, Instant};
use embedded_io_async::{Read, Write};
use heapless::String;

use crate::log::{Log, SysFlash};
use crate::platform::{HwError, System};
use crate::screens::{Scale, Series};
use crate::shell::Shell;
use crate::state::{SAMPLE, TEMP_HISTORY};

const USAGE: &str = "log | log start [interval] | stop | show | csv | clear   (interval: 30s, 5m, 1h; default 1m)";

/// Parses `30s`, `5m`, `1h` or a plain number of minutes.
pub(crate) fn parse_interval(s: &str) -> Option<u32> {
    let (digits, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => s.split_at(i),
        None => (s, "m"),
    };
    let n: u32 = digits.parse().ok()?;
    let secs = match unit {
        "s" => n,
        "m" | "min" => n.checked_mul(60)?,
        "h" => n.checked_mul(3600)?,
        _ => return None,
    };
    (1..=24 * 3600).contains(&secs).then_some(secs)
}

/// `90` -> `90 s`, `300` -> `5 min`, `7200` -> `2 h`.
pub(crate) fn fmt_interval(secs: u32) -> String<16> {
    let mut s = String::new();
    let _ = if secs.is_multiple_of(3600) {
        write!(s, "{} h", secs / 3600)
    } else if secs.is_multiple_of(60) {
        write!(s, "{} min", secs / 60)
    } else {
        write!(s, "{secs} s")
    };
    s
}

/// Up to `N` averages of consecutive records, oldest first, covering the whole log.
struct Buckets<const N: usize> {
    sums: [f32; N],
    counts: [u16; N],
    /// Records per bucket.
    size: usize,
    min: f32,
    max: f32,
    boots: u32,
    room: bool,
}

impl<const N: usize> Buckets<N> {
    fn used(&self) -> usize {
        self.counts.iter().filter(|&&c| c > 0).count()
    }

    fn average(&self, i: usize) -> f32 {
        self.sums[i] / self.counts[i].max(1) as f32
    }
}

impl<S: System, R: Read, W: Write> Shell<S, R, W> {
    /// Finds the log in flash the first time it is needed.
    fn log_mounted(&mut self) -> Result<Log, HwError> {
        if let Some(log) = self.log {
            return Ok(log);
        }
        let log = Log::mount(&mut SysFlash(&mut self.sys))?;
        self.log = Some(log);
        Ok(log)
    }

    /// At boot: carries on logging if the saved settings say so.
    pub(crate) async fn log_resume(&mut self) {
        if self.settings.log_secs > 0 {
            self.log_start(self.settings.log_secs, false).await;
        }
    }

    async fn log_start(&mut self, secs: u32, verbose: bool) {
        if let Err(e) = self.log_mounted() {
            out!(self.term, "log: {}\r\n", no_log(e));
            return;
        }
        self.log_room = crate::env::detect(&mut self.sys).await.first().copied();
        self.settings.log_secs = secs;
        self.log_due = Some(Instant::now() + Duration::from_secs(secs as u64));
        let source = self.log_room.map_or("the chip's thermometer", |f| f.kind.name());
        if verbose {
            out!(self.term, "logging {source} every {}{}\r\n", fmt_interval(secs).as_str(), crate::shell::UNSAVED);
        } else {
            out!(self.term, "  logging {source} every {}\r\n", fmt_interval(secs).as_str());
        }
    }

    /// Writes a record if one is due. Called from the shell's main loop.
    pub(crate) async fn log_tick(&mut self) {
        let Some(due) = self.log_due else { return };
        let now = Instant::now();
        if now < due {
            return;
        }
        let period = Duration::from_secs(self.settings.log_secs.max(1) as u64);
        // Skip the records a long-running command made us miss rather than catch up.
        let mut next = due + period;
        if next <= now {
            next = now + period;
        }
        self.log_due = Some(next);

        let room = match self.log_room {
            Some(sensor) => crate::env::read(&mut self.sys, sensor).await.ok().map(|r| r.temp_c),
            None => None,
        };
        let temp_c = room.unwrap_or_else(|| {
            // The chip: the average of the last minute, steadier than one sample.
            let (sum, n) = TEMP_HISTORY.lock(|h| h.borrow().iter().fold((0.0, 0), |(s, n), v| (s + v, n + 1)));
            if n > 0 { sum / n as f32 } else { SAMPLE.lock(|s| s.get()).reading.temp_c }
        });
        let centi_c = (temp_c * 100.0).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        let boot = !self.log_since_boot;
        let Ok(mut log) = self.log_mounted() else { return };
        match log.append(&mut SysFlash(&mut self.sys), centi_c, boot, room.is_some()) {
            Ok(()) => {
                self.log = Some(log);
                self.log_since_boot = true;
            }
            Err(e) => {
                self.log_due = None;
                out!(self.term, "\r\n[log] writing failed ({e}), logging stopped\r\n");
            }
        }
    }

    pub(crate) async fn cmd_log<'a>(&mut self, mut args: impl Iterator<Item = &'a str>) {
        match args.next() {
            None => self.log_status().await,
            Some("start") => {
                let secs = match args.next() {
                    None => 60,
                    Some(v) => match parse_interval(v) {
                        Some(s) => s,
                        None => {
                            out!(self.term, "usage: {USAGE}\r\n");
                            return;
                        }
                    },
                };
                self.log_start(secs, true).await;
            }
            Some("stop") => {
                self.log_due = None;
                self.settings.log_secs = 0;
                out!(self.term, "logging stopped (the records stay){}\r\n", crate::shell::UNSAVED);
            }
            Some("show") => self.log_show().await,
            Some("csv") => self.log_csv().await,
            Some("clear") => {
                let result =
                    self.log_mounted().and_then(|mut log| log.clear(&mut SysFlash(&mut self.sys)).map(|()| log));
                match result {
                    Ok(log) => {
                        self.log = Some(log);
                        self.term.print("log erased\r\n").await;
                    }
                    Err(e) => out!(self.term, "log: {}\r\n", no_log(e)),
                }
            }
            Some(_) => out!(self.term, "usage: {USAGE}\r\n"),
        }
    }

    async fn log_status(&mut self) {
        let log = match self.log_mounted() {
            Ok(log) => log,
            Err(e) => {
                out!(self.term, "log: {}\r\n", no_log(e));
                return;
            }
        };
        match self.log_due {
            Some(_) => {
                let source = self.log_room.map_or("chip", |f| f.kind.name());
                out!(self.term, "logging: on, {source} every {}\r\n", fmt_interval(self.settings.log_secs).as_str());
            }
            None => self.term.print("logging: off ('log start' starts it)\r\n").await,
        }
        let mut last = None;
        let _ = log.for_each(&mut SysFlash(&mut self.sys), |r| last = Some(r));
        out!(self.term, "records: {} of {}", log.count, log.capacity);
        if self.settings.log_secs > 0 {
            let hours = log.capacity as u64 * self.settings.log_secs as u64 / 3600;
            out!(self.term, " (room for {hours} h at this rate, then the oldest go)");
        }
        self.term.print("\r\n").await;
        if let Some(r) = last {
            out!(self.term, "newest: {:.2} C\r\n", r.temp_c());
        }
    }

    fn log_buckets<const N: usize>(&mut self) -> Result<Buckets<N>, HwError> {
        let log = self.log_mounted()?;
        let size = log.count.div_ceil(N).max(1);
        let mut b =
            Buckets { sums: [0.0; N], counts: [0; N], size, min: f32::MAX, max: f32::MIN, boots: 0, room: false };
        let mut i = 0;
        log.for_each(&mut SysFlash(&mut self.sys), |r| {
            let t = r.temp_c();
            let k = (i / size).min(N - 1);
            b.sums[k] += t;
            b.counts[k] += 1;
            b.min = b.min.min(t);
            b.max = b.max.max(t);
            b.boots += r.boot as u32;
            b.room |= r.room;
            i += 1;
        })?;
        Ok(b)
    }

    async fn log_show(&mut self) {
        const COLUMNS: usize = 64;
        const ROWS: usize = 8;
        let b: Buckets<COLUMNS> = match self.log_buckets() {
            Ok(b) => b,
            Err(e) => {
                out!(self.term, "log: {}\r\n", no_log(e));
                return;
            }
        };
        let used = b.used();
        if used == 0 {
            self.term.print("the log is empty ('log start' starts it)\r\n").await;
            return;
        }
        out!(self.term, "{} records, {:.2} to {:.2} C", self.log.map_or(0, |l| l.count), b.min, b.max);
        if self.settings.log_secs > 0 {
            let span = fmt_interval(self.settings.log_secs * b.size as u32);
            out!(self.term, ", one column = {} record(s) = {}", b.size, span.as_str());
        } else {
            out!(self.term, ", one column = {} record(s)", b.size);
        }
        if b.boots > 1 {
            out!(self.term, ", {} reboot(s) in between", b.boots - 1);
        }
        self.term.print("\r\n").await;
        let (mut lo, mut hi) = (b.min, b.max);
        if hi - lo < 1.0 {
            let mid = (hi + lo) / 2.0;
            (lo, hi) = (mid - 0.5, mid + 0.5);
        }
        const BLOCKS: [char; 9] =
            [' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}'];
        for row in (0..ROWS).rev() {
            let mut line: String<320> = String::new();
            let label = lo + (hi - lo) * (row + 1) as f32 / ROWS as f32;
            let _ = write!(line, "{label:7.2} \u{2502}\x1b[36m");
            for i in 0..used {
                let _ = line.push(BLOCKS[crate::shell::chart_level(b.average(i), lo, hi, row, ROWS)]);
            }
            let _ = line.push_str("\x1b[0m\r\n");
            self.term.print(&line).await;
        }
        out!(self.term, "        \u{2514}oldest{:>width$}\r\n", "newest", width = used.saturating_sub(6).max(7));

        // The same on the OLED, with twice the resolution.
        if self.oled_init().await.is_ok() {
            let Ok(fine) = self.log_buckets::<{ crate::oled::WIDTH }>() else { return };
            let mut series = Series::new();
            for i in 0..fine.used() {
                series.push(fine.average(i));
            }
            let title = if fine.room { "room log" } else { "chip log" };
            let mut unit: String<4> = String::new();
            let _ = write!(unit, "{}C", crate::oled::DEGREE);
            crate::screens::plot(&mut self.oled, title, &series, Scale::Auto { min_span: 1.0 }, 1, &unit);
            let mut on = true;
            self.oled_mirror(&mut on).await;
        }
    }

    async fn log_csv(&mut self) {
        let log = match self.log_mounted() {
            Ok(log) => log,
            Err(e) => {
                out!(self.term, "log: {}\r\n", no_log(e));
                return;
            }
        };
        // Copy the records out in chunks: printing needs `self` too.
        self.term.print("seq,boot,temp_c,source\r\n").await;
        let mut boot = 0u32;
        let mut done = 0usize;
        loop {
            let mut chunk: heapless::Vec<crate::log::Record, 32> = heapless::Vec::new();
            let mut seen = 0usize;
            let _ = log.for_each(&mut SysFlash(&mut self.sys), |r| {
                if seen >= done && !chunk.is_full() {
                    let _ = chunk.push(r);
                }
                seen += 1;
            });
            if chunk.is_empty() {
                break;
            }
            done += chunk.len();
            for r in &chunk {
                boot += r.boot as u32;
                let source = if r.room { "room" } else { "chip" };
                out!(self.term, "{},{},{:.2},{source}\r\n", r.seq, boot.max(1), r.temp_c());
            }
        }
    }
}

fn no_log(e: HwError) -> &'static str {
    match e {
        HwError::Unsupported => "this board has no flash set aside for a log",
        _ => "the flash did not answer",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals() {
        assert_eq!(parse_interval("30s"), Some(30));
        assert_eq!(parse_interval("5m"), Some(300));
        assert_eq!(parse_interval("5"), Some(300));
        assert_eq!(parse_interval("2h"), Some(7200));
        assert_eq!(parse_interval("0s"), None);
        assert_eq!(parse_interval("25h"), None);
        assert_eq!(parse_interval("5x"), None);
        assert_eq!(fmt_interval(90).as_str(), "90 s");
        assert_eq!(fmt_interval(300).as_str(), "5 min");
        assert_eq!(fmt_interval(7200).as_str(), "2 h");
    }
}
