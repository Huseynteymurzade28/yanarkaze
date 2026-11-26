//! The interactive shell: line editing, history, built-in commands and event printing.

use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_time::{Instant, Timer};
use embedded_io_async::{Read, Write};
use heapless::String;
use portable_atomic::Ordering;

use crate::editor::{Editor, Out};
use crate::platform::System;
use crate::state::{
    ALERT_MILLI_C, BUTTON_PRESSES, EVENTS, Event, LedMode, MORSE_MAX, SAMPLE, STAT_SHELL, STATS, alert_limit_c,
    current_led_mode, set_led_mode,
};
use crate::term::Term;
use crate::{VERSION, fun};

const PROMPT: &str = "\x1b[1;38;5;208myanarkaze\x1b[0m\x1b[1;38;5;51m>\x1b[0m ";

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

/// One shell command: its name, how to call it and what it does. `help` and tab completion
/// are generated from these tables; `Shell::run_command` dispatches on the name.
pub(crate) struct Command {
    pub name: &'static str,
    pub args: &'static str,
    pub help: &'static str,
    /// Words that tab completion offers for the first argument.
    pub completions: &'static [&'static str],
}

const fn cmd(name: &'static str, args: &'static str, help: &'static str) -> Command {
    Command { name, args, help, completions: &[] }
}

const LED_MODES: &[&str] = &["on", "off", "blink", "heartbeat", "morse"];

pub(crate) const SYSTEM_COMMANDS: &[Command] = &[
    cmd("help", "[command]", "show this help, or the help for one command"),
    cmd("info", "", "board, chip, clock, uptime and reset reason"),
    cmd("ps", "", "list running tasks and their wakeups"),
    cmd("top", "", "live task monitor with wakeups per second (any key stops)"),
    Command { completions: LED_MODES, ..cmd("led", "<mode>", "on | off | blink [ms] | heartbeat | morse <text>") },
    cmd("temp", "", "on-chip temperature (and supply voltage)"),
    cmd("watch", "", "live temperature graph (press any key to stop)"),
    cmd("alert", "<celsius>", "set the temperature alert threshold"),
    cmd("uptime", "", "time since boot"),
    cmd("history", "", "list the previous command lines"),
    cmd("echo", "<text>", "print the text"),
    cmd("hang", "", "freeze the executor -> watchdog resets the system"),
    cmd("reboot", "", "restart"),
    cmd("bootloader", "", "restart into the USB bootloader, ready for new firmware"),
    cmd("clear", "", "clear the screen"),
];

pub(crate) const FUN_COMMANDS: &[Command] = &[
    cmd("reaction", "", "reflex game: hit the button when the LED turns on"),
    Command {
        completions: &["d6", "2d6", "d20"],
        ..cmd("roll", "[N]dM", "roll dice with the RNG (roll 2d6, roll d20)")
    },
    cmd("timer", "<s> [label]", "spawn a new countdown task at runtime"),
    cmd("mandel", "", "Mandelbrot set, timed in CPU cycles"),
    cmd("life", "", "Conway's Game of Life"),
    cmd("matrix", "", "digital rain"),
    cmd("snake", "", "the classic, with arrow keys (the button pauses)"),
    cmd("dump", "<addr> [len]", "hex dump of real memory (dump with no args for examples)"),
    cmd("say", "<text>", "Ferris the crab says it"),
    cmd("fortune", "", "words of wisdom"),
];

/// Names of every command, in help order (for tab completion of the first word).
const COMMAND_NAMES: [&str; SYSTEM_COMMANDS.len() + FUN_COMMANDS.len()] = {
    let mut names = [""; SYSTEM_COMMANDS.len() + FUN_COMMANDS.len()];
    let mut i = 0;
    while i < SYSTEM_COMMANDS.len() {
        names[i] = SYSTEM_COMMANDS[i].name;
        i += 1;
    }
    let mut j = 0;
    while j < FUN_COMMANDS.len() {
        names[i + j] = FUN_COMMANDS[j].name;
        j += 1;
    }
    names
};

pub(crate) fn find_command(name: &str) -> Option<&'static Command> {
    SYSTEM_COMMANDS.iter().chain(FUN_COMMANDS).find(|c| c.name == name)
}

/// Tab completion: command names for the first word, the command's own list after it.
pub(crate) fn complete(context: &str) -> &'static [&'static str] {
    let mut words = context.split_whitespace();
    match (words.next(), words.next()) {
        (None, _) => &COMMAND_NAMES,
        (Some("help"), None) => &COMMAND_NAMES,
        (Some(name), None) => find_command(name).map_or(&[], |c| c.completions),
        _ => &[],
    }
}

const KEYS_HELP: &str = "Keys: Tab = complete, up/down = history, left/right/Home/End = move,\r\n      \
                         Ctrl+W/U/K = delete word/to start/to end, Ctrl+L = clear, Ctrl+C = cancel\r\n";

/// Shell state. The fun commands live in `fun.rs` as another `impl` block.
pub struct Shell<S: System, R: Read, W: Write> {
    pub(crate) sys: S,
    pub(crate) rx: R,
    pub(crate) term: Term<W>,
    pub(crate) spawner: Spawner,
    editor: Editor,
}

/// The shell task body. Run it from a board's `#[embassy_executor::task]`.
///
/// `rx` must be cancel-safe: dropping a pending `read` must not lose bytes (ring buffers,
/// pipes and buffered UARTs are fine).
pub async fn run<S: System, R: Read, W: Write>(sys: S, rx: R, tx: W, spawner: Spawner) -> ! {
    let mut shell = Shell { sys, rx, term: Term::new(tx), spawner, editor: Editor::new(PROMPT, complete) };
    shell.main_loop().await
}

fn uptime_parts() -> (u64, u64, u64, u64) {
    let ms = Instant::now().as_millis();
    (ms / 3_600_000, (ms / 60_000) % 60, (ms / 1000) % 60, ms % 1000)
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

    /// Prints the output of the line editor.
    async fn emit(&mut self, out: &Out) {
        self.term.print(out).await;
    }

    async fn main_loop(&mut self) -> ! {
        self.banner().await;
        self.term.print(PROMPT).await;
        self.term.flush().await;

        let mut buf = [0u8; 32];
        loop {
            // Wait for whichever comes first: keyboard input or an event from another task.
            match select(self.rx.read(&mut buf), EVENTS.receive()).await {
                Either::First(Ok(n)) => {
                    STAT_SHELL.tick();
                    for &b in &buf[..n] {
                        let mut out = Out::new();
                        let submitted = self.editor.feed(b, &mut out);
                        self.emit(&out).await;
                        if let Some(line) = submitted {
                            self.run_command(line.trim()).await;
                            self.term.print(PROMPT).await;
                        }
                    }
                }
                Either::First(Err(_)) => {}
                Either::Second(event) => {
                    STAT_SHELL.tick();
                    self.print_event(event).await;
                    let mut out = Out::new();
                    self.editor.redraw(&mut out);
                    self.emit(&out).await;
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
            "help" => self.cmd_help(args.next()).await,
            "history" => {
                for (i, entry) in self.editor.history().enumerate() {
                    out!(self.term, "  {:>2}  {entry}\r\n", i + 1);
                }
            }
            "echo" => {
                for (i, word) in args.enumerate() {
                    if i > 0 {
                        self.term.print(" ").await;
                    }
                    self.term.print(word).await;
                }
                self.term.print("\r\n").await;
            }
            "info" => self.cmd_info().await,
            "ps" => self.cmd_ps().await,
            "top" => self.cmd_top().await,
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
            "bootloader" => {
                self.term.print("restarting into the bootloader, see you after the update...\r\n").await;
                self.term.flush().await;
                Timer::after_millis(50).await; // let the output drain
                self.sys.enter_bootloader();
                self.term.print("this board has no bootloader the shell can start\r\n").await;
            }
            "clear" => self.term.print("\x1b[2J\x1b[H").await,
            "reaction" => self.cmd_reaction().await,
            "roll" => self.cmd_roll(args.next()).await,
            "timer" => self.cmd_timer(args).await,
            "mandel" => self.cmd_mandel().await,
            "life" => self.cmd_life().await,
            "matrix" => self.cmd_matrix().await,
            "snake" => self.cmd_snake().await,
            "dump" => self.cmd_dump(args).await,
            "say" => fun::say(&mut self.term, args).await,
            "fortune" => self.cmd_fortune().await,
            other => out!(self.term, "unknown command: '{other}' (try 'help')\r\n"),
        }
    }

    async fn cmd_help(&mut self, topic: Option<&str>) {
        if let Some(topic) = topic {
            match find_command(topic) {
                Some(c) => self.print_usage(c).await,
                None => out!(self.term, "no such command: '{topic}'\r\n"),
            }
            return;
        }
        for (title, commands) in [("System:", SYSTEM_COMMANDS), ("Fun:", FUN_COMMANDS)] {
            out!(self.term, "{title}\r\n");
            for c in commands {
                self.print_usage(c).await;
            }
            self.term.print("\r\n").await;
        }
        self.term.print(KEYS_HELP).await;
        let button = self.sys.info().button;
        out!(self.term, "Press {button} to cycle LED modes.\r\n");
    }

    async fn print_usage(&mut self, c: &Command) {
        let mut call: String<32> = String::new();
        let _ = call.push_str(c.name);
        if !c.args.is_empty() {
            let _ = call.push(' ');
            let _ = call.push_str(c.args);
        }
        out!(self.term, "  {:<20} {}\r\n", call.as_str(), c.help);
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

    async fn cmd_top(&mut self) {
        const REFRESH_MS: u64 = 1000;
        let mut previous = [0u32; STATS.len()];
        for (prev, stat) in previous.iter_mut().zip(STATS.iter()) {
            *prev = stat.wakeups();
        }
        let mut last = Instant::now();
        self.term.print("\x1b[2J\x1b[?25l").await;
        for frame in 0u32.. {
            let (h, m, s, _) = uptime_parts();
            let elapsed_ms = last.elapsed().as_millis().max(1);
            last = Instant::now();
            let sample = SAMPLE.lock(|s| s.get());
            let led = current_led_mode();
            let board = self.sys.info().board;
            let t = &mut self.term;

            out!(t, "\x1b[H\x1b[1;38;5;208myanarkaze\x1b[0m top - {board} - up {h:02}:{m:02}:{s:02}\x1b[K\r\n");
            out!(
                t,
                "temp {:.1} C (alert {:.1} C)   led {}   button presses {}\x1b[K\r\n\x1b[K\r\n",
                sample.reading.temp_c,
                alert_limit_c(),
                led.name(),
                BUTTON_PRESSES.load(Ordering::Relaxed)
            );
            out!(t, "\x1b[7m  {:<10} {:>10} {:>8}  {:<22}\x1b[0m\r\n", "TASK", "WAKEUPS", "PER SEC", "ACTIVITY");
            for (prev, stat) in previous.iter_mut().zip(STATS.iter()) {
                let now = stat.wakeups();
                let delta = now.wrapping_sub(*prev);
                *prev = now;
                let per_sec = delta as u64 * 1000 / elapsed_ms;
                let mut rate: String<12> = String::new();
                if frame == 0 {
                    let _ = rate.push('-'); // no interval to measure yet
                } else {
                    let _ = write!(rate, "{per_sec}");
                }
                // Log-ish bar: 1, 2, 4, 8, ... wakeups per second each add one block.
                let mut bar: String<64> = String::new();
                let mut v = per_sec;
                while v > 0 && bar.len() < 60 {
                    let _ = bar.push('\u{25a0}');
                    v /= 2;
                }
                out!(
                    t,
                    "  {:<10} {:>10} {:>8}  \x1b[36m{}\x1b[0m\x1b[K\r\n",
                    stat.name,
                    now,
                    rate.as_str(),
                    bar.as_str()
                );
            }
            let timers = fun::ACTIVE_TIMERS.load(Ordering::Relaxed);
            out!(t, "  {:<10} {:>10} {:>8}  {}/{} running\x1b[K\r\n", "countdown", "-", "-", timers, fun::TIMER_SLOTS);
            out!(t, "\x1b[K\r\n\x1b[2many key stops\x1b[0m\x1b[K");
            self.term.flush().await;
            if self.key_or_timeout(REFRESH_MS).await {
                break;
            }
        }
        self.term.print("\x1b[?25h\r\n").await;
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

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::format;

    #[test]
    fn command_names_are_unique() {
        for (i, a) in COMMAND_NAMES.iter().enumerate() {
            assert!(!COMMAND_NAMES[i + 1..].contains(a), "'{a}' is listed twice");
        }
    }

    #[test]
    fn every_listed_command_is_dispatched() {
        // Cheap guard against adding a table entry but forgetting the `match` arm.
        let source = include_str!("shell.rs");
        let dispatch = &source[source.find("match cmd {").unwrap()..];
        for name in COMMAND_NAMES {
            assert!(dispatch.contains(&format!("\"{name}\" =>")), "'{name}' is not handled by run_command");
        }
    }

    #[test]
    fn usage_fits_the_help_column() {
        for c in SYSTEM_COMMANDS.iter().chain(FUN_COMMANDS) {
            assert!(c.name.len() + 1 + c.args.len() <= 20, "usage of '{}' is too long", c.name);
        }
    }

    #[test]
    fn completion_contexts() {
        assert_eq!(complete("").len(), COMMAND_NAMES.len());
        assert_eq!(complete("help ").len(), COMMAND_NAMES.len());
        assert_eq!(complete("led "), LED_MODES);
        assert!(complete("led blink ").is_empty());
        assert!(complete("nope ").is_empty());
    }
}
