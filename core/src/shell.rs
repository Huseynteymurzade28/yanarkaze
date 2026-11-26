//! The interactive shell: line editing, history, built-in commands and event printing.

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_time::{Instant, Timer};
use embedded_io_async::{Read, Write};
use heapless::{String, Vec};
use portable_atomic::Ordering;

use crate::platform::System;
use crate::state::{
    ALERT_MILLI_C, BUTTON_PRESSES, EVENTS, Event, LedMode, MORSE_MAX, SAMPLE, STAT_SHELL, STATS, alert_limit_c,
    current_led_mode, set_led_mode,
};
use crate::term::Term;
use crate::{VERSION, fun};

const PROMPT: &str = "\x1b[1;38;5;208myanarkaze\x1b[0m\x1b[1;38;5;51m>\x1b[0m ";
const LINE_MAX: usize = 96;
const HISTORY: usize = 8;

const LOGO: &[&str] = &[
    "  \x1b[1;38;5;196m      \x1b[1;38;5;202m      \x1b[1;38;5;208m      \x1b[1;38;5;214m      \x1b[1;38;5;220m     \x1b[1;38;5;159m _   \x1b[1;38;5;123m      \x1b[1;38;5;87m     \x1b[1;38;5;51m     \x1b[0m",
    "  \x1b[1;38;5;196m _  _ \x1b[1;38;5;202m __ _ \x1b[1;38;5;208m _ _  \x1b[1;38;5;214m __ _ \x1b[1;38;5;220m _ _ \x1b[1;38;5;159m| |__\x1b[1;38;5;123m __ _ \x1b[1;38;5;87m ___ \x1b[1;38;5;51m ___ \x1b[0m",
    "  \x1b[1;38;5;196m| || |\x1b[1;38;5;202m/ _` |\x1b[1;38;5;208m| ' \\ \x1b[1;38;5;214m/ _` |\x1b[1;38;5;220m| '_|\x1b[1;38;5;159m| / /\x1b[1;38;5;123m/ _` |\x1b[1;38;5;87m|_ / \x1b[1;38;5;51m/ -_)\x1b[0m",
    "  \x1b[1;38;5;196m \\_, |\x1b[1;38;5;202m\\__,_|\x1b[1;38;5;208m|_||_|\x1b[1;38;5;214m\\__,_|\x1b[1;38;5;220m|_|  \x1b[1;38;5;159m|_\\_\\\x1b[1;38;5;123m\\__,_|\x1b[1;38;5;87m/__| \x1b[1;38;5;51m\\___|\x1b[0m",
    "  \x1b[1;38;5;196m |__/ \x1b[1;38;5;202m      \x1b[1;38;5;208m      \x1b[1;38;5;214m      \x1b[1;38;5;220m     \x1b[1;38;5;159m     \x1b[1;38;5;123m      \x1b[1;38;5;87m     \x1b[1;38;5;51m     \x1b[0m",
    "",
    "  \x1b[38;5;208myanar\x1b[0m (fire, as in Yanardag) + \x1b[38;5;51mkaze\x1b[0m (\u{98a8}, wind)",
    "  an async OS that keeps burning wherever the wind takes it",
    "",
];

const HELP: &[&str] = &[
    "System:",
    "  help                 show this help",
    "  info                 board, chip, clock, uptime and reset reason",
    "  ps                   list running tasks and their wakeups",
    "  led <mode>           on | off | blink [ms] | heartbeat | morse <text>",
    "  temp                 on-chip temperature (and supply voltage)",
    "  watch                live temperature graph (press any key to stop)",
    "  alert <celsius>      set the temperature alert threshold",
    "  uptime               time since boot",
    "  hang                 freeze the executor -> watchdog resets the system",
    "  reboot               restart",
    "  clear                clear the screen",
    "",
    "Fun:",
    "  reaction             reflex game: hit the button when the LED turns on",
    "  roll [N]dM           roll dice with the RNG (roll 2d6, roll d20)",
    "  timer <s> [label]    spawn a new countdown task at runtime",
    "  mandel               Mandelbrot set, timed in CPU cycles",
    "  life                 Conway's Game of Life",
    "  matrix               digital rain",
    "  dump <addr> [len]    hex dump of real memory (dump with no args for examples)",
    "  say <text>           Ferris the crab says it",
    "  fortune              words of wisdom",
    "",
    "Keys: up/down = history, Ctrl+C = cancel line",
];

/// Shell state. The fun commands live in `fun.rs` as another `impl` block.
pub struct Shell<S: System, R: Read, W: Write> {
    pub(crate) sys: S,
    pub(crate) rx: R,
    pub(crate) term: Term<W>,
    pub(crate) spawner: Spawner,
}

/// The shell task body. Run it from a board's `#[embassy_executor::task]`.
///
/// `rx` must be cancel-safe: dropping a pending `read` must not lose bytes (ring buffers,
/// pipes and buffered UARTs are fine).
pub async fn run<S: System, R: Read, W: Write>(sys: S, rx: R, tx: W, spawner: Spawner) -> ! {
    let mut shell = Shell { sys, rx, term: Term::new(tx), spawner };
    shell.main_loop().await
}

fn uptime_parts() -> (u64, u64, u64, u64) {
    let ms = Instant::now().as_millis();
    (ms / 3_600_000, (ms / 60_000) % 60, (ms / 1000) % 60, ms % 1000)
}

enum Escape {
    None,
    Esc,
    Csi,
}

impl<S: System, R: Read, W: Write> Shell<S, R, W> {
    /// Waits `ms` milliseconds. Returns true if a key was pressed in the meantime.
    pub(crate) async fn key_or_timeout(&mut self, ms: u64) -> bool {
        let mut buf = [0u8; 8];
        match select(self.rx.read(&mut buf), Timer::after_millis(ms)).await {
            Either::First(Ok(n)) if n > 0 => true,
            Either::First(_) => {
                // Disconnected or a read error: still honour the delay.
                Timer::after_millis(ms).await;
                false
            }
            Either::Second(()) => false,
        }
    }

    async fn banner(&mut self) {
        self.term.print("\r\n").await;
        self.term.print_lines(LOGO).await;
        let info = self.sys.info();
        let (board, chip, hints) = (info.board, info.chip, info.hints);
        out!(self.term, "  running on \x1b[1m{board}\x1b[0m ({chip}), yanarkaze v{VERSION}\r\n");
        for hint in hints {
            out!(self.term, "  \x1b[2m{hint}\x1b[0m\r\n");
        }
        let reason = self.sys.reset_reason();
        out!(self.term, "  last reset: {reason}\r\n");
        self.term.print("  type 'help' to get started\r\n\r\n").await;
    }

    async fn redraw_line(&mut self, line: &str) {
        self.term.print("\r").await;
        self.term.print(PROMPT).await;
        self.term.print(line).await;
        self.term.print("\x1b[K").await;
    }

    async fn main_loop(&mut self) -> ! {
        self.banner().await;
        self.term.print(PROMPT).await;

        let mut line: String<LINE_MAX> = String::new();
        let mut history: Vec<String<LINE_MAX>, HISTORY> = Vec::new();
        let mut cursor = 0; // position in history while browsing with the arrow keys
        let mut escape = Escape::None;
        let mut last_was_cr = false;
        let mut buf = [0u8; 32];

        loop {
            // Wait for whichever comes first: keyboard input or an event from another task.
            match select(self.rx.read(&mut buf), EVENTS.receive()).await {
                Either::First(Ok(n)) => {
                    STAT_SHELL.tick();
                    for &b in &buf[..n] {
                        // Arrow keys arrive as ESC [ A / ESC [ B.
                        match escape {
                            Escape::Esc => {
                                escape = if b == b'[' { Escape::Csi } else { Escape::None };
                                continue;
                            }
                            Escape::Csi => {
                                if (0x40..=0x7e).contains(&b) {
                                    escape = Escape::None;
                                    let target = match b {
                                        b'A' if cursor > 0 => Some(cursor - 1),
                                        b'B' if cursor < history.len() => Some(cursor + 1),
                                        _ => None,
                                    };
                                    if let Some(target) = target {
                                        cursor = target;
                                        line = history.get(cursor).cloned().unwrap_or_default();
                                        self.redraw_line(&line).await;
                                    }
                                }
                                continue;
                            }
                            Escape::None => {}
                        }

                        let was_cr = core::mem::replace(&mut last_was_cr, b == b'\r');
                        match b {
                            0x1b => escape = Escape::Esc,
                            b'\n' if was_cr => {} // CR LF counts as one Enter
                            b'\r' | b'\n' => {
                                self.term.print("\r\n").await;
                                let cmd = core::mem::take(&mut line);
                                let cmd = cmd.trim();
                                if !cmd.is_empty() && history.last().map(|h| h.as_str()) != Some(cmd) {
                                    if history.is_full() {
                                        history.remove(0);
                                    }
                                    let _ = history.push(String::try_from(cmd).unwrap_or_default());
                                }
                                cursor = history.len();
                                self.run_command(cmd).await;
                                self.term.print(PROMPT).await;
                            }
                            0x03 => {
                                self.term.print("^C\r\n").await;
                                line.clear();
                                cursor = history.len();
                                self.term.print(PROMPT).await;
                            }
                            0x08 | 0x7f => {
                                if line.pop().is_some() {
                                    self.term.print("\x08 \x08").await;
                                }
                            }
                            0x20..=0x7e => {
                                if line.push(b as char).is_ok() {
                                    self.term.write_bytes(&[b]).await;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Either::First(Err(_)) => {}
                Either::Second(event) => {
                    STAT_SHELL.tick();
                    self.print_event(event).await;
                    self.term.print(PROMPT).await;
                    self.term.print(&line).await;
                }
            }
            self.term.flush().await;
        }
    }

    async fn print_event(&mut self, event: Event) {
        let t = &mut self.term;
        match event {
            Event::Button { presses, mode } => {
                out!(t, "\r\n\x1b[1;33m[event]\x1b[0m button pressed (#{presses}), led mode -> {mode}\r\n")
            }
            Event::TempAlert { temp_c, limit_c } => {
                out!(t, "\r\n\x1b[1;31m[alert]\x1b[0m temperature {temp_c:.1} C is above {limit_c:.1} C\r\n")
            }
            Event::TempNormal { temp_c } => {
                out!(t, "\r\n\x1b[1;32m[ok]\x1b[0m temperature back to normal ({temp_c:.1} C)\r\n")
            }
            Event::Timer { id, secs, label } => {
                out!(t, "\x07\r\n\x1b[1;35m[timer]\x1b[0m countdown#{id} '{}' done after {secs} s\r\n", label.as_str())
            }
        }
    }

    async fn run_command(&mut self, line: &str) {
        let mut args = line.split_whitespace();
        let Some(cmd) = args.next() else { return };

        match cmd {
            "help" => {
                self.term.print_lines(HELP).await;
                let button = self.sys.info().button;
                out!(self.term, "Press {button} to cycle LED modes.\r\n");
            }
            "info" => self.cmd_info().await,
            "ps" => self.cmd_ps().await,
            "led" => self.cmd_led(args).await,
            "temp" => {
                let sample = SAMPLE.lock(|s| s.get());
                out!(self.term, "temperature : {:.1} C\r\n", sample.reading.temp_c);
                if let Some(mv) = sample.reading.vdda_mv {
                    out!(self.term, "supply      : {mv} mV\r\n");
                }
                out!(self.term, "alert above : {:.1} C\r\n", alert_limit_c());
                out!(self.term, "samples     : {}\r\n", sample.count);
            }
            "watch" => self.cmd_watch().await,
            "alert" => match args.next().and_then(|v| v.parse::<f32>().ok()) {
                Some(c) => {
                    ALERT_MILLI_C.store((c * 1000.0) as i32, Ordering::Relaxed);
                    out!(self.term, "temperature alert threshold set to {c:.1} C\r\n");
                }
                None => self.term.print("usage: alert <celsius>\r\n").await,
            },
            "uptime" => {
                let (h, m, s, ms) = uptime_parts();
                out!(self.term, "up {h:02}:{m:02}:{s:02}.{ms:03}\r\n");
            }
            "hang" => {
                self.term.print("\x1b[1;31mfreezing the executor with a blocking loop...\x1b[0m\r\n").await;
                self.term.print("no task can run now, so nobody feeds the watchdog. bye!\r\n").await;
                self.term.flush().await;
                Timer::after_millis(20).await; // let the output drain
                // A busy loop never yields back to the executor: exactly the bug a
                // watchdog protects against in a cooperative system.
                self.sys.hang();
            }
            "reboot" => {
                self.term.print("rebooting...\r\n").await;
                self.term.flush().await;
                Timer::after_millis(20).await;
                self.sys.reset();
            }
            "clear" => self.term.print("\x1b[2J\x1b[H").await,
            "reaction" => self.cmd_reaction().await,
            "roll" => self.cmd_roll(args.next()).await,
            "timer" => self.cmd_timer(args).await,
            "mandel" => self.cmd_mandel().await,
            "life" => self.cmd_life().await,
            "matrix" => self.cmd_matrix().await,
            "dump" => self.cmd_dump(args).await,
            "say" => fun::say(&mut self.term, args).await,
            "fortune" => self.cmd_fortune().await,
            other => out!(self.term, "unknown command: '{other}' (try 'help')\r\n"),
        }
    }

    async fn cmd_info(&mut self) {
        let (h, m, s, _) = uptime_parts();
        let reason = self.sys.reset_reason();
        let uid = self.sys.unique_id();
        let info = self.sys.info();
        let (board, chip, cpu, fpu) = (info.board, info.chip, info.cpu, info.fpu);
        let (mhz, flash, ram) = (info.cpu_mhz, info.flash_kib, info.ram_kib);
        let t = &mut self.term;

        out!(t, "board     : {board}\r\n");
        out!(t, "chip      : {chip}\r\n");
        match mhz {
            Some(mhz) => out!(t, "cpu       : {cpu} @ {mhz} MHz{}\r\n", if fpu { ", hardware FPU" } else { "" }),
            None => out!(t, "cpu       : {cpu}\r\n"),
        }
        if let (Some(flash), Some(ram)) = (flash, ram) {
            out!(t, "memory    : {flash} KiB flash, {ram} KiB RAM\r\n");
        }
        if let Some(uid) = uid {
            out!(t, "uid       : {}\r\n", uid.as_str());
        }
        out!(t, "runtime   : yanarkaze v{VERSION} on embassy-executor (cooperative async)\r\n");
        out!(t, "uptime    : {h:02}:{m:02}:{s:02}\r\n");
        out!(t, "reset     : {reason}\r\n");
    }

    async fn cmd_ps(&mut self) {
        let t = &mut self.term;
        out!(t, "  {:<10} {:>10}  {}\r\n", "TASK", "WAKEUPS", "STATE");
        for stat in STATS.iter() {
            let state = match stat.name {
                "shell" => "running",
                "led" => current_led_mode().name(),
                _ => "waiting",
            };
            out!(t, "  {:<10} {:>10}  {}\r\n", stat.name, stat.wakeups(), state);
        }
        let timers = fun::ACTIVE_TIMERS.load(Ordering::Relaxed);
        out!(t, "  {:<10} {:>10}  {}/{} slots running\r\n", "countdown", "-", timers, fun::TIMER_SLOTS);
        out!(t, "  button presses: {}\r\n", BUTTON_PRESSES.load(Ordering::Relaxed));
    }

    async fn cmd_led<'a>(&mut self, mut args: impl Iterator<Item = &'a str>) {
        const USAGE: &str = "usage: led on | off | blink [ms] | heartbeat | morse <text>\r\n";
        let mode = match args.next() {
            Some("on") => LedMode::On,
            Some("off") => LedMode::Off,
            Some("heartbeat") => LedMode::Heartbeat,
            Some("blink") => {
                let ms = args.next().and_then(|v| v.parse().ok()).unwrap_or(500u32);
                LedMode::Blink(ms.clamp(20, 10_000))
            }
            Some("morse") => {
                let mut text: String<MORSE_MAX> = String::new();
                for (i, word) in args.enumerate() {
                    if i > 0 {
                        let _ = text.push(' ');
                    }
                    let _ = text.push_str(word);
                }
                if text.is_empty() {
                    self.term.print("usage: led morse <text>\r\n").await;
                    return;
                }
                LedMode::Morse(text)
            }
            _ => {
                self.term.print(USAGE).await;
                return;
            }
        };
        out!(self.term, "led mode -> {}\r\n", mode.name());
        set_led_mode(mode);
    }

    async fn cmd_watch(&mut self) {
        self.term.print("live temperature - press any key to stop\r\n").await;
        loop {
            let reading = SAMPLE.lock(|s| s.get()).reading;
            let limit = alert_limit_c();
            // Map 20..45 C onto a 40 character bar.
            let len = (((reading.temp_c - 20.0) / 25.0 * 40.0) as i32).clamp(0, 40) as usize;
            let color = if reading.temp_c > limit { "31" } else { "36" };
            let mut bar: String<48> = String::new();
            for i in 0..40 {
                let _ = bar.push(if i < len { '#' } else { '.' });
            }
            out!(self.term, "\r  {:6.2} C  \x1b[{}m{}\x1b[0m ", reading.temp_c, color, bar.as_str());
            if self.key_or_timeout(250).await {
                break;
            }
        }
        self.term.print("\r\n").await;
    }
}
