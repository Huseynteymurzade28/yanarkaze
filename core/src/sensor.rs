//! The sensor task: samples the temperature periodically and raises alerts.

use embassy_time::Timer;

use crate::platform::Sensor;
use crate::state::{EVENTS, Event, SAMPLE, STAT_SENSOR, Sample, TEMP_HISTORY, alert_limit_c};

const PERIOD_MS: u64 = 250;
const HYSTERESIS_C: f32 = 1.0;
/// Samples averaged into one point of the one-minute history.
const PER_SECOND: u32 = (1000 / PERIOD_MS) as u32;

/// The sensor task body. Run it from a board's `#[embassy_executor::task]`.
pub async fn run(mut sensor: impl Sensor) -> ! {
    let mut alerting = false;
    let mut count = 0u32;
    let mut sum = 0.0f32;
    loop {
        STAT_SENSOR.tick();
        let reading = sensor.read().await;
        count = count.wrapping_add(1);
        SAMPLE.lock(|s| s.set(Sample { reading, count }));
        sum += reading.temp_c;
        if count.is_multiple_of(PER_SECOND) {
            let average = sum / PER_SECOND as f32;
            TEMP_HISTORY.lock(|h| h.borrow_mut().push(average));
            sum = 0.0;
        }

        let limit_c = alert_limit_c();
        let temp_c = reading.temp_c;
        if !alerting && temp_c > limit_c {
            alerting = true;
            let _ = EVENTS.try_send(Event::TempAlert { temp_c, limit_c });
        } else if alerting && temp_c < limit_c - HYSTERESIS_C {
            alerting = false;
            let _ = EVENTS.try_send(Event::TempNormal { temp_c });
        }

        Timer::after_millis(PERIOD_MS).await;
    }
}
