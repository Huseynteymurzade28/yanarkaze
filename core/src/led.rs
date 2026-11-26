//! The LED task: runs patterns (blink, heartbeat, morse) and switches instantly when a
//! new mode is signalled.

use embassy_futures::select::{Either, select};
use embassy_time::Timer;

use crate::platform::Led;
use crate::state::{LED_MODE, LedMode, STAT_LED, current_led_mode};

async fn pulse(led: &mut impl Led, on_ms: u64, off_ms: u64) {
    led.set(true);
    STAT_LED.tick();
    Timer::after_millis(on_ms).await;
    led.set(false);
    STAT_LED.tick();
    Timer::after_millis(off_ms).await;
}

pub(crate) fn morse_code(c: char) -> &'static str {
    match c.to_ascii_uppercase() {
        'A' => ".-",
        'B' => "-...",
        'C' => "-.-.",
        'D' => "-..",
        'E' => ".",
        'F' => "..-.",
        'G' => "--.",
        'H' => "....",
        'I' => "..",
        'J' => ".---",
        'K' => "-.-",
        'L' => ".-..",
        'M' => "--",
        'N' => "-.",
        'O' => "---",
        'P' => ".--.",
        'Q' => "--.-",
        'R' => ".-.",
        'S' => "...",
        'T' => "-",
        'U' => "..-",
        'V' => "...-",
        'W' => ".--",
        'X' => "-..-",
        'Y' => "-.--",
        'Z' => "--..",
        '0' => "-----",
        '1' => ".----",
        '2' => "..---",
        '3' => "...--",
        '4' => "....-",
        '5' => ".....",
        '6' => "-....",
        '7' => "--...",
        '8' => "---..",
        '9' => "----.",
        _ => "",
    }
}

/// Runs one LED pattern forever. It is cancelled (dropped) by `select` as soon as a new
/// mode is signalled.
async fn run_pattern(led: &mut impl Led, mode: &LedMode) {
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
                pulse(led, half, half).await;
            }
            LedMode::Heartbeat => {
                pulse(led, 80, 120).await;
                pulse(led, 80, 900).await;
            }
            LedMode::Morse(text) => {
                for c in text.chars() {
                    if c == ' ' {
                        Timer::after_millis(UNIT * 7).await;
                        continue;
                    }
                    for symbol in morse_code(c).chars() {
                        let on = if symbol == '.' { UNIT } else { UNIT * 3 };
                        pulse(led, on, UNIT).await;
                    }
                    Timer::after_millis(UNIT * 2).await;
                }
                Timer::after_millis(UNIT * 10).await;
            }
        }
    }
}

/// The LED task body. Run it from a board's `#[embassy_executor::task]`.
pub async fn run(mut led: impl Led) -> ! {
    let mut mode = current_led_mode();
    loop {
        if let Either::Second(new_mode) = select(run_pattern(&mut led, &mode), LED_MODE.wait()).await {
            mode = new_mode;
        }
    }
}
