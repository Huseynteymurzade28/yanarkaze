//! The interactive shell: line editing, history, built-in commands and event printing.

use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_time::{Instant, Timer};
use embedded_io_async::{Read, Write};
use heapless::String;
use portable_atomic::Ordering;

use crate::editor::{Editor, Out};
use crate::oled::Display;
use crate::platform::System;
use crate::state::{
    ALERT_MILLI_C, BUTTON_PRESSES, EVENTS, Event, HISTORY_SECS, LedMode, MORSE_MAX, SAMPLE, STAT_SHELL, STATS,
    TEMP_HISTORY, alert_limit_c, current_led_mode, set_led_mode,
};
use crate::term::Term;
use crate::{VERSION, calc, fun};

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
    cmd("watch", "", "live temperature bar (press any key to stop)"),
    cmd("graph", "", "live chart of the last minute's temperature (any key stops)"),
    cmd("alert", "<celsius>", "set the temperature alert threshold"),
    cmd("uptime", "", "time since boot"),
    cmd("history", "", "list the previous command lines"),
    cmd("echo", "<text>", "print the text"),
    cmd("calc", "<expr>", "calculator: 0xff & ~0x0f, 1 << 12, 7 / 2, (3 + 4) * 2"),
    cmd("repeat", "<n> <cmd>", "run a command n times (any key stops)"),
    cmd("every", "<s> <cmd>", "run a command every s seconds (any key stops)"),
    cmd("hang", "", "freeze the executor -> watchdog resets the system"),
    cmd("reboot", "", "restart"),
    cmd("bootloader", "", "restart into the USB bootloader, ready for new firmware"),
    cmd("clear", "", "clear the screen"),
];

const GPIO_ACTIONS: &[&str] = &["on", "off", "toggle", "read"];
pub(crate) const OLED_ACTIONS: &[&str] =
    &["text", "temp", "dash", "life", "logo", "preview", "clear", "invert", "contrast", "on", "off"];

pub(crate) const HW_COMMANDS: &[Command] = &[
    cmd("pins", "", "which pins can do what on this board"),
    cmd("gpio", "<pin> <action>", "on | off | toggle | read [up|down]   e.g. gpio 16 on"),
    cmd("pwm", "<pin> <%> [hz]", "PWM output, e.g. LED brightness: pwm 16 25   (pwm 16 off)"),
    cmd("fade", "<pin> [ms]", "make an LED breathe (any key stops)"),
    cmd("adc", "<pin> [watch]", "read an analog pin in millivolts, e.g. a potentiometer"),
    cmd("knob", "<adc> <pwm>", "a potentiometer dims an LED, live: knob 26 16"),
    Command {
        completions: &["scan", "read", "write", "speed", "reset"],
        ..cmd("i2c", "scan|read|write", "find I2C devices, read or write registers")
    },
    Command {
        completions: OLED_ACTIONS,
        ..cmd("oled", "<action>", "0.96\" I2C display: text <msg> | temp | dash | life | logo | preview | clear")
    },
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
    cmd("2048", "", "slide and merge tiles with the arrow keys"),
    cmd("dump", "<addr> [len]", "hex dump of real memory (dump with no args for examples)"),
    cmd("say", "<text>", "Ferris the crab says it"),
    cmd("fortune", "", "words of wisdom"),
];

/// Names of every command, in help order (for tab completion of the first word).
/// Every command table, in help order.
const SECTIONS: [(&str, &[Command]); 3] =
    [("System:", SYSTEM_COMMANDS), ("Hardware:", HW_COMMANDS), ("Fun:", FUN_COMMANDS)];

const COMMAND_COUNT: usize = SYSTEM_COMMANDS.len() + HW_COMMANDS.len() + FUN_COMMANDS.len();

/// Names of every command, in help order (for tab completion of the first word).
const COMMAND_NAMES: [&str; COMMAND_COUNT] = {
    let mut names = [""; COMMAND_COUNT];
    let mut n = 0;
    let mut s = 0;
    while s < SECTIONS.len() {
        let table = SECTIONS[s].1;
        let mut i = 0;
        while i < table.len() {
            names[n] = table[i].name;
            n += 1;
            i += 1;
        }
        s += 1;
    }
    names
};

fn all_commands() -> impl Iterator<Item = &'static Command> {
    SECTIONS.iter().flat_map(|(_, table)| table.iter())
}

pub(crate) fn find_command(name: &str) -> Option<&'static Command> {
    all_commands().find(|c| c.name == name)
}

/// Tab completion: command names for the first word, the command's own list after it.
pub(crate) fn complete(context: &str) -> &'static [&'static str] {
    // Only the last command of a `a; b` chain matters, minus any `repeat 3` / `every 2`.
    let mut context = context.rsplit(';').next().unwrap_or("");
    loop {
        let (first, rest) = split_word(context);
        let (count, inner) = split_word(rest);
        if matches!(first, "repeat" | "every") && !count.is_empty() && (!inner.is_empty() || rest.ends_with(' ')) {
            context = inner;
        } else {
            break;
        }
    }
    let mut words = context.split_whitespace();
    match (words.next(), words.next()) {
        (None, _) => &COMMAND_NAMES,
        (Some("help"), None) => &COMMAND_NAMES,
        (Some(name), None) => find_command(name).map_or(&[], |c| c.completions),
        (Some("gpio"), Some(_pin)) if words.next().is_none() => GPIO_ACTIONS,
        _ => &[],
    }
}

/// Edit distance between two short words: insertions, deletions, substitutions and
/// swapped neighbours ("tmep" is one step from "temp"), the most common typo.
fn distance(a: &str, b: &str) -> usize {
    const MAX: usize = 24;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() > MAX || b.len() > MAX {
        return usize::MAX;
    }
    let mut d = [[0usize; MAX + 1]; MAX + 1];
    for (i, row) in d.iter_mut().enumerate().take(a.len() + 1) {
        row[0] = i;
    }
    for j in 0..=b.len() {
        d[0][j] = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[a.len()][b.len()]
}

/// The closest of `choices` to a mistyped `word`, if it is close enough to be a typo.
pub(crate) fn suggest<'a>(word: &str, choices: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = if word.len() <= 3 { 1 } else { 2 };
    choices
        .into_iter()
        .map(|c| (distance(word, c), c))
        .filter(|&(d, _)| d <= limit)
        .min_by_key(|&(d, _)| d)
        .map(|(_, c)| c)
}

/// Resolves `word` against `choices`: an exact match, or an unambiguous prefix (`tex` -> `text`).
pub(crate) fn resolve<'a>(word: &str, choices: &[&'a str]) -> Option<&'a str> {
    if let Some(&exact) = choices.iter().find(|&&c| c == word) {
        return Some(exact);
    }
    let mut matches = choices.iter().filter(|c| !word.is_empty() && c.starts_with(word));
    match (matches.next(), matches.next()) {
        (Some(&only), None) => Some(only),
        _ => None,
    }
}

/// Splits off the first word: `"led  on x"` -> `("led", "on x")`.
pub(crate) fn split_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(' ') {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

/// Which of the 8 block heights to draw for `value` in chart row `row` (0 = bottom) of a
/// chart `rows` high spanning `lo..hi`.
pub(crate) fn chart_level(value: f32, lo: f32, hi: f32, row: usize, rows: usize) -> usize {
    let eighths = ((value - lo) / (hi - lo) * (rows * 8) as f32) as i32;
    (eighths - row as i32 * 8).clamp(0, 8) as usize
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
    /// Last level written to each pin by `gpio`, for `gpio <pin> toggle`.
    pub(crate) gpio_levels: u64,
    pub(crate) oled: Display,
    pub(crate) oled_ready: bool,
    pub(crate) oled_inverted: bool,
}

/// The shell task body. Run it from a board's `#[embassy_executor::task]`.
///
/// `rx` must be cancel-safe: dropping a pending `read` must not lose bytes (ring buffers,
/// pipes and buffered UARTs are fine).
pub async fn run<S: System, R: Read, W: Write>(sys: S, rx: R, tx: W, spawner: Spawner) -> ! {
    let mut shell = Shell {
        sys,
        rx,
        term: Term::new(tx),
        spawner,
        editor: Editor::new(PROMPT, complete),
        gpio_levels: 0,
        oled: Display::new(),
        oled_ready: false,
        oled_inverted: false,
    };
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
        self.oled_boot().await;
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

    /// Runs a command line: `;` separates commands, `repeat` and `every` wrap one.
    async fn run_command(&mut self, line: &str) {
        for part in line.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            let (first, rest) = split_word(part);
            let (count, inner) = split_word(rest);
            match first {
                "repeat" => match count.parse::<u32>() {
                    Ok(n @ 1..=1000) if Self::wrappable(inner) => {
                        for i in 0..n {
                            self.run_single(inner).await;
                            if i + 1 < n && self.key_or_timeout(1).await {
                                out!(self.term, "stopped after {} of {n}\r\n", i + 1);
                                break;
                            }
                        }
                    }
                    _ => self.term.print("usage: repeat <1..1000> <command>\r\n").await,
                },
                "every" => match count.parse::<f32>() {
                    Ok(s) if (0.1..=3600.0).contains(&s) && Self::wrappable(inner) => loop {
                        self.run_single(inner).await;
                        self.term.flush().await;
                        if self.key_or_timeout((s * 1000.0) as u64).await {
                            break;
                        }
                    },
                    _ => self.term.print("usage: every <0.1..3600 seconds> <command>\r\n").await,
                },
                _ => self.run_single(part).await,
            }
        }
    }

    /// `repeat` and `every` take one plain command (nesting them would need async recursion).
    fn wrappable(cmd: &str) -> bool {
        !cmd.is_empty() && !matches!(split_word(cmd).0, "repeat" | "every")
    }

    async fn run_single(&mut self, line: &str) {
        let rest = split_word(line).1;
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
            "graph" => self.cmd_graph().await,
            "pins" => self.cmd_pins().await,
            "gpio" => self.cmd_gpio(args).await,
            "pwm" => self.cmd_pwm(args).await,
            "fade" => self.cmd_fade(args).await,
            "adc" => self.cmd_adc(args).await,
            "knob" => self.cmd_knob(args).await,
            "i2c" => self.cmd_i2c(args).await,
            "oled" => self.cmd_oled(rest).await,
            "calc" => self.cmd_calc(rest).await,
            // Reached only for misuse like `repeat 2 repeat 3 x`; run_command handles the rest.
            "repeat" | "every" => self.term.print("repeat and every cannot be nested\r\n").await,
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
            "2048" => self.cmd_2048().await,
            "dump" => self.cmd_dump(args).await,
            "say" => fun::say(&mut self.term, args).await,
            "fortune" => self.cmd_fortune().await,
            other => match suggest(other, COMMAND_NAMES) {
                Some(close) => out!(self.term, "unknown command: '{other}' - did you mean '{close}'?\r\n"),
                None => out!(self.term, "unknown command: '{other}' (try 'help')\r\n"),
            },
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
        for (title, commands) in SECTIONS {
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

    async fn cmd_calc(&mut self, expr: &str) {
        if expr.is_empty() {
            self.term.print("usage: calc <expression>, e.g. calc 0x1F << 4 | 3\r\n").await;
            return;
        }
        match calc::eval(expr) {
            Ok(calc::Value::Int(i)) => {
                out!(self.term, "= {i}  ({i:#x}");
                if (0..=0xFFFF_FFFF).contains(&i) {
                    out!(self.term, ", {i:#b}");
                }
                self.term.print(")\r\n").await;
            }
            Ok(calc::Value::Float(f)) => out!(self.term, "= {f}\r\n"),
            Err(e) => {
                out!(self.term, "error: {e}\r\n");
                if let calc::Error::Syntax(at) = e {
                    // Echo the expression and point at the problem.
                    out!(self.term, "  {expr}\r\n  {:>w$}\r\n", "^", w = at + 1);
                }
            }
        }
    }

    async fn cmd_graph(&mut self) {
        const ROWS: usize = 10;
        const BLOCKS: [char; 9] =
            [' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}'];
        self.term.print("\x1b[2J\x1b[?25l").await;
        loop {
            let data: heapless::Vec<f32, HISTORY_SECS> = TEMP_HISTORY.lock(|h| h.borrow().iter().collect());
            let limit = alert_limit_c();
            self.term.print("\x1b[H").await;
            let Some(&now) = data.last() else {
                self.term.print("collecting samples...\x1b[K\r\n").await;
                if self.key_or_timeout(500).await {
                    break;
                }
                continue;
            };
            let (mut lo, mut hi) = data.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
            out!(
                self.term,
                "\x1b[1mtemperature\x1b[0m, last {} s   now {now:.2} C   min {lo:.2}   max {hi:.2}   alert {limit:.1}\x1b[K\r\n\r\n",
                data.len()
            );
            // At least one degree of range, so sensor noise does not fill the whole chart.
            if hi - lo < 1.0 {
                let mid = (hi + lo) / 2.0;
                (lo, hi) = (mid - 0.5, mid + 0.5);
            }
            for row in (0..ROWS).rev() {
                let mut line: String<512> = String::new();
                let label = lo + (hi - lo) * (row + 1) as f32 / ROWS as f32;
                let _ = write!(line, "{label:7.2} \u{2502}");
                // Right-align, so the newest point is always under "now".
                for _ in data.len()..HISTORY_SECS {
                    let _ = line.push(' ');
                }
                let mut red = false;
                for &v in &data {
                    let hot = v > limit;
                    if hot != red {
                        let _ = line.push_str(if hot { "\x1b[31m" } else { "\x1b[36m" });
                        red = hot;
                    } else if line.ends_with('\u{2502}') {
                        let _ = line.push_str("\x1b[36m");
                    }
                    let _ = line.push(BLOCKS[chart_level(v, lo, hi, row, ROWS)]);
                }
                let _ = line.push_str("\x1b[0m\x1b[K\r\n");
                self.term.print(&line).await;
            }
            let mut axis: String<256> = String::new();
            let _ = axis.push_str("        \u{2514}");
            for _ in 0..HISTORY_SECS {
                let _ = axis.push('\u{2500}');
            }
            // Printed in two parts: the box-drawing axis alone nearly fills out!'s buffer.
            self.term.print(&axis).await;
            out!(self.term, "\r\n         -{HISTORY_SECS}s{:>w$}\x1b[K\r\n", "now", w = HISTORY_SECS - 3);
            out!(self.term, "\r\n\x1b[2mone point per second, any key stops\x1b[0m\x1b[K");
            self.term.flush().await;
            if self.key_or_timeout(1000).await {
                break;
            }
        }
        self.term.print("\x1b[?25h\r\n").await;
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
        let dispatch = &source[source.find("async fn run_single").unwrap()..];
        for name in COMMAND_NAMES {
            let arm = format!("\"{name}\" =>");
            let first_of_several = format!("\"{name}\" |");
            let last_of_several = format!("| \"{name}\" =>");
            assert!(
                [arm, first_of_several, last_of_several].iter().any(|p| dispatch.contains(p.as_str())),
                "'{name}' is not handled by run_single"
            );
        }
    }

    #[test]
    fn usage_fits_the_help_column() {
        for c in all_commands() {
            assert!(c.name.len() + 1 + c.args.len() <= 20, "usage of '{}' is too long", c.name);
        }
    }

    #[test]
    fn typos_and_prefixes() {
        assert_eq!(distance("tex", "text"), 1);
        assert_eq!(distance("tmep", "temp"), 1);
        assert_eq!(distance("tmep", "top"), 2);
        assert_eq!(suggest("tmep", COMMAND_NAMES), Some("temp"));
        assert_eq!(suggest("hlep", COMMAND_NAMES), Some("help"));
        assert_eq!(suggest("xyzzy", COMMAND_NAMES), None);
        assert_eq!(resolve("tex", OLED_ACTIONS), Some("text"));
        assert_eq!(resolve("text", OLED_ACTIONS), Some("text"));
        assert_eq!(resolve("te", OLED_ACTIONS), None); // text or temp
        assert_eq!(resolve("on", OLED_ACTIONS), Some("on")); // exact beats the prefix of nothing else
        assert_eq!(resolve("o", OLED_ACTIONS), None); // on or off
    }

    #[test]
    fn splitting_words() {
        assert_eq!(split_word("  led  on x"), ("led", "on x"));
        assert_eq!(split_word("ps"), ("ps", ""));
        assert_eq!(split_word(""), ("", ""));
    }

    #[test]
    fn chart_levels() {
        // A 2-row chart from 0 to 2: 1.0 fills the bottom row and leaves the top empty.
        assert_eq!(chart_level(1.0, 0.0, 2.0, 0, 2), 8);
        assert_eq!(chart_level(1.0, 0.0, 2.0, 1, 2), 0);
        assert_eq!(chart_level(1.5, 0.0, 2.0, 1, 2), 4);
        assert_eq!(chart_level(-5.0, 0.0, 2.0, 0, 2), 0);
        assert_eq!(chart_level(9.0, 0.0, 2.0, 1, 2), 8);
    }

    #[test]
    fn completion_contexts() {
        assert_eq!(complete("").len(), COMMAND_NAMES.len());
        assert_eq!(complete("help ").len(), COMMAND_NAMES.len());
        assert_eq!(complete("led "), LED_MODES);
        assert!(complete("led blink ").is_empty());
        assert!(complete("nope ").is_empty());
        // After `;` and inside repeat / every, completion starts over with a command.
        assert_eq!(complete("temp; ").len(), COMMAND_NAMES.len());
        assert_eq!(complete("repeat 3 ").len(), COMMAND_NAMES.len());
        assert_eq!(complete("every 2 led "), LED_MODES);
        assert_eq!(complete("gpio 16 "), GPIO_ACTIONS);
        assert!(complete("gpio 16 read ").is_empty());
        assert_eq!(complete("ps; repeat 3 led "), LED_MODES);
        assert!(complete("repeat ").is_empty()); // the count comes first
    }
}
