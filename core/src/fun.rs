//! The fun (and slightly eccentric) shell commands.
//!
//! Each one shows off a piece of the platform or of the async runtime:
//!   roll / fortune / matrix / life -> the platform's random number generator
//!   reaction                      -> button timestamps + task coordination
//!   timer                         -> spawning new tasks at runtime from a task pool
//!   mandel                        -> floating point speed, timed in CPU cycles
//!   dump                          -> raw memory access (flash, RAM, factory data)
//!   say                           -> Ferris the crab, because Rust

use core::fmt::Write as _;

use embassy_futures::select::{Either3, select3};
use embassy_time::{Instant, Timer};
use embedded_io_async::{Read, Write};
use heapless::String;
use portable_atomic::{AtomicU32, Ordering};

use crate::platform::System;
use crate::shell::Shell;
use crate::state::{BUTTON_SIGNAL, EVENTS, Event, GAME_ACTIVE, LedMode, current_led_mode, set_led_mode};
use crate::term::Term;

pub const TIMER_SLOTS: u32 = 4;
pub static ACTIVE_TIMERS: AtomicU32 = AtomicU32::new(0);
static NEXT_TIMER_ID: AtomicU32 = AtomicU32::new(1);
static BEST_REACTION_MS: AtomicU32 = AtomicU32::new(u32::MAX);

/// Up to TIMER_SLOTS of these can exist at once. Their memory is reserved at compile time,
/// so spawning one never allocates.
#[embassy_executor::task(pool_size = 4)]
async fn countdown_task(id: u32, secs: u32, label: String<24>) {
    ACTIVE_TIMERS.fetch_add(1, Ordering::Relaxed);
    Timer::after_secs(secs as u64).await;
    ACTIVE_TIMERS.fetch_sub(1, Ordering::Relaxed);
    EVENTS.send(Event::Timer { id, secs, label }).await;
}

pub(crate) fn parse_dice(spec: &str) -> Option<(u32, u32)> {
    let (count, sides) = spec.split_once(['d', 'D'])?;
    let count = if count.is_empty() { 1 } else { count.parse().ok()? };
    let sides = sides.parse().ok()?;
    Some((count, sides))
}

pub(crate) fn parse_number(s: &str) -> Option<usize> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => usize::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

/// One step of Conway's Game of Life on a torus.
pub(crate) fn life_step<const W: usize, const H: usize>(grid: &[[bool; W]; H]) -> [[bool; W]; H] {
    let mut next = [[false; W]; H];
    for y in 0..H {
        for x in 0..W {
            let mut neighbours = 0;
            for dy in [H - 1, 0, 1] {
                for dx in [W - 1, 0, 1] {
                    if (dy, dx) != (0, 0) && grid[(y + dy) % H][(x + dx) % W] {
                        neighbours += 1;
                    }
                }
            }
            next[y][x] = matches!((grid[y][x], neighbours), (true, 2) | (_, 3));
        }
    }
    next
}

/// A stopwatch that uses the CPU cycle counter when the platform has one.
struct Stopwatch {
    started: Instant,
    cycles_at_start: Option<u32>,
}

impl<S: System, R: Read, W: Write> Shell<S, R, W> {
    fn rand_range(&mut self, lo: u32, hi: u32) -> u32 {
        lo + self.sys.random_u32() % (hi - lo)
    }

    fn stopwatch(&self) -> Stopwatch {
        Stopwatch { started: Instant::now(), cycles_at_start: self.sys.cycle_counter() }
    }

    /// Returns (elapsed cycles if available, elapsed microseconds).
    fn elapsed(&self, sw: &Stopwatch) -> (Option<u32>, u64) {
        let cycles = match (sw.cycles_at_start, self.sys.cycle_counter()) {
            (Some(start), Some(now)) => Some(now.wrapping_sub(start)),
            _ => None,
        };
        (cycles, sw.started.elapsed().as_micros())
    }

    // -----------------------------------------------------------------------
    // roll
    // -----------------------------------------------------------------------

    pub(crate) async fn cmd_roll(&mut self, spec: Option<&str>) {
        let Some((count, sides)) = parse_dice(spec.unwrap_or("1d6")) else {
            self.term.print("usage: roll [N]dM   e.g. roll 2d6, roll d20\r\n").await;
            return;
        };
        if !(1..=20).contains(&count) || !(2..=1000).contains(&sides) {
            self.term.print("between 1 and 20 dice, with 2 to 1000 sides please\r\n").await;
            return;
        }

        const FACES: [&str; 6] = ["\u{2680}", "\u{2681}", "\u{2682}", "\u{2683}", "\u{2684}", "\u{2685}"];
        let mut line: String<256> = String::new();
        let mut total = 0;
        for _ in 0..count {
            let value = self.rand_range(1, sides + 1);
            total += value;
            if sides == 6 {
                let _ = write!(line, "{} ", FACES[value as usize - 1]);
            } else {
                let _ = write!(line, "[{value}] ");
            }
        }
        out!(self.term, "\x1b[1;33m{}\x1b[0m = {total}\r\n", line.as_str());
    }

    // -----------------------------------------------------------------------
    // reaction
    // -----------------------------------------------------------------------

    pub(crate) async fn cmd_reaction(&mut self) {
        let button = self.sys.info().button;
        out!(self.term, "\x1b[1mREACTION TEST\x1b[0m - press {button} as soon as the LED turns ON\r\n");
        self.term.print("get ready... (any key cancels)\r\n").await;
        self.term.flush().await;

        let previous_mode = current_led_mode();
        // While the game runs, the button task reports presses to us instead of changing LED modes.
        GAME_ACTIVE.store(true, Ordering::Relaxed);
        set_led_mode(LedMode::Off);
        BUTTON_SIGNAL.reset();

        let delay = self.rand_range(1500, 5000) as u64;
        let mut buf = [0u8; 8];
        match select3(Timer::after_millis(delay), BUTTON_SIGNAL.wait(), self.rx.read(&mut buf)).await {
            Either3::First(()) => {
                set_led_mode(LedMode::On);
                let start = Instant::now();
                self.term.print("\x1b[1;32mGO!\x1b[0m\r\n").await;
                self.term.flush().await;

                match select3(BUTTON_SIGNAL.wait(), Timer::after_secs(3), self.rx.read(&mut buf)).await {
                    Either3::First(pressed_at) => {
                        // The timestamp is taken in the button task the moment it wakes up.
                        let ms = pressed_at.saturating_duration_since(start).as_millis() as u32;
                        let best = BEST_REACTION_MS.fetch_min(ms, Ordering::Relaxed).min(ms);
                        let verdict = match ms {
                            0..=149 => "superhuman (or you guessed)",
                            150..=199 => "lightning fast",
                            200..=249 => "great reflexes",
                            250..=349 => "pretty good",
                            _ => "did you fall asleep?",
                        };
                        out!(self.term, "\x1b[1;36m{ms} ms\x1b[0m - {verdict}   (best: {best} ms)\r\n");
                    }
                    Either3::Second(()) => self.term.print("too slow, 3 seconds passed\r\n").await,
                    Either3::Third(_) => self.term.print("cancelled\r\n").await,
                }
            }
            Either3::Second(_) => {
                self.term.print("\x1b[1;31mfalse start!\x1b[0m you pressed before the LED turned on\r\n").await
            }
            Either3::Third(_) => self.term.print("cancelled\r\n").await,
        }

        GAME_ACTIVE.store(false, Ordering::Relaxed);
        set_led_mode(previous_mode);
    }

    // -----------------------------------------------------------------------
    // timer
    // -----------------------------------------------------------------------

    pub(crate) async fn cmd_timer<'a>(&mut self, mut args: impl Iterator<Item = &'a str>) {
        let secs = args.next().and_then(|s| s.parse::<u32>().ok()).filter(|s| (1..=86_400).contains(s));
        let Some(secs) = secs else {
            self.term.print("usage: timer <seconds> [label]   e.g. timer 10 tea is ready\r\n").await;
            return;
        };
        let mut label: String<24> = String::new();
        for (i, word) in args.enumerate() {
            if i > 0 {
                let _ = label.push(' ');
            }
            let _ = label.push_str(word);
        }
        if label.is_empty() {
            let _ = label.push_str("timer");
        }

        let id = NEXT_TIMER_ID.load(Ordering::Relaxed);
        match countdown_task(id, secs, label.clone()) {
            Ok(token) => {
                NEXT_TIMER_ID.fetch_add(1, Ordering::Relaxed);
                self.spawner.spawn(token);
                out!(
                    self.term,
                    "spawned task \x1b[1mcountdown#{id}\x1b[0m: '{}' fires in {secs} s\r\n",
                    label.as_str()
                );
            }
            Err(_) => out!(self.term, "task pool is full: all {TIMER_SLOTS} countdown slots are running\r\n"),
        }
    }

    // -----------------------------------------------------------------------
    // mandel
    // -----------------------------------------------------------------------

    pub(crate) async fn cmd_mandel(&mut self) {
        const W: usize = 72;
        const H: usize = 26;
        const MAX_ITER: u32 = 100;
        const RAINBOW: [u8; 24] =
            [196, 202, 208, 214, 220, 226, 190, 154, 118, 82, 46, 47, 48, 49, 50, 51, 45, 39, 33, 27, 21, 57, 93, 129];

        let mut cycles: u64 = 0;
        let mut micros: u64 = 0;
        let mut iterations: u64 = 0;
        for row in 0..H {
            let mut line: String<1536> = String::new();
            let sw = self.stopwatch();
            let ci = -1.15 + 2.3 * row as f32 / (H - 1) as f32;
            let mut last_color = None;
            for col in 0..W {
                let cr = -2.2 + 3.0 * col as f32 / (W - 1) as f32;
                let (mut zr, mut zi, mut n) = (0.0f32, 0.0f32, 0u32);
                while n < MAX_ITER && zr * zr + zi * zi <= 4.0 {
                    let t = zr * zr - zi * zi + cr;
                    zi = 2.0 * zr * zi + ci;
                    zr = t;
                    n += 1;
                }
                iterations += n as u64;
                // Inside the set, or escaping almost immediately: leave it blank.
                if n == MAX_ITER || n <= 2 {
                    let _ = line.push(' ');
                } else {
                    let color = RAINBOW[n as usize % RAINBOW.len()];
                    if last_color != Some(color) {
                        let _ = write!(line, "\x1b[38;5;{color}m");
                        last_color = Some(color);
                    }
                    let _ = line.push('\u{2588}');
                }
            }
            let (c, us) = self.elapsed(&sw);
            cycles += c.unwrap_or(0) as u64;
            micros += us;
            let _ = line.push_str("\x1b[0m\r\n");
            // Awaiting the output yields to the executor, so other tasks keep running between rows.
            self.term.print(&line).await;
        }

        let info = self.sys.info();
        let float = if info.fpu { "hardware FPU" } else { "software floats (no FPU)" };
        match (self.sys.cycle_counter(), info.cpu_mhz) {
            (Some(_), Some(mhz)) => out!(
                self.term,
                "{iterations} iterations in {cycles} CPU cycles = {:.1} ms at {mhz} MHz, {float}\r\n",
                cycles as f32 / (mhz as f32 * 1000.0)
            ),
            _ => out!(self.term, "{iterations} iterations in {:.1} ms of compute, {float}\r\n", micros as f32 / 1000.0),
        }
    }

    // -----------------------------------------------------------------------
    // life
    // -----------------------------------------------------------------------

    pub(crate) async fn cmd_life(&mut self) {
        const W: usize = 56;
        const H: usize = 28; // drawn with half blocks, so 14 text lines
        let mut grid = [[false; W]; H];
        for row in grid.iter_mut() {
            for cell in row.iter_mut() {
                *cell = self.sys.random_u32() % 100 < 30;
            }
        }

        self.term.print("\x1b[2J\x1b[?25l").await; // clear screen, hide cursor
        for generation in 0.. {
            let mut alive = 0;
            self.term.print("\x1b[H\x1b[32m").await;
            for pair in 0..H / 2 {
                let mut line: String<256> = String::new();
                for col in 0..W {
                    let (top, bottom) = (grid[pair * 2][col], grid[pair * 2 + 1][col]);
                    alive += top as u32 + bottom as u32;
                    let _ = line.push(match (top, bottom) {
                        (true, true) => '\u{2588}',
                        (true, false) => '\u{2580}',
                        (false, true) => '\u{2584}',
                        (false, false) => ' ',
                    });
                }
                let _ = line.push_str("\r\n");
                self.term.print(&line).await;
            }
            out!(self.term, "\x1b[0mgeneration {generation:<5} alive {alive:<5} (any key stops)\x1b[K\r\n");
            self.term.flush().await;

            let next = life_step(&grid);
            let stable = next == grid;
            grid = next;
            if alive == 0 || stable || self.key_or_timeout(60).await {
                break;
            }
        }
        self.term.print("\x1b[?25h").await; // show the cursor again
    }

    // -----------------------------------------------------------------------
    // matrix
    // -----------------------------------------------------------------------

    pub(crate) async fn cmd_matrix(&mut self) {
        const W: usize = 72;
        let mut remaining = [0u8; W];
        let mut head = [false; W];
        self.term.print("\x1b[2J\x1b[H\x1b[?25l").await;
        loop {
            let mut line: String<1024> = String::new();
            let mut color = 0u8;
            for col in 0..W {
                if remaining[col] == 0 && self.sys.random_u32() % 30 == 0 {
                    remaining[col] = self.rand_range(4, 22) as u8;
                    head[col] = true;
                }
                if remaining[col] == 0 {
                    let _ = line.push(' ');
                    continue;
                }
                let wanted = if head[col] {
                    1
                } else if remaining[col] < 4 {
                    3
                } else {
                    2
                };
                if wanted != color {
                    let _ = line.push_str(match wanted {
                        1 => "\x1b[1;97m", // bright white head
                        2 => "\x1b[0;92m", // green body
                        _ => "\x1b[0;32m", // dim tail
                    });
                    color = wanted;
                }
                // Half-width katakana (U+FF66..U+FF9D) or a digit.
                let r = self.sys.random_u32();
                let ch = if r % 4 == 0 {
                    char::from(b'0' + (r % 10) as u8)
                } else {
                    char::from_u32(0xFF66 + (r >> 8) % 56).unwrap_or('*')
                };
                let _ = line.push(ch);
                head[col] = false;
                remaining[col] -= 1;
            }
            let _ = line.push_str("\x1b[0m\r\n");
            self.term.print(&line).await;
            self.term.flush().await;
            if self.key_or_timeout(45).await {
                break;
            }
        }
        self.term.print("\x1b[0m\x1b[?25h\r\nwake up, Neo...\r\n").await;
    }

    // -----------------------------------------------------------------------
    // dump
    // -----------------------------------------------------------------------

    async fn print_regions(&mut self) {
        for i in 0..self.sys.memory_regions().len() {
            let region = self.sys.memory_regions()[i];
            out!(self.term, "  0x{:08X}..0x{:08X}  {}\r\n", region.start, region.end, region.name);
        }
    }

    pub(crate) async fn cmd_dump<'a>(&mut self, mut args: impl Iterator<Item = &'a str>) {
        let Some(addr) = args.next().and_then(parse_number) else {
            self.term.print("usage: dump <addr> [len]   readable regions on this board:\r\n").await;
            self.print_regions().await;
            return;
        };
        let len = args.next().and_then(parse_number).unwrap_or(64).clamp(1, 256);
        let region =
            self.sys.memory_regions().iter().find(|r| addr >= r.start && addr.saturating_add(len) <= r.end).copied();
        let Some(region) = region else {
            self.term.print("outside the readable regions (a random address can fault). allowed:\r\n").await;
            self.print_regions().await;
            return;
        };

        out!(self.term, "\x1b[2m{len} bytes of {}\x1b[0m\r\n", region.name);
        for row_start in (addr..addr + len).step_by(16) {
            let row_len = (addr + len - row_start).min(16);
            let mut hex: String<64> = String::new();
            let mut ascii: String<32> = String::new();
            for i in 0..row_len {
                // SAFETY: the whole range was checked against the board's readable regions.
                let byte = unsafe { self.sys.read_byte(row_start + i) };
                let _ = write!(hex, "{byte:02x} ");
                let _ = ascii.push(if byte.is_ascii_graphic() || byte == b' ' { byte as char } else { '.' });
            }
            out!(
                self.term,
                "\x1b[36m{row_start:08x}\x1b[0m  {:<48} \x1b[33m|{}|\x1b[0m\r\n",
                hex.as_str(),
                ascii.as_str()
            );
        }
    }

    // -----------------------------------------------------------------------
    // fortune
    // -----------------------------------------------------------------------

    pub(crate) async fn cmd_fortune(&mut self) {
        const FORTUNES: [&str; 13] = [
            "Premature optimization is the root of all evil. -- Donald Knuth",
            "Simplicity is prerequisite for reliability. -- Edsger W. Dijkstra",
            "Talk is cheap. Show me the code. -- Linus Torvalds",
            "There are only two hard things in Computer Science: cache invalidation and naming things. -- Phil Karlton",
            "The best code is no code at all. -- Jeff Atwood",
            "In embedded, the hardware is guilty until proven innocent.",
            "A watchdog is just a friend who checks that you are still alive.",
            "Never trust a datasheet you have not measured. (We found a swapped bit in a PAC once.)",
            "async fn main() { loop { coffee().await; } }",
            "If it compiles, it probably works. If it is Rust, it probably really works.",
            "Every blocking loop in a cooperative scheduler is a small act of betrayal.",
            "Undefined behaviour is the compiler's way of saying: you are on your own.",
            "Yanardag has been burning for thousands of years without a single reboot.",
        ];
        let pick = FORTUNES[self.sys.random_u32() as usize % FORTUNES.len()];
        out!(self.term, "\x1b[3;35m{pick}\x1b[0m\r\n");
    }
}

// ---------------------------------------------------------------------------
// say - Ferris the crab says things
// ---------------------------------------------------------------------------

pub(crate) async fn say<'a, W: Write>(term: &mut Term<W>, words: impl Iterator<Item = &'a str>) {
    const WIDTH: usize = 36;
    let mut lines: heapless::Vec<String<WIDTH>, 6> = heapless::Vec::new();
    let mut current: String<WIDTH> = String::new();
    for word in words {
        let word = &word[..word.floor_char_boundary(WIDTH)];
        let too_long = !current.is_empty() && current.len() + 1 + word.len() > WIDTH;
        if too_long && lines.push(core::mem::take(&mut current)).is_err() {
            break;
        }
        if !current.is_empty() {
            let _ = current.push(' ');
        }
        let _ = current.push_str(word);
    }
    if !current.is_empty() {
        let _ = lines.push(current);
    }
    if lines.is_empty() {
        let _ = lines.push(String::try_from("Hello from yanarkaze!").unwrap_or_default());
    }

    let width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let mut border: String<{ WIDTH + 2 }> = String::new();
    for _ in 0..width + 2 {
        let _ = border.push('_');
    }
    out!(term, " {}\r\n", border.as_str());
    for (i, line) in lines.iter().enumerate() {
        let (l, r) = match (lines.len(), i) {
            (1, _) => ('<', '>'),
            (_, 0) => ('/', '\\'),
            (n, i) if i == n - 1 => ('\\', '/'),
            _ => ('|', '|'),
        };
        out!(term, "{l} {:<width$} {r}\r\n", line.as_str());
    }
    border.clear();
    for _ in 0..width + 2 {
        let _ = border.push('-');
    }
    out!(term, " {}\r\n", border.as_str());
    term.print_lines(&[
        "        \\",
        "         \\",
        "\x1b[38;5;208m            _~^~^~_",
        "        \\) /  o o  \\ (/",
        "          '_   -   _'",
        "          / '-----' \\\x1b[0m",
    ])
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dice_specs() {
        assert_eq!(parse_dice("2d6"), Some((2, 6)));
        assert_eq!(parse_dice("d20"), Some((1, 20)));
        assert_eq!(parse_dice("3D8"), Some((3, 8)));
        assert_eq!(parse_dice("6"), None);
        assert_eq!(parse_dice("xdy"), None);
    }

    #[test]
    fn numbers_in_hex_and_decimal() {
        assert_eq!(parse_number("0x1FFF7590"), Some(0x1FFF_7590));
        assert_eq!(parse_number("64"), Some(64));
        assert_eq!(parse_number("0xZZ"), None);
    }

    #[test]
    fn blinker_oscillates() {
        let mut grid = [[false; 5]; 5];
        grid[2][1] = true;
        grid[2][2] = true;
        grid[2][3] = true;
        let next = life_step(&grid);
        assert!(next[1][2] && next[2][2] && next[3][2]);
        assert!(!next[2][1] && !next[2][3]);
        assert_eq!(life_step(&next), grid);
    }

    #[test]
    fn morse_table() {
        assert_eq!(crate::led::morse_code('s'), "...");
        assert_eq!(crate::led::morse_code('O'), "---");
        assert_eq!(crate::led::morse_code('?'), "");
    }
}
