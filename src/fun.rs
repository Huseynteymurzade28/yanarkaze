//! The fun (and slightly eccentric) shell commands.
//!
//! Each one shows off a piece of the chip or of the async runtime:
//!   roll / fortune / matrix / life -> hardware true random number generator (TRNG)
//!   reaction                      -> EXTI interrupt timestamps + task coordination
//!   timer                         -> spawning new tasks at runtime from a task pool
//!   mandel                        -> hardware FPU + DWT cycle counter
//!   dump                          -> raw memory access (flash, SRAM, factory data)
//!   say                           -> Ferris the crab, because Rust

use core::fmt::Write as _;
use core::sync::atomic::{AtomicU32, Ordering};

use cortex_m::peripheral::DWT;
use embassy_executor::Spawner;
use embassy_futures::select::{Either, Either3, select, select3};
use embassy_stm32::peripherals::RNG;
use embassy_stm32::rng::Rng;
use embassy_stm32::usart::RingBufferedUartRx;
use embassy_time::{Instant, Timer};
use heapless::String;

use crate::{BUTTON_SIGNAL, CURRENT_LED_MODE, EVENTS, Event, GAME_ACTIVE, LedMode, Term, set_led_mode};

pub type Rx = RingBufferedUartRx<'static>;

/// Resources only the fun commands need.
pub struct Ctx {
    pub rng: Rng<'static, RNG>,
    pub spawner: Spawner,
}

impl Ctx {
    pub fn rand(&mut self) -> u32 {
        self.rng.next_u32()
    }

    /// Random number in `lo..hi`.
    pub fn rand_range(&mut self, lo: u32, hi: u32) -> u32 {
        lo + self.rand() % (hi - lo)
    }
}

/// Waits `ms` milliseconds. Returns true if a key was pressed in the meantime.
async fn key_or_timeout(rx: &mut Rx, ms: u64) -> bool {
    let mut buf = [0u8; 8];
    matches!(select(rx.read(&mut buf), Timer::after_millis(ms)).await, Either::First(_))
}

// ---------------------------------------------------------------------------
// roll - dice from the hardware TRNG
// ---------------------------------------------------------------------------

fn parse_dice(spec: &str) -> Option<(u32, u32)> {
    let (count, sides) = spec.split_once(['d', 'D'])?;
    let count = if count.is_empty() { 1 } else { count.parse().ok()? };
    let sides = sides.parse().ok()?;
    Some((count, sides))
}

pub async fn roll(term: &mut Term, ctx: &mut Ctx, spec: Option<&str>) {
    let Some((count, sides)) = parse_dice(spec.unwrap_or("1d6")) else {
        term.print("usage: roll [N]dM   e.g. roll 2d6, roll d20\r\n").await;
        return;
    };
    if !(1..=20).contains(&count) || !(2..=1000).contains(&sides) {
        term.print("between 1 and 20 dice, with 2 to 1000 sides please\r\n").await;
        return;
    }

    const FACES: [&str; 6] = ["\u{2680}", "\u{2681}", "\u{2682}", "\u{2683}", "\u{2684}", "\u{2685}"];
    let mut line: String<256> = String::new();
    let mut total = 0;
    for _ in 0..count {
        let value = ctx.rand_range(1, sides + 1);
        total += value;
        if sides == 6 {
            let _ = write!(line, "{} ", FACES[value as usize - 1]);
        } else {
            let _ = write!(line, "[{value}] ");
        }
    }
    out!(term, "\x1b[1;33m{}\x1b[0m = {total}\r\n", line.as_str());
    term.print("\x1b[2m(true random numbers from the STM32 hardware RNG)\x1b[0m\r\n").await;
}

// ---------------------------------------------------------------------------
// reaction - a reflex game with the LED and the blue button
// ---------------------------------------------------------------------------

static BEST_REACTION_MS: AtomicU32 = AtomicU32::new(u32::MAX);

pub async fn reaction(term: &mut Term, rx: &mut Rx, ctx: &mut Ctx) {
    term.print("\x1b[1mREACTION TEST\x1b[0m - press the blue button as soon as the LED turns ON\r\n").await;
    term.print("get ready... (any key cancels)\r\n").await;

    let previous_mode = CURRENT_LED_MODE.lock(|m| m.borrow().clone());
    // While the game runs, the button task reports presses to us instead of changing LED modes.
    GAME_ACTIVE.store(true, Ordering::Relaxed);
    set_led_mode(LedMode::Off);
    BUTTON_SIGNAL.reset();

    let delay = ctx.rand_range(1500, 5000) as u64;
    let mut buf = [0u8; 8];
    match select3(Timer::after_millis(delay), BUTTON_SIGNAL.wait(), rx.read(&mut buf)).await {
        Either3::First(()) => {
            set_led_mode(LedMode::On);
            let start = Instant::now();
            term.print("\x1b[1;32mGO!\x1b[0m\r\n").await;

            match select3(BUTTON_SIGNAL.wait(), Timer::after_secs(3), rx.read(&mut buf)).await {
                Either3::First(pressed_at) => {
                    // The timestamp is taken in the button task right when the EXTI interrupt wakes it.
                    let ms = (pressed_at - start).as_millis() as u32;
                    let best = BEST_REACTION_MS.fetch_min(ms, Ordering::Relaxed).min(ms);
                    let verdict = match ms {
                        0..=149 => "superhuman (or you guessed)",
                        150..=199 => "lightning fast",
                        200..=249 => "great reflexes",
                        250..=349 => "pretty good",
                        _ => "did you fall asleep?",
                    };
                    out!(term, "\x1b[1;36m{ms} ms\x1b[0m - {verdict}   (best: {best} ms)\r\n");
                }
                Either3::Second(()) => term.print("too slow, 3 seconds passed\r\n").await,
                Either3::Third(_) => term.print("cancelled\r\n").await,
            }
        }
        Either3::Second(_) => term.print("\x1b[1;31mfalse start!\x1b[0m you pressed before the LED turned on\r\n").await,
        Either3::Third(_) => term.print("cancelled\r\n").await,
    }

    GAME_ACTIVE.store(false, Ordering::Relaxed);
    set_led_mode(previous_mode);
}

// ---------------------------------------------------------------------------
// timer - spawn a brand new task at runtime
// ---------------------------------------------------------------------------

pub const TIMER_SLOTS: u32 = 4;
pub static ACTIVE_TIMERS: AtomicU32 = AtomicU32::new(0);
static NEXT_TIMER_ID: AtomicU32 = AtomicU32::new(1);

/// Up to TIMER_SLOTS of these can exist at once. Their memory is reserved at compile
/// time, so spawning one never allocates.
#[embassy_executor::task(pool_size = 4)]
async fn countdown_task(id: u32, secs: u32, label: String<24>) {
    ACTIVE_TIMERS.fetch_add(1, Ordering::Relaxed);
    Timer::after_secs(secs as u64).await;
    ACTIVE_TIMERS.fetch_sub(1, Ordering::Relaxed);
    EVENTS.send(Event::Timer { id, secs, label }).await;
}

pub async fn timer<'a>(term: &mut Term, ctx: &mut Ctx, mut args: impl Iterator<Item = &'a str>) {
    let Some(secs) = args.next().and_then(|s| s.parse::<u32>().ok()).filter(|s| (1..=86_400).contains(s)) else {
        term.print("usage: timer <seconds> [label]   e.g. timer 10 tea is ready\r\n").await;
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
            ctx.spawner.spawn(token);
            out!(term, "spawned task \x1b[1mcountdown#{id}\x1b[0m: '{}' fires in {secs} s\r\n", label.as_str());
        }
        Err(_) => out!(term, "task pool is full: all {TIMER_SLOTS} countdown slots are running\r\n"),
    }
}

// ---------------------------------------------------------------------------
// mandel - Mandelbrot set on the FPU, timed with the DWT cycle counter
// ---------------------------------------------------------------------------

pub async fn mandel(term: &mut Term) {
    const W: usize = 72;
    const H: usize = 26;
    const MAX_ITER: u32 = 100;
    const RAINBOW: [u8; 24] = [
        196, 202, 208, 214, 220, 226, 190, 154, 118, 82, 46, 47, 48, 49, 50, 51, 45, 39, 33, 27, 21, 57, 93, 129,
    ];

    let mut cycles: u64 = 0;
    let mut iterations: u64 = 0;
    for row in 0..H {
        let mut line: String<1536> = String::new();
        let started = DWT::cycle_count();
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
        cycles += DWT::cycle_count().wrapping_sub(started) as u64;
        let _ = line.push_str("\x1b[0m\r\n");
        // Awaiting the UART here yields to the executor, so other tasks keep running between rows.
        term.print(&line).await;
    }
    out!(
        term,
        "{iterations} iterations in {cycles} CPU cycles = {:.1} ms of compute on the FPU at 80 MHz\r\n",
        cycles as f32 / 80_000.0
    );
}

// ---------------------------------------------------------------------------
// life - Conway's Game of Life, seeded from the TRNG
// ---------------------------------------------------------------------------

pub async fn life(term: &mut Term, rx: &mut Rx, ctx: &mut Ctx) {
    const W: usize = 56;
    const H: usize = 28; // drawn with half blocks, so 14 text lines
    let mut grid = [[false; W]; H];
    for row in grid.iter_mut() {
        for cell in row.iter_mut() {
            *cell = ctx.rand() % 100 < 30;
        }
    }

    term.print("\x1b[2J\x1b[?25l").await; // clear screen, hide cursor
    for generation in 0.. {
        let mut alive = 0;
        term.print("\x1b[H\x1b[32m").await;
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
            term.print(&line).await;
        }
        out!(term, "\x1b[0mgeneration {generation:<5} alive {alive:<5} (any key stops)\x1b[K\r\n");

        // Next generation on a torus (edges wrap around).
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
        let stable = next == grid;
        grid = next;

        if alive == 0 || stable || key_or_timeout(rx, 60).await {
            break;
        }
    }
    term.print("\x1b[?25h").await; // show the cursor again
}

// ---------------------------------------------------------------------------
// matrix - digital rain
// ---------------------------------------------------------------------------

pub async fn matrix(term: &mut Term, rx: &mut Rx, ctx: &mut Ctx) {
    const W: usize = 72;
    let mut remaining = [0u8; W];
    let mut head = [false; W];
    term.print("\x1b[2J\x1b[H\x1b[?25l").await;
    loop {
        let mut line: String<1024> = String::new();
        let mut color = 0u8;
        for col in 0..W {
            if remaining[col] == 0 && ctx.rand() % 30 == 0 {
                remaining[col] = ctx.rand_range(4, 22) as u8;
                head[col] = true;
            }
            if remaining[col] == 0 {
                let _ = line.push(' ');
                continue;
            }
            let wanted = if head[col] { 1 } else if remaining[col] < 4 { 3 } else { 2 };
            if wanted != color {
                let _ = line.push_str(match wanted {
                    1 => "\x1b[1;97m", // bright white head
                    2 => "\x1b[0;92m", // green body
                    _ => "\x1b[0;32m", // dim tail
                });
                color = wanted;
            }
            // Half-width katakana (U+FF66..U+FF9D) or a digit.
            let r = ctx.rand();
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
        term.print(&line).await;
        if key_or_timeout(rx, 45).await {
            break;
        }
    }
    term.print("\x1b[0m\x1b[?25h\r\nwake up, Neo...\r\n").await;
}

// ---------------------------------------------------------------------------
// dump - hex dump of real memory
// ---------------------------------------------------------------------------

/// Regions that are safe to read on the STM32L452 (anything else may HardFault).
const READABLE: [(u32, u32, &str); 4] = [
    (0x0800_0000, 0x0808_0000, "flash"),
    (0x1FFF_0000, 0x1FFF_7400, "system memory / OTP"),
    (0x1FFF_7500, 0x1FFF_7600, "factory data (UID, calibration)"),
    (0x2000_0000, 0x2002_8000, "SRAM"),
];

fn parse_number(s: &str) -> Option<u32> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

pub async fn dump<'a>(term: &mut Term, mut args: impl Iterator<Item = &'a str>) {
    let Some(addr) = args.next().and_then(parse_number) else {
        term.print("usage: dump <addr> [len]\r\n").await;
        term.print("  dump 0x1FFF7590 12   the chip's unique ID\r\n").await;
        term.print("  dump 0x08000000      vector table (stack pointer, reset handler)\r\n").await;
        term.print("  dump 0x20000000      start of RAM\r\n").await;
        return;
    };
    let len = args.next().and_then(parse_number).unwrap_or(64).clamp(1, 256);
    let Some(&(_, _, region)) = READABLE.iter().find(|(start, end, _)| addr >= *start && addr + len <= *end) else {
        term.print("outside the allowed regions (a random address can HardFault). allowed:\r\n").await;
        for (start, end, name) in READABLE {
            out!(term, "  0x{start:08X}..0x{end:08X}  {name}\r\n");
        }
        return;
    };

    out!(term, "\x1b[2m{len} bytes of {region}\x1b[0m\r\n");
    for row_start in (addr..addr + len).step_by(16) {
        let row_len = (addr + len - row_start).min(16);
        let mut hex: String<64> = String::new();
        let mut ascii: String<32> = String::new();
        for i in 0..row_len {
            let byte = unsafe { core::ptr::read_volatile((row_start + i) as *const u8) };
            let _ = write!(hex, "{byte:02x} ");
            let _ = ascii.push(if byte.is_ascii_graphic() || byte == b' ' { byte as char } else { '.' });
        }
        out!(term, "\x1b[36m{row_start:08x}\x1b[0m  {:<48} \x1b[33m|{}|\x1b[0m\r\n", hex.as_str(), ascii.as_str());
    }
}

// ---------------------------------------------------------------------------
// say - Ferris the crab says things
// ---------------------------------------------------------------------------

pub async fn say<'a>(term: &mut Term, words: impl Iterator<Item = &'a str>) {
    const WIDTH: usize = 36;
    let mut lines: heapless::Vec<String<WIDTH>, 6> = heapless::Vec::new();
    let mut current: String<WIDTH> = String::new();
    for word in words {
        let word = &word[..word.len().min(WIDTH)];
        if !current.is_empty() && current.len() + 1 + word.len() > WIDTH {
            if lines.push(core::mem::take(&mut current)).is_err() {
                break;
            }
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
        let _ = lines.push(String::try_from("Hello from NucleOS!").unwrap_or_default());
    }

    let width = lines.iter().map(|l| l.len()).max().unwrap_or(0);
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

// ---------------------------------------------------------------------------
// fortune
// ---------------------------------------------------------------------------

const FORTUNES: [&str; 12] = [
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
];

pub async fn fortune(term: &mut Term, ctx: &mut Ctx) {
    let pick = FORTUNES[ctx.rand() as usize % FORTUNES.len()];
    out!(term, "\x1b[3;35m{pick}\x1b[0m\r\n");
}
