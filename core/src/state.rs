//! State shared between tasks, and the primitives they use to talk to each other.

use core::cell::{Cell, RefCell};

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::Instant;
use heapless::String;
use portable_atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use crate::platform::Reading;

pub const MORSE_MAX: usize = 32;

#[derive(Clone, PartialEq)]
pub enum LedMode {
    Off,
    On,
    Blink(u32),
    Heartbeat,
    Morse(String<MORSE_MAX>),
}

impl LedMode {
    pub fn name(&self) -> &'static str {
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
pub enum Event {
    Button { presses: u32, mode: &'static str },
    TempAlert { temp_c: f32, limit_c: f32 },
    TempNormal { temp_c: f32 },
    Timer { id: u32, secs: u32, label: String<24> },
}

/// The latest sensor sample plus a running count.
#[derive(Clone, Copy, Default)]
pub struct Sample {
    pub reading: Reading,
    pub count: u32,
}

/// New LED mode for the LED task (latest value wins).
pub static LED_MODE: Signal<CriticalSectionRawMutex, LedMode> = Signal::new();
/// The mode the LED task is currently running, for `ps` and to restore after games.
pub static CURRENT_LED_MODE: Mutex<CriticalSectionRawMutex, RefCell<LedMode>> =
    Mutex::new(RefCell::new(LedMode::Heartbeat));
pub static EVENTS: Channel<CriticalSectionRawMutex, Event, 8> = Channel::new();
pub static SAMPLE: Mutex<CriticalSectionRawMutex, Cell<Sample>> =
    Mutex::new(Cell::new(Sample { reading: Reading { temp_c: 0.0, vdda_mv: None }, count: 0 }));

/// One temperature per second for the last `HISTORY_SECS` seconds, for the `graph` command.
pub const HISTORY_SECS: usize = 60;

pub struct TempHistory {
    values: [f32; HISTORY_SECS],
    /// Index the next value goes to.
    next: usize,
    len: usize,
}

impl TempHistory {
    pub const fn new() -> Self {
        Self { values: [0.0; HISTORY_SECS], next: 0, len: 0 }
    }

    pub fn push(&mut self, value: f32) {
        self.values[self.next] = value;
        self.next = (self.next + 1) % HISTORY_SECS;
        self.len = (self.len + 1).min(HISTORY_SECS);
    }

    /// The stored values, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = f32> + '_ {
        let start = (self.next + HISTORY_SECS - self.len) % HISTORY_SECS;
        (0..self.len).map(move |i| self.values[(start + i) % HISTORY_SECS])
    }
}

impl Default for TempHistory {
    fn default() -> Self {
        Self::new()
    }
}

pub static TEMP_HISTORY: Mutex<CriticalSectionRawMutex, RefCell<TempHistory>> =
    Mutex::new(RefCell::new(TempHistory::new()));

/// Temperature alert threshold in milli-degrees Celsius.
pub static ALERT_MILLI_C: AtomicI32 = AtomicI32::new(32_000);
pub static BUTTON_PRESSES: AtomicU32 = AtomicU32::new(0);
/// While a game owns the button, presses are reported on `BUTTON_SIGNAL` (with their
/// timestamp) instead of cycling LED modes.
pub static GAME_ACTIVE: AtomicBool = AtomicBool::new(false);
pub static BUTTON_SIGNAL: Signal<CriticalSectionRawMutex, Instant> = Signal::new();

pub fn set_led_mode(mode: LedMode) {
    CURRENT_LED_MODE.lock(|m| *m.borrow_mut() = mode.clone());
    LED_MODE.signal(mode);
}

pub fn current_led_mode() -> LedMode {
    CURRENT_LED_MODE.lock(|m| m.borrow().clone())
}

pub fn alert_limit_c() -> f32 {
    ALERT_MILLI_C.load(Ordering::Relaxed) as f32 / 1000.0
}

/// Per-task wakeup counters, shown by the `ps` command.
pub struct TaskStat {
    pub name: &'static str,
    wakeups: AtomicU32,
}

impl TaskStat {
    const fn new(name: &'static str) -> Self {
        Self { name, wakeups: AtomicU32::new(0) }
    }

    pub fn tick(&self) {
        self.wakeups.fetch_add(1, Ordering::Relaxed);
    }

    pub fn wakeups(&self) -> u32 {
        self.wakeups.load(Ordering::Relaxed)
    }
}

pub static STAT_SHELL: TaskStat = TaskStat::new("shell");
pub static STAT_LED: TaskStat = TaskStat::new("led");
pub static STAT_BUTTON: TaskStat = TaskStat::new("button");
pub static STAT_SENSOR: TaskStat = TaskStat::new("sensor");
pub static STAT_WATCHDOG: TaskStat = TaskStat::new("watchdog");
pub static STATS: [&TaskStat; 5] = [&STAT_SHELL, &STAT_LED, &STAT_BUTTON, &STAT_SENSOR, &STAT_WATCHDOG];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_keeps_the_latest_values_in_order() {
        let mut h = TempHistory::new();
        assert_eq!(h.iter().count(), 0);
        for i in 0..HISTORY_SECS + 5 {
            h.push(i as f32);
        }
        let values: heapless::Vec<f32, HISTORY_SECS> = h.iter().collect();
        assert_eq!(values.len(), HISTORY_SECS);
        assert_eq!(values[0], 5.0);
        assert_eq!(values[HISTORY_SECS - 1], (HISTORY_SECS + 4) as f32);
    }
}
