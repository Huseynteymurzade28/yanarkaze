//! The watchdog task: keeps feeding the watchdog as long as the executor is healthy.
//!
//! In a cooperative scheduler a single task that never yields freezes everything. The
//! watchdog turns that freeze into a clean reset instead of a dead device.

use embassy_time::Timer;

use crate::platform::Watchdog;
use crate::state::STAT_WATCHDOG;

const FEED_PERIOD_MS: u64 = 500;

/// The watchdog task body. Run it from a board's `#[embassy_executor::task]`.
pub async fn run(mut watchdog: impl Watchdog) -> ! {
    watchdog.start();
    loop {
        STAT_WATCHDOG.tick();
        watchdog.feed();
        Timer::after_millis(FEED_PERIOD_MS).await;
    }
}
