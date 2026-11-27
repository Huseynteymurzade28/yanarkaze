//! The button task: debounces presses and cycles LED modes, or hands presses to a game.

use embassy_futures::select::{Either, select};
use embassy_time::{Instant, Timer};
use portable_atomic::Ordering;

use crate::platform::Button;
use crate::state::{BUTTON_PRESSES, BUTTON_RELEASE, BUTTON_SIGNAL, EVENTS, Event, GAME_ACTIVE, LedMode, STAT_BUTTON, set_led_mode};

const DEBOUNCE_MS: u64 = 20;
/// A press only counts once the pin has stayed down this long, so short contacts are
/// ignored. The press is still timestamped at its first edge (the reaction game needs that).
const CONFIRM_MS: u64 = 30;
/// After a release the pin has to stay up this long before the next press counts. A loose
/// wire scraped against GND bounces for much longer than a real button does.
const SETTLE_MS: u64 = 100;

/// The button task body. Run it from a board's `#[embassy_executor::task]`.
pub async fn run(mut button: impl Button) -> ! {
    let cycle = [LedMode::Blink(500), LedMode::Blink(100), LedMode::Heartbeat, LedMode::Off];
    let mut index = 0;
    loop {
        // On interrupt-capable boards the CPU sleeps here until the pin changes.
        button.wait_for_press().await;
        let pressed_at = Instant::now();
        STAT_BUTTON.tick();
        if let Either::Second(()) = select(Timer::after_millis(CONFIRM_MS), button.wait_for_release()).await {
            continue; // a glitch, not a press
        }

        if GAME_ACTIVE.load(Ordering::Relaxed) {
            BUTTON_SIGNAL.signal(pressed_at);
            button.wait_for_release().await;
            BUTTON_RELEASE.signal(Instant::now());
        } else {
            let presses = BUTTON_PRESSES.fetch_add(1, Ordering::Relaxed) + 1;
            let mode = cycle[index].clone();
            index = (index + 1) % cycle.len();
            let name = mode.name();
            set_led_mode(mode);
            let _ = EVENTS.try_send(Event::Button { presses, mode: name });
        }

        wait_until_settled(&mut button).await;
    }
}

/// Waits until the button has been released and stayed released for `SETTLE_MS`.
async fn wait_until_settled(button: &mut impl Button) {
    loop {
        button.wait_for_release().await;
        match select(Timer::after_millis(SETTLE_MS), button.wait_for_press()).await {
            Either::First(()) => return,
            // Went down again too soon: still bouncing.
            Either::Second(()) => Timer::after_millis(DEBOUNCE_MS).await,
        }
    }
}
