//! yanarkaze on the ESP32-C3 (single RISC-V core, 160 MHz).
//!
//!   console  -> the built-in USB Serial/JTAG controller (no USB-UART chip needed)
//!   LED      -> GPIO8 (the blue LED on "SuperMini" boards, active low)
//!   button   -> the BOOT button on GPIO9 (active low, GPIO interrupt)
//!   sensor   -> the on-chip temperature sensor (TSENS)
//!   watchdog -> TIMG1 main watchdog, 2 s timeout
//!   RNG      -> hardware RNG
//!
//! The executor and the embassy-time driver come from esp-rtos.

#![no_std]
#![no_main]

use core::fmt::Write as _;

use embassy_executor::Spawner;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::peripherals::TIMG1;
use esp_hal::rng::Rng;
use esp_hal::time::Duration;
use esp_hal::timer::timg::{MwdtStage, TimerGroup, Wdt};
use esp_hal::tsens::{self, TemperatureSensor};
use esp_hal::usb::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx};
use esp_hal::{Async, efuse};
use heapless::String;
use static_cell::StaticCell;
use yanarkaze_core::{BoardInfo, Button, Led, MemRegion, Reading, Sensor, System, Watchdog};

esp_bootloader_esp_idf::esp_app_desc!();

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // Nobody feeds the watchdog any more, so the chip resets itself shortly.
    loop {
        core::hint::spin_loop();
    }
}

static INFO: BoardInfo = BoardInfo {
    board: "ESP32-C3",
    chip: "ESP32-C3",
    cpu: "RISC-V RV32IMC",
    cpu_mhz: Some(160),
    fpu: false,
    flash_kib: Some(4096),
    ram_kib: Some(400),
    button: "the BOOT button",
    hints: &["console over the built-in USB Serial/JTAG"],
};

static REGIONS: [MemRegion; 1] = [MemRegion { start: 0x3FC8_0000, end: 0x3FCE_0000, name: "internal SRAM (data bus)" }];

struct EspLed(Output<'static>);

impl Led for EspLed {
    fn set(&mut self, on: bool) {
        // Active low on most C3 boards; flip this if yours is wired the other way.
        self.0.set_level(if on { Level::Low } else { Level::High });
    }
}

struct BootButton(Input<'static>);

impl Button for BootButton {
    async fn wait_for_press(&mut self) {
        self.0.wait_for_falling_edge().await;
    }

    async fn wait_for_release(&mut self) {
        self.0.wait_for_high().await;
    }
}

struct DieSensor(TemperatureSensor<'static>);

impl Sensor for DieSensor {
    async fn read(&mut self) -> Reading {
        Reading { temp_c: self.0.get_temperature().to_celsius(), vdda_mv: None }
    }
}

struct EspWatchdog(Wdt<TIMG1<'static>>);

impl Watchdog for EspWatchdog {
    fn start(&mut self) {
        self.0.set_timeout(MwdtStage::Stage0, Duration::from_millis(2000));
        self.0.enable();
    }

    fn feed(&mut self) {
        self.0.feed();
    }
}

struct EspSystem {
    rng: Rng,
    reset_reason: &'static str,
}

impl System for EspSystem {
    fn info(&self) -> &BoardInfo {
        &INFO
    }

    fn reset_reason(&self) -> &'static str {
        self.reset_reason
    }

    fn random_u32(&mut self) -> u32 {
        self.rng.random()
    }

    fn unique_id(&self) -> Option<String<32>> {
        // The factory-programmed base MAC address is unique per chip.
        let mut id = String::new();
        for byte in efuse::base_mac_address().as_bytes() {
            let _ = write!(id, "{byte:02X}");
        }
        Some(id)
    }

    fn memory_regions(&self) -> &[MemRegion] {
        &REGIONS
    }

    fn reset(&mut self) -> ! {
        esp_hal::system::software_reset()
    }
}

#[embassy_executor::task]
async fn shell_task(
    sys: EspSystem,
    rx: UsbSerialJtagRx<'static, Async>,
    tx: UsbSerialJtagTx<'static, Async>,
    spawner: Spawner,
) {
    yanarkaze_core::shell::run(sys, rx, tx, spawner).await
}

#[embassy_executor::task]
async fn led_task(led: EspLed) {
    yanarkaze_core::led::run(led).await
}

#[embassy_executor::task]
async fn button_task(button: BootButton) {
    yanarkaze_core::button::run(button).await
}

#[embassy_executor::task]
async fn sensor_task(sensor: DieSensor) {
    yanarkaze_core::sensor::run(sensor).await
}

#[embassy_executor::task]
async fn watchdog_task(watchdog: EspWatchdog) {
    yanarkaze_core::watchdog::run(watchdog).await
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    let p = esp_hal::init(esp_hal::Config::default());

    let timg0 = TimerGroup::new(p.TIMG0);
    esp_rtos::start(timg0.timer0, p.FROM_CPU_INTR0);

    static RESET_REASON: StaticCell<String<32>> = StaticCell::new();
    let reason = RESET_REASON.init(String::new());
    match esp_hal::system::reset_reason() {
        Some(r) => {
            let _ = write!(reason, "{r:?}");
        }
        None => {
            let _ = reason.push_str("unknown");
        }
    }

    let (rx, tx) = UsbSerialJtag::new(p.USB_DEVICE).into_async().split();
    let led = EspLed(Output::new(p.GPIO8, Level::High, OutputConfig::default()));
    let button = BootButton(Input::new(p.GPIO9, InputConfig::default().with_pull(Pull::Up)));
    let sensor = DieSensor(TemperatureSensor::new(p.TSENS, tsens::Config::default()).unwrap());
    let watchdog = EspWatchdog(TimerGroup::new(p.TIMG1).wdt);
    let sys = EspSystem { rng: Rng::new(), reset_reason: reason.as_str() };

    spawner.spawn(shell_task(sys, rx, tx, spawner).unwrap());
    spawner.spawn(led_task(led).unwrap());
    spawner.spawn(button_task(button).unwrap());
    spawner.spawn(sensor_task(sensor).unwrap());
    spawner.spawn(watchdog_task(watchdog).unwrap());
}
