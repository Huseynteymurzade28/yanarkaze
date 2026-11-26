//! yanarkaze on the Nordic nRF52840-DK.
//!
//!   console  -> UARTE0 on P0.06 (TX) / P0.08 (RX), the J-Link virtual COM port
//!   LED      -> LED1 on P0.13 (active low)
//!   button   -> Button 1 on P0.11 (active low, GPIOTE interrupt)
//!   sensor   -> the TEMP peripheral (die temperature, 0.25 C steps)
//!   watchdog -> WDT, 2 s timeout
//!   RNG      -> hardware RNG

#![no_std]
#![no_main]

use core::fmt::Write as _;

use cortex_m::peripheral::DWT;
use defmt::info;
use embassy_executor::Spawner;
use embassy_nrf::buffered_uarte::{self, BufferedUarte, BufferedUarteRx, BufferedUarteTx};
use embassy_nrf::gpio::{Input, Level, Output, OutputDrive, Pull};
use embassy_nrf::mode::Blocking;
use embassy_nrf::rng::Rng;
use embassy_nrf::temp::{self, Temp};
use embassy_nrf::uarte::{self, Baudrate};
use embassy_nrf::wdt::{self, WatchdogHandle};
use embassy_nrf::{bind_interrupts, pac, peripherals};
use heapless::String;
use static_cell::StaticCell;
use yanarkaze_core::{BoardInfo, Button, Led, MemRegion, Reading, Sensor, System, Watchdog};
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    UARTE0 => buffered_uarte::InterruptHandler<peripherals::UARTE0>;
    TEMP => temp::InterruptHandler;
});

static INFO: BoardInfo = BoardInfo {
    board: "nRF52840-DK",
    chip: "nRF52840",
    cpu: "Cortex-M4F",
    cpu_mhz: Some(64),
    fpu: true,
    flash_kib: Some(1024),
    ram_kib: Some(256),
    button: "Button 1",
    hints: &["try: dump 0x10000060 8 (FICR device ID), mandel, reaction"],
};

static REGIONS: [MemRegion; 4] = [
    MemRegion { start: 0x0000_0000, end: 0x0010_0000, name: "flash" },
    MemRegion { start: 0x1000_0000, end: 0x1000_0400, name: "FICR (factory information)" },
    MemRegion { start: 0x1000_1000, end: 0x1000_1400, name: "UICR (user information)" },
    MemRegion { start: 0x2000_0000, end: 0x2004_0000, name: "RAM" },
];

struct DkLed(Output<'static>);

impl Led for DkLed {
    fn set(&mut self, on: bool) {
        // The DK LEDs are wired active low.
        self.0.set_level(if on { Level::Low } else { Level::High });
    }
}

struct DkButton(Input<'static>);

impl Button for DkButton {
    async fn wait_for_press(&mut self) {
        self.0.wait_for_falling_edge().await;
    }

    async fn wait_for_release(&mut self) {
        self.0.wait_for_high().await;
    }
}

struct DieSensor(Temp<'static>);

impl Sensor for DieSensor {
    async fn read(&mut self) -> Reading {
        // The TEMP peripheral reports a fixed-point value with 2 fractional bits.
        let quarter_degrees = self.0.read().await.to_bits();
        Reading { temp_c: quarter_degrees as f32 / 4.0, vdda_mv: None }
    }
}

/// The nRF WDT starts counting as soon as it is configured, so `start` has nothing to do.
struct DkWatchdog(WatchdogHandle);

impl Watchdog for DkWatchdog {
    fn start(&mut self) {}

    fn feed(&mut self) {
        self.0.pet();
    }
}

struct DkSystem {
    rng: Rng<'static, Blocking>,
    reset_reason: &'static str,
}

impl System for DkSystem {
    fn info(&self) -> &BoardInfo {
        &INFO
    }

    fn reset_reason(&self) -> &'static str {
        self.reset_reason
    }

    fn random_u32(&mut self) -> u32 {
        let mut bytes = [0u8; 4];
        self.rng.blocking_fill_bytes(&mut bytes);
        u32::from_le_bytes(bytes)
    }

    fn unique_id(&self) -> Option<String<32>> {
        let mut id = String::new();
        let (hi, lo) = (pac::FICR.deviceid(1).read(), pac::FICR.deviceid(0).read());
        let _ = write!(id, "{hi:08X}{lo:08X}");
        Some(id)
    }

    fn cycle_counter(&self) -> Option<u32> {
        Some(DWT::cycle_count())
    }

    fn memory_regions(&self) -> &[MemRegion] {
        &REGIONS
    }

    fn reset(&mut self) -> ! {
        cortex_m::peripheral::SCB::sys_reset()
    }
}

fn read_reset_reason() -> &'static str {
    let r = pac::POWER.resetreas().read();
    let reason = if r.dog() {
        "watchdog"
    } else if r.sreq() {
        "software reset"
    } else if r.lockup() {
        "CPU lockup"
    } else if r.resetpin() {
        "reset pin"
    } else if r.off() {
        "wake from system OFF"
    } else {
        "power on"
    };
    // The flags are sticky: write ones to clear them for the next boot.
    pac::POWER.resetreas().write_value(r);
    reason
}

#[embassy_executor::task]
async fn shell_task(sys: DkSystem, rx: BufferedUarteRx<'static>, tx: BufferedUarteTx<'static>, spawner: Spawner) {
    yanarkaze_core::shell::run(sys, rx, tx, spawner).await
}

#[embassy_executor::task]
async fn led_task(led: DkLed) {
    yanarkaze_core::led::run(led).await
}

#[embassy_executor::task]
async fn button_task(button: DkButton) {
    yanarkaze_core::button::run(button).await
}

#[embassy_executor::task]
async fn sensor_task(sensor: DieSensor) {
    yanarkaze_core::sensor::run(sensor).await
}

#[embassy_executor::task]
async fn watchdog_task(watchdog: DkWatchdog) {
    yanarkaze_core::watchdog::run(watchdog).await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    let mut core = cortex_m::Peripherals::take().unwrap();
    core.DCB.enable_trace();
    core.DWT.enable_cycle_counter();

    let reset_reason = read_reset_reason();
    info!("yanarkaze booting on nRF52840-DK, reset reason: {}", reset_reason);

    static RX_BUF: StaticCell<[u8; 256]> = StaticCell::new();
    static TX_BUF: StaticCell<[u8; 1024]> = StaticCell::new();
    let mut uart_config = uarte::Config::default();
    uart_config.baudrate = Baudrate::Baud115200;
    let uart = BufferedUarte::new(
        p.UARTE0,
        p.TIMER0,
        p.PPI_CH0,
        p.PPI_CH1,
        p.PPI_GROUP0,
        p.P0_08,
        p.P0_06,
        Irqs,
        uart_config,
        RX_BUF.init([0; 256]),
        TX_BUF.init([0; 1024]),
    );
    let (rx, tx) = uart.split();

    let mut wdt_config = wdt::Config::default();
    wdt_config.timeout_ticks = 32768 * 2; // 2 s
    let watchdog = match wdt::Watchdog::try_new::<_, 1>(p.WDT, wdt_config) {
        Ok((_wdt, [handle])) => handle,
        Err(_) => panic!("watchdog already running with another configuration"),
    };

    let sys = DkSystem { rng: Rng::new_blocking(p.RNG), reset_reason };
    let led = DkLed(Output::new(p.P0_13, Level::High, OutputDrive::Standard));
    let button = DkButton(Input::new(p.P0_11, Pull::Up));
    let sensor = DieSensor(Temp::new(p.TEMP, Irqs));

    spawner.spawn(shell_task(sys, rx, tx, spawner).unwrap());
    spawner.spawn(led_task(led).unwrap());
    spawner.spawn(button_task(button).unwrap());
    spawner.spawn(sensor_task(sensor).unwrap());
    spawner.spawn(watchdog_task(DkWatchdog(watchdog)).unwrap());
}
