//! The button task: debounces presses and cycles LED modes, or hands presses to a game.

use embassy_time::{Instant, Timer};
use portable_atomic::Ordering;

use crate::platform::Button;
use crate::state::{BUTTON_PRESSES, BUTTON_SIGNAL, EVENTS, Event, GAME_ACTIVE, LedMode, STAT_BUTTON, set_led_mode};

const DEBOUNCE_MS: u64 = 20;

/// The button task body. Run it from a board's `#[embassy_executor::task]`.
pub async fn run(mut button: impl Button) -> ! {
    let cycle = [LedMode::Blink(500), LedMode::Blink(100), LedMode::Heartbeat, LedMode::Off];
    let mut index = 0;
    loop {
        // On interrupt-capable boards the CPU sleeps here until the pin changes.
        button.wait_for_press().await;
        let pressed_at = Instant::now();
        STAT_BUTTON.tick();

        if GAME_ACTIVE.load(Ordering::Relaxed) {
            BUTTON_SIGNAL.signal(pressed_at);
        } else {
            let presses = BUTTON_PRESSES.fetch_add(1, Ordering::Relaxed) + 1;
            let mode = cycle[index].clone();
            index = (index + 1) % cycle.len();
            let name = mode.name();
            set_led_mode(mode);
            let _ = EVENTS.try_send(Event::Button { presses, mode: name });
        }

        Timer::after_millis(DEBOUNCE_MS).await;
        button.wait_for_release().await;
        Timer::after_millis(DEBOUNCE_MS).await;
    }
}
