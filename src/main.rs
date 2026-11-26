//! NucleOS - a tiny async "operating system" demo for the NUCLEO-L452RE,
//! built on the Embassy async executor.
//!
//! Concurrent tasks:
//!   - shell    : interactive command line over the ST-LINK virtual COM port
//!   - led      : runs LED patterns (blink, heartbeat, morse) without blocking anyone
//!   - button   : interrupt-driven user button (EXTI), debounced, cycles LED modes
//!   - sensor   : samples the on-chip temperature sensor and VDDA, raises alerts
//!   - watchdog : feeds the independent watchdog; if the executor stalls, the chip resets
//!
//! Tasks talk to each other through embassy-sync primitives (Signal, Channel, Mutex).

#![no_std]
#![no_main]

use core::cell::{Cell, RefCell};
use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_stm32::adc::{Adc, SampleTime};
use embassy_stm32::exti::{self, ExtiInput};
use embassy_stm32::gpio::{Level, Output, Pull, Speed};
use embassy_stm32::mode::Async;
use embassy_stm32::peripherals::ADC1;
use embassy_stm32::rng::{self, Rng};
use embassy_stm32::usart::{self, RingBufferedUartRx, Uart, UartTx};
use embassy_stm32::wdg::IndependentWatchdog;
use embassy_stm32::{Config, Peri, bind_interrupts, interrupt, pac, peripherals, rcc};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use heapless::String;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USART2 => usart::InterruptHandler<peripherals::USART2>;
    DMA1_CHANNEL6 => embassy_stm32::dma::InterruptHandler<peripherals::DMA1_CH6>;
    DMA1_CHANNEL7 => embassy_stm32::dma::InterruptHandler<peripherals::DMA1_CH7>;
    RNG => rng::InterruptHandler<peripherals::RNG>;
    EXTI15_10 => exti::InterruptHandler<interrupt::typelevel::EXTI15_10>;
});

// ---------------------------------------------------------------------------
// Shared state between tasks
// ---------------------------------------------------------------------------

const MORSE_MAX: usize = 32;

#[derive(Clone, PartialEq)]
enum LedMode {
    Off,
    On,
    Blink(u32),
    Heartbeat,
    Morse(String<MORSE_MAX>),
}

impl LedMode {
    fn name(&self) -> &'static str {
        match self {
            LedMode::Off => "off",
            LedMode::On => "on",
            LedMode::Blink(_) => "blink",
            LedMode::Heartbeat => "heartbeat",
            LedMode::Morse(_) => "morse",
        }
    }
}

/// Asynchronous events that tasks push to the shell, which prints them.
enum Event {
    Button { presses: u32, mode: &'static str },
    TempAlert { temp_c: f32, limit_c: f32 },
    TempNormal { temp_c: f32 },
    Timer { id: u32, secs: u32, label: String<24> },
}

#[derive(Clone, Copy)]
struct Reading {
    temp_c: f32,
    vdda_mv: u32,
    samples: u32,
}

static LED_MODE: Signal<CriticalSectionRawMutex, LedMode> = Signal::new();
static CURRENT_LED_MODE: Mutex<CriticalSectionRawMutex, RefCell<LedMode>> =
    Mutex::new(RefCell::new(LedMode::Heartbeat));
static EVENTS: Channel<CriticalSectionRawMutex, Event, 8> = Channel::new();
static READING: Mutex<CriticalSectionRawMutex, Cell<Reading>> = Mutex::new(Cell::new(Reading {
    temp_c: 0.0,
    vdda_mv: 0,
    samples: 0,
}));
/// Temperature alert threshold in milli-degrees Celsius.
static ALERT_MILLI_C: AtomicI32 = AtomicI32::new(32_000);
static BUTTON_PRESSES: AtomicU32 = AtomicU32::new(0);
/// While a game owns the button, presses are reported here (with their timestamp)
/// instead of cycling LED modes.
static GAME_ACTIVE: AtomicBool = AtomicBool::new(false);
static BUTTON_SIGNAL: Signal<CriticalSectionRawMutex, Instant> = Signal::new();
static RESET_REASON: Mutex<CriticalSectionRawMutex, Cell<&'static str>> = Mutex::new(Cell::new("unknown"));

/// Per-task wakeup counters, shown by the `ps` command.
struct TaskStat {
    name: &'static str,
    wakeups: AtomicU32,
}

impl TaskStat {
    const fn new(name: &'static str) -> Self {
        Self { name, wakeups: AtomicU32::new(0) }
    }
    fn tick(&self) {
        self.wakeups.fetch_add(1, Ordering::Relaxed);
    }
}

static STAT_SHELL: TaskStat = TaskStat::new("shell");
static STAT_LED: TaskStat = TaskStat::new("led");
static STAT_BUTTON: TaskStat = TaskStat::new("button");
static STAT_SENSOR: TaskStat = TaskStat::new("sensor");
static STAT_WATCHDOG: TaskStat = TaskStat::new("watchdog");
static STATS: [&TaskStat; 5] = [&STAT_SHELL, &STAT_LED, &STAT_BUTTON, &STAT_SENSOR, &STAT_WATCHDOG];

fn set_led_mode(mode: LedMode) {
    CURRENT_LED_MODE.lock(|m| *m.borrow_mut() = mode.clone());
    LED_MODE.signal(mode);
}

// ---------------------------------------------------------------------------
// LED task
// ---------------------------------------------------------------------------

/// The user LED is LD2 on PA5 (NUCLEO-L452RE) or LD4 on PB13 (NUCLEO-L452RE-P).
/// We drive both so the demo works on either board.
struct Led {
    a: Output<'static>,
    b: Output<'static>,
}

impl Led {
    fn set(&mut self, on: bool) {
        let level = if on { Level::High } else { Level::Low };
        self.a.set_level(level);
        self.b.set_level(level);
    }

    async fn pulse(&mut self, on_ms: u64, off_ms: u64) {
        self.set(true);
        STAT_LED.tick();
        Timer::after_millis(on_ms).await;
        self.set(false);
        STAT_LED.tick();
        Timer::after_millis(off_ms).await;
    }
}

fn morse_code(c: char) -> &'static str {
    match c.to_ascii_uppercase() {
        'A' => ".-", 'B' => "-...", 'C' => "-.-.", 'D' => "-..", 'E' => ".",
        'F' => "..-.", 'G' => "--.", 'H' => "....", 'I' => "..", 'J' => ".---",
        'K' => "-.-", 'L' => ".-..", 'M' => "--", 'N' => "-.", 'O' => "---",
        'P' => ".--.", 'Q' => "--.-", 'R' => ".-.", 'S' => "...", 'T' => "-",
        'U' => "..-", 'V' => "...-", 'W' => ".--", 'X' => "-..-", 'Y' => "-.--",
        'Z' => "--..", '0' => "-----", '1' => ".----", '2' => "..---", '3' => "...--",
        '4' => "....-", '5' => ".....", '6' => "-....", '7' => "--...", '8' => "---..",
        '9' => "----.",
        _ => "",
    }
}

/// Runs one LED pattern forever. It is cancelled (dropped) by `select`
/// as soon as a new mode is signalled.
async fn run_pattern(led: &mut Led, mode: &LedMode) {
    const UNIT: u64 = 150; // morse time unit in ms
    loop {
        match mode {
            LedMode::Off => {
                led.set(false);
                core::future::pending::<()>().await;
            }
            LedMode::On => {
                led.set(true);
                core::future::pending::<()>().await;
            }
            LedMode::Blink(period) => {
                let half = (*period as u64 / 2).max(10);
                led.pulse(half, half).await;
            }
            LedMode::Heartbeat => {
                led.pulse(80, 120).await;
                led.pulse(80, 900).await;
            }
            LedMode::Morse(text) => {
                for c in text.chars() {
                    if c == ' ' {
                        Timer::after_millis(UNIT * 7).await;
                        continue;
                    }
                    for symbol in morse_code(c).chars() {
                        let on = if symbol == '.' { UNIT } else { UNIT * 3 };
                        led.pulse(on, UNIT).await;
                    }
                    Timer::after_millis(UNIT * 2).await;
                }
                Timer::after_millis(UNIT * 10).await;
            }
        }
    }
}

#[embassy_executor::task]
async fn led_task(mut led: Led) {
    let mut mode = CURRENT_LED_MODE.lock(|m| m.borrow().clone());
    loop {
        match select(run_pattern(&mut led, &mode), LED_MODE.wait()).await {
            Either::First(()) => {}
            Either::Second(new_mode) => {
                info!("led: mode -> {}", new_mode.name());
                mode = new_mode;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Button task (interrupt driven)
// ---------------------------------------------------------------------------

#[embassy_executor::task]
async fn button_task(mut button: ExtiInput<'static, Async>) {
    let cycle = [LedMode::Blink(500), LedMode::Blink(100), LedMode::Heartbeat, LedMode::Off];
    let mut index = 0;
    loop {
        // The blue button is active low. The CPU sleeps here until the EXTI interrupt fires.
        button.wait_for_falling_edge().await;
        let pressed_at = Instant::now();
        STAT_BUTTON.tick();
        if GAME_ACTIVE.load(Ordering::Relaxed) {
            BUTTON_SIGNAL.signal(pressed_at);
            Timer::after_millis(20).await;
            button.wait_for_high().await;
            continue;
        }
        Timer::after_millis(20).await; // debounce
        if button.is_high() {
            continue;
        }

        let presses = BUTTON_PRESSES.fetch_add(1, Ordering::Relaxed) + 1;
        let mode = cycle[index].clone();
        index = (index + 1) % cycle.len();
        info!("button: press #{} -> {}", presses, mode.name());
        let name = mode.name();
        set_led_mode(mode);
        let _ = EVENTS.try_send(Event::Button { presses, mode: name });

        button.wait_for_high().await;
    }
}

// ---------------------------------------------------------------------------
// Sensor task
// ---------------------------------------------------------------------------

// Factory calibration values (STM32L452 datasheet), measured at VDDA = 3.0 V.
const VREFINT_CAL: *const u16 = 0x1FFF_75AA as *const u16;
const TS_CAL1: *const u16 = 0x1FFF_75A8 as *const u16; // 30 degC
const TS_CAL2: *const u16 = 0x1FFF_75CA as *const u16; // 130 degC
const CAL_VDDA_MV: f32 = 3000.0;

#[embassy_executor::task]
async fn sensor_task(adc: Peri<'static, ADC1>) {
    let mut adc = Adc::new(adc);
    let mut vrefint = adc.enable_vrefint();
    let mut temperature = adc.enable_temperature();
    // Workaround: stm32-metapac 21 swaps CH17SEL/CH18SEL for STM32L4. Per RM0394 the
    // temperature sensor is bit 23 and VBAT is bit 24, so fix the register by hand.
    pac::ADC1_COMMON.ccr().modify(|w| w.0 = (w.0 | (1 << 23)) & !(1 << 24));
    Timer::after_millis(1).await; // let the sensors stabilise

    let (vref_cal, ts_cal1, ts_cal2) =
        unsafe { (VREFINT_CAL.read_volatile() as f32, TS_CAL1.read_volatile() as f32, TS_CAL2.read_volatile() as f32) };

    let mut alerting = false;
    let mut samples = 0u32;
    loop {
        STAT_SENSOR.tick();
        let vref_raw = adc.blocking_read(&mut vrefint, SampleTime::CYCLES640_5) as f32;
        let ts_raw = adc.blocking_read(&mut temperature, SampleTime::CYCLES640_5) as f32;

        let vdda_mv = CAL_VDDA_MV * vref_cal / vref_raw;
        // Scale the reading to the calibration voltage, then interpolate between the two points.
        let ts_scaled = ts_raw * vdda_mv / CAL_VDDA_MV;
        let temp_c = (130.0 - 30.0) / (ts_cal2 - ts_cal1) * (ts_scaled - ts_cal1) + 30.0;

        samples += 1;
        READING.lock(|r| r.set(Reading { temp_c, vdda_mv: vdda_mv as u32, samples }));

        let limit_c = ALERT_MILLI_C.load(Ordering::Relaxed) as f32 / 1000.0;
        if !alerting && temp_c > limit_c {
            alerting = true;
            warn!("sensor: temperature alert {} C", temp_c);
            let _ = EVENTS.try_send(Event::TempAlert { temp_c, limit_c });
        } else if alerting && temp_c < limit_c - 1.0 {
            alerting = false;
            let _ = EVENTS.try_send(Event::TempNormal { temp_c });
        }

        Timer::after_millis(250).await;
    }
}

// ---------------------------------------------------------------------------
// Watchdog task
// ---------------------------------------------------------------------------

#[embassy_executor::task]
async fn watchdog_task(mut wdg: IndependentWatchdog<'static, peripherals::IWDG>) {
    wdg.unleash();
    loop {
        STAT_WATCHDOG.tick();
        wdg.pet();
        Timer::after_millis(500).await;
    }
}

// ---------------------------------------------------------------------------
// Shell task
// ---------------------------------------------------------------------------

const PROMPT: &str = "\x1b[1;32mnucleos\x1b[0m> ";

struct Term {
    tx: UartTx<'static, Async>,
}

impl Term {
    /// TX goes through DMA: the CPU hands the buffer to the DMA controller and the
    /// task sleeps until the transfer-complete interrupt wakes it up.
    async fn print(&mut self, s: &str) {
        // A zero-length DMA transfer is invalid, so skip empty strings.
        if s.is_empty() {
            return;
        }
        let _ = self.tx.write(s.as_bytes()).await;
    }

    async fn print_lines(&mut self, lines: &[&str]) {
        for line in lines {
            self.print(line).await;
            self.print("\r\n").await;
        }
    }

    async fn printf(&mut self, args: fmt::Arguments<'_>) {
        let mut s: String<256> = String::new();
        let _ = s.write_fmt(args);
        self.print(&s).await;
    }
}

macro_rules! out {
    ($term:expr, $($arg:tt)*) => {
        $term.printf(format_args!($($arg)*)).await
    };
}

mod fun;

const BANNER: &[&str] = &[
    "",
    "\x1b[1;36m  _   _            _       ___  ____  ",
    " | \\ | |_   _  ___| | ___ / _ \\/ ___| ",
    " |  \\| | | | |/ __| |/ _ \\ | | \\___ \\ ",
    " | |\\  | |_| | (__| |  __/ |_| |___) |",
    " |_| \\_|\\__,_|\\___|_|\\___|\\___/|____/ \x1b[0m",
    "",
    "  async Rust on STM32L452 - powered by Embassy",
    "  type 'help' to get started",
    "",
];

const HELP: &[&str] = &[
    "Commands:",
    "  help                 show this help",
    "  info                 chip, clock, uptime and reset reason",
    "  ps                   list running tasks and their wakeups",
    "  led <mode>           on | off | blink [ms] | heartbeat | morse <text>",
    "  temp                 on-chip temperature and supply voltage",
    "  watch                live temperature graph (press any key to stop)",
    "  alert <celsius>      set the temperature alert threshold",
    "  uptime               time since boot",
    "  hang                 freeze the executor -> watchdog resets the chip",
    "  reboot               software reset",
    "  clear                clear the screen",
    "",
    "Fun:",
    "  reaction             reflex game: hit the button when the LED turns on",
    "  roll [N]dM           roll dice with the hardware TRNG (roll 2d6, roll d20)",
    "  timer <s> [label]    spawn a new countdown task at runtime",
    "  mandel               Mandelbrot on the FPU, timed in CPU cycles",
    "  life                 Conway's Game of Life",
    "  matrix               digital rain",
    "  dump <addr> [len]    hex dump real memory (try: dump 0x1FFF7590 12)",
    "  say <text>           Ferris the crab says it",
    "  fortune              words of wisdom",
    "",
    "Press the blue USER button to cycle LED modes.",
];

fn uptime_parts() -> (u64, u64, u64, u64) {
    let ms = Instant::now().as_millis();
    (ms / 3_600_000, (ms / 60_000) % 60, (ms / 1000) % 60, ms % 1000)
}

async fn print_event(term: &mut Term, event: Event) {
    match event {
        Event::Button { presses, mode } => {
            out!(term, "\r\n\x1b[1;33m[event]\x1b[0m button pressed (#{presses}), led mode -> {mode}\r\n")
        }
        Event::TempAlert { temp_c, limit_c } => {
            out!(term, "\r\n\x1b[1;31m[alert]\x1b[0m temperature {temp_c:.1} C is above {limit_c:.1} C\r\n")
        }
        Event::TempNormal { temp_c } => {
            out!(term, "\r\n\x1b[1;32m[ok]\x1b[0m temperature back to normal ({temp_c:.1} C)\r\n")
        }
        Event::Timer { id, secs, label } => {
            out!(term, "\x07\r\n\x1b[1;35m[timer]\x1b[0m countdown#{id} '{}' done after {secs} s\r\n", label.as_str())
        }
    }
}

async fn run_command(term: &mut Term, rx: &mut RingBufferedUartRx<'static>, ctx: &mut fun::Ctx, line: &str) {
    let mut args = line.split_whitespace();
    let Some(cmd) = args.next() else { return };

    match cmd {
        "help" => term.print_lines(HELP).await,
        "info" => {
            let (h, m, s, _) = uptime_parts();
            let reason = RESET_REASON.lock(|r| r.get());
            out!(term, "chip      : STM32L452RE (Cortex-M4F)\r\n");
            out!(term, "clock     : {} MHz\r\n", rcc::frequency::<peripherals::USART2>().0 / 1_000_000);
            out!(term, "memory    : 512 KiB flash, 160 KiB SRAM\r\n");
            out!(term, "uid       : {}\r\n", embassy_stm32::uid::uid_hex());
            out!(term, "runtime   : embassy-executor (cooperative async)\r\n");
            out!(term, "uptime    : {h:02}:{m:02}:{s:02}\r\n");
            out!(term, "reset     : {reason}\r\n");
        }
        "ps" => {
            out!(term, "  {:<10} {:>10}  {}\r\n", "TASK", "WAKEUPS", "STATE");
            for stat in STATS.iter() {
                let state = match stat.name {
                    "shell" => "running",
                    "led" => CURRENT_LED_MODE.lock(|m| m.borrow().name()),
                    _ => "waiting",
                };
                out!(term, "  {:<10} {:>10}  {}\r\n", stat.name, stat.wakeups.load(Ordering::Relaxed), state);
            }
            let timers = fun::ACTIVE_TIMERS.load(Ordering::Relaxed);
            out!(term, "  {:<10} {:>10}  {}/{} slots running\r\n", "countdown", "-", timers, fun::TIMER_SLOTS);
            out!(term, "  button presses: {}\r\n", BUTTON_PRESSES.load(Ordering::Relaxed));
        }
        "led" => {
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
                        term.print("usage: led morse <text>\r\n").await;
                        return;
                    }
                    LedMode::Morse(text)
                }
                _ => {
                    term.print("usage: led on | off | blink [ms] | heartbeat | morse <text>\r\n").await;
                    return;
                }
            };
            out!(term, "led mode -> {}\r\n", mode.name());
            set_led_mode(mode);
        }
        "temp" => {
            let r = READING.lock(|r| r.get());
            out!(term, "temperature : {:.1} C\r\n", r.temp_c);
            out!(term, "vdda        : {} mV\r\n", r.vdda_mv);
            out!(term, "alert above : {:.1} C\r\n", ALERT_MILLI_C.load(Ordering::Relaxed) as f32 / 1000.0);
            out!(term, "samples     : {}\r\n", r.samples);
        }
        "watch" => {
            term.print("live temperature - press any key to stop\r\n").await;
            let mut buf = [0u8; 8];
            loop {
                let r = READING.lock(|r| r.get());
                let limit = ALERT_MILLI_C.load(Ordering::Relaxed) as f32 / 1000.0;
                // Map 20..45 C onto a 40 character bar.
                let len = (((r.temp_c - 20.0) / 25.0 * 40.0) as i32).clamp(0, 40) as usize;
                let color = if r.temp_c > limit { "31" } else { "36" };
                let mut bar: String<48> = String::new();
                for i in 0..40 {
                    let _ = bar.push(if i < len { '#' } else { '.' });
                }
                out!(term, "\r  {:6.2} C  {:4} mV  \x1b[{}m{}\x1b[0m ", r.temp_c, r.vdda_mv, color, bar.as_str());
                if let Either::First(_) = select(rx.read(&mut buf), Timer::after(Duration::from_millis(250))).await {
                    break;
                }
            }
            term.print("\r\n").await;
        }
        "alert" => match args.next().and_then(|v| v.parse::<f32>().ok()) {
            Some(c) => {
                ALERT_MILLI_C.store((c * 1000.0) as i32, Ordering::Relaxed);
                out!(term, "temperature alert threshold set to {c:.1} C\r\n");
            }
            None => term.print("usage: alert <celsius>\r\n").await,
        },
        "uptime" => {
            let (h, m, s, ms) = uptime_parts();
            out!(term, "up {h:02}:{m:02}:{s:02}.{ms:03}\r\n");
        }
        "hang" => {
            term.print("\x1b[1;31mfreezing the executor with a blocking loop...\x1b[0m\r\n").await;
            term.print("no task can run now, so nobody feeds the watchdog. bye!\r\n").await;
            // A busy loop never yields back to the executor: this is exactly the bug
            // a watchdog protects against in a cooperative system.
            #[allow(clippy::empty_loop)]
            loop {}
        }
        "reboot" => {
            term.print("rebooting...\r\n").await;
            Timer::after_millis(10).await;
            cortex_m::peripheral::SCB::sys_reset();
        }
        "clear" => term.print("\x1b[2J\x1b[H").await,
        "reaction" => fun::reaction(term, rx, ctx).await,
        "roll" => fun::roll(term, ctx, args.next()).await,
        "timer" => fun::timer(term, ctx, args).await,
        "mandel" => fun::mandel(term).await,
        "life" => fun::life(term, rx, ctx).await,
        "matrix" => fun::matrix(term, rx, ctx).await,
        "dump" => fun::dump(term, args).await,
        "say" => fun::say(term, args).await,
        "fortune" => fun::fortune(term, ctx).await,
        other => out!(term, "unknown command: '{other}' (try 'help')\r\n"),
    }
}

#[embassy_executor::task]
async fn shell_task(tx: UartTx<'static, Async>, mut rx: RingBufferedUartRx<'static>, mut ctx: fun::Ctx) {
    let mut term = Term { tx };

    term.print_lines(BANNER).await;
    let reason = RESET_REASON.lock(|r| r.get());
    out!(term, "  last reset: {reason}\r\n\r\n");
    term.print(PROMPT).await;

    let mut line: String<96> = String::new();
    let mut buf = [0u8; 16];
    loop {
        // Wait for whichever comes first: keyboard input or an event from another task.
        match select(rx.read(&mut buf), EVENTS.receive()).await {
            Either::First(Ok(n)) => {
                STAT_SHELL.tick();
                for &b in &buf[..n] {
                    match b {
                        b'\r' | b'\n' => {
                            term.print("\r\n").await;
                            let cmd: String<96> = line.clone();
                            line.clear();
                            run_command(&mut term, &mut rx, &mut ctx, cmd.trim()).await;
                            term.print(PROMPT).await;
                        }
                        0x08 | 0x7f => {
                            if line.pop().is_some() {
                                term.print("\x08 \x08").await;
                            }
                        }
                        0x20..=0x7e => {
                            if line.push(b as char).is_ok() {
                                let _ = term.tx.write(&[b]).await;
                            }
                        }
                        _ => {} // ignore other control bytes (escape sequences, etc.)
                    }
                }
            }
            Either::First(Err(_)) => {}
            Either::Second(event) => {
                STAT_SHELL.tick();
                print_event(&mut term, event).await;
                term.print(PROMPT).await;
                term.print(&line).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

fn read_reset_reason() -> &'static str {
    let csr = pac::RCC.csr().read();
    let reason = if csr.iwdgrstf() {
        "watchdog (IWDG)"
    } else if csr.wwdgrstf() {
        "window watchdog"
    } else if csr.sftrstf() {
        "software reset"
    } else if csr.borrstf() {
        "power on"
    } else if csr.pinrstf() {
        "reset pin / debugger"
    } else {
        "unknown"
    };
    // Clear the flags so the next boot reports a fresh reason.
    pac::RCC.csr().modify(|w| w.set_rmvf(true));
    reason
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut config = Config::default();
    {
        use embassy_stm32::rcc::*;
        // 16 MHz HSI -> PLL -> 80 MHz system clock (the max for STM32L4).
        config.rcc.hsi = true;
        config.rcc.pll = Some(Pll {
            source: PllSource::HSI,
            prediv: PllPreDiv::DIV1,
            mul: PllMul::MUL10,
            divp: None,
            divq: None,
            divr: Some(PllRDiv::DIV2),
        });
        config.rcc.sys = Sysclk::PLL1_R;
        config.rcc.mux.adcsel = mux::Adcsel::SYS;
        // The true random number generator needs a 48 MHz clock.
        config.rcc.hsi48 = Some(Hsi48Config { sync_from_usb: false });
        config.rcc.mux.clk48sel = mux::Clk48sel::HSI48;
    }
    let p = embassy_stm32::init(config);

    // Enable the DWT cycle counter, used by `mandel` to measure work in CPU cycles.
    let mut core = cortex_m::Peripherals::take().unwrap();
    core.DCB.enable_trace();
    core.DWT.enable_cycle_counter();

    let reason = read_reset_reason();
    RESET_REASON.lock(|r| r.set(reason));
    info!("NucleOS booting, reset reason: {}", reason);

    // USART2 on PA2/PA3 is wired to the ST-LINK virtual COM port (/dev/ttyACM0).
    // RX uses a circular DMA buffer, so no byte is lost even while the shell is busy.
    static RX_DMA_BUF: StaticCell<[u8; 256]> = StaticCell::new();
    let mut uart_config = usart::Config::default();
    uart_config.baudrate = 115_200;
    let uart = Uart::new(p.USART2, p.PA3, p.PA2, p.DMA1_CH7, p.DMA1_CH6, Irqs, uart_config).unwrap();
    let (uart_tx, uart_rx) = uart.split();
    let uart_rx = uart_rx.into_ring_buffered(RX_DMA_BUF.init([0; 256]));

    let led = Led {
        a: Output::new(p.PA5, Level::Low, Speed::Low),
        b: Output::new(p.PB13, Level::Low, Speed::Low),
    };
    let button = ExtiInput::new(p.PC13, p.EXTI13, Pull::None, Irqs);
    let wdg = IndependentWatchdog::new(p.IWDG, 2_000_000); // 2 s timeout

    let ctx = fun::Ctx { rng: Rng::new(p.RNG, Irqs), spawner };
    spawner.spawn(shell_task(uart_tx, uart_rx, ctx).unwrap());
    spawner.spawn(led_task(led).unwrap());
    spawner.spawn(button_task(button).unwrap());
    spawner.spawn(sensor_task(p.ADC1).unwrap());
    spawner.spawn(watchdog_task(wdg).unwrap());

    info!("all tasks spawned");
}
