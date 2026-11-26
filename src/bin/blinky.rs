#![no_std]
#![no_main]

use defmt::info;
use embassy_executor::Spawner;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_time::Timer;
use {defmt_rtt as _, panic_probe as _};

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_stm32::init(Default::default());
    info!("Hello from Embassy on NUCLEO-L452RE");

    // NUCLEO-L452RE: LD2 = PA5, NUCLEO-L452RE-P: LD4 = PB13
    let mut led_a5 = Output::new(p.PA5, Level::Low, Speed::Low);
    let mut led_b13 = Output::new(p.PB13, Level::Low, Speed::Low);

    loop {
        info!("LED on");
        led_a5.set_high();
        led_b13.set_high();
        Timer::after_millis(500).await;

        info!("LED off");
        led_a5.set_low();
        led_b13.set_low();
        Timer::after_millis(500).await;
    }
}
