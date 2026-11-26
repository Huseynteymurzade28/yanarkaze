//! yanarkaze on the Raspberry Pi Pico (RP2040).
//!
//! The Pico has no USB-serial bridge, so the console is the chip's own USB port acting as
//! a CDC-ACM serial device (it shows up as /dev/ttyACM*).
//!   console  -> USB CDC-ACM
//!   LED      -> the green LED on GPIO25
//!   button   -> the BOOTSEL button (polled, the Pico has no user button)
//!   sensor   -> the RP2040 internal temperature sensor (ADC channel 4)
//!   watchdog -> RP2040 watchdog, 2 s timeout
//!   RNG      -> ring oscillator random bits (ROSC)

#![no_std]
#![no_main]

use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_rp::adc::{self, Adc, Channel};
use embassy_rp::bootsel::is_bootsel_pressed;
use embassy_rp::clocks::RoscRng;
use embassy_rp::flash::{Blocking, Flash};
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{BOOTSEL, USB};
use embassy_rp::usb::{self, Driver};
use embassy_rp::watchdog::{ResetReason, Watchdog as RpWatchdog};
use embassy_rp::{Peri, bind_interrupts};
use embassy_time::{Duration, Timer};
use embassy_usb::class::cdc_acm::{CdcAcmClass, Receiver, Sender, State};
use embassy_usb::{Builder, UsbDevice};
use heapless::String;
use panic_halt as _;
use static_cell::StaticCell;
use yanarkaze_core::{BoardInfo, Button, Led, MemRegion, Reading, Sensor, System, Watchdog};

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
    ADC_IRQ_FIFO => adc::InterruptHandler;
});

const FLASH_SIZE: usize = 2 * 1024 * 1024;
const WATCHDOG_TIMEOUT: Duration = Duration::from_millis(2000);

static INFO: BoardInfo = BoardInfo {
    board: "Raspberry Pi Pico",
    chip: "RP2040",
    cpu: "dual Cortex-M0+",
    cpu_mhz: Some(125),
    fpu: false,
    flash_kib: Some(2048),
    ram_kib: Some(264),
    button: "the BOOTSEL button",
    hints: &["console over native USB; the LED is on GPIO25", "try: mandel (no FPU here!), dump 0x0 64 (boot ROM)"],
};

static REGIONS: [MemRegion; 3] = [
    MemRegion { start: 0x0000_0000, end: 0x0000_4000, name: "boot ROM" },
    MemRegion { start: 0x1000_0000, end: 0x1000_0000 + FLASH_SIZE, name: "flash (XIP)" },
    MemRegion { start: 0x2000_0000, end: 0x2004_2000, name: "SRAM" },
];

// ---------------------------------------------------------------------------
// USB serial console
// ---------------------------------------------------------------------------

type UsbDriver = Driver<'static, USB>;

/// Writes in packets shorter than the 64-byte endpoint size, so the host never waits for
/// a zero-length packet to finish a transfer.
struct UsbTx(Sender<'static, UsbDriver>);

impl embedded_io_async::ErrorType for UsbTx {
    type Error = core::convert::Infallible;
}

impl embedded_io_async::Write for UsbTx {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.0.wait_connection().await;
        let n = buf.len().min(63);
        // If the host goes away mid-write the bytes are simply dropped.
        let _ = self.0.write_packet(&buf[..n]).await;
        Ok(n)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        // Every packet is handed to the USB controller as soon as it is written.
        Ok(())
    }
}

/// Keeps leftover bytes of a USB packet between reads, so callers may read any amount.
struct UsbRx {
    rx: Receiver<'static, UsbDriver>,
    buf: [u8; 64],
    pos: usize,
    len: usize,
}

impl embedded_io_async::ErrorType for UsbRx {
    type Error = core::convert::Infallible;
}

impl embedded_io_async::Read for UsbRx {
    async fn read(&mut self, out: &mut [u8]) -> Result<usize, Self::Error> {
        while self.pos == self.len {
            self.rx.wait_connection().await;
            if let Ok(n) = self.rx.read_packet(&mut self.buf).await {
                self.pos = 0;
                self.len = n;
            }
        }
        let n = out.len().min(self.len - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// Trait implementations
// ---------------------------------------------------------------------------

struct PicoLed(Output<'static>);

impl Led for PicoLed {
    fn set(&mut self, on: bool) {
        self.0.set_level(if on { Level::High } else { Level::Low });
    }
}

/// BOOTSEL is wired to the flash chip select, so it can only be polled, not used as an
/// interrupt. Polling every 5 ms is plenty for a human finger.
struct BootselButton(Peri<'static, BOOTSEL>);

impl BootselButton {
    fn pressed(&mut self) -> bool {
        is_bootsel_pressed(self.0.reborrow())
    }
}

impl Button for BootselButton {
    async fn wait_for_press(&mut self) {
        while !self.pressed() {
            Timer::after_millis(5).await;
        }
    }

    async fn wait_for_release(&mut self) {
        while self.pressed() {
            Timer::after_millis(5).await;
        }
    }
}

struct DieSensor {
    adc: Adc<'static, adc::Async>,
    channel: Channel<'static>,
}

impl Sensor for DieSensor {
    async fn read(&mut self) -> Reading {
        let raw = self.adc.read(&mut self.channel).await.unwrap_or(0) as f32;
        // RP2040 datasheet, section 4.9.5: T = 27 - (V - 0.706) / 0.001721
        let volts = raw * 3.3 / 4096.0;
        Reading { temp_c: 27.0 - (volts - 0.706) / 0.001721, vdda_mv: None }
    }
}

struct PicoWatchdog(RpWatchdog);

impl Watchdog for PicoWatchdog {
    fn start(&mut self) {
        self.0.start(WATCHDOG_TIMEOUT);
    }

    fn feed(&mut self) {
        self.0.feed(WATCHDOG_TIMEOUT);
    }
}

struct PicoSystem {
    rng: RoscRng,
    reset_reason: &'static str,
    uid: String<32>,
}

impl System for PicoSystem {
    fn info(&self) -> &BoardInfo {
        &INFO
    }

    fn reset_reason(&self) -> &'static str {
        self.reset_reason
    }

    fn random_u32(&mut self) -> u32 {
        self.rng.next_u32()
    }

    fn unique_id(&self) -> Option<String<32>> {
        Some(self.uid.clone())
    }

    fn memory_regions(&self) -> &[MemRegion] {
        &REGIONS
    }

    fn reset(&mut self) -> ! {
        cortex_m::peripheral::SCB::sys_reset()
    }
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, UsbDriver>) {
    device.run().await
}

#[embassy_executor::task]
async fn shell_task(sys: PicoSystem, rx: UsbRx, tx: UsbTx, spawner: Spawner) {
    yanarkaze_core::shell::run(sys, rx, tx, spawner).await
}

#[embassy_executor::task]
async fn led_task(led: PicoLed) {
    yanarkaze_core::led::run(led).await
}

#[embassy_executor::task]
async fn button_task(button: BootselButton) {
    yanarkaze_core::button::run(button).await
}

#[embassy_executor::task]
async fn sensor_task(sensor: DieSensor) {
    yanarkaze_core::sensor::run(sensor).await
}

#[embassy_executor::task]
async fn watchdog_task(watchdog: PicoWatchdog) {
    yanarkaze_core::watchdog::run(watchdog).await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let watchdog = RpWatchdog::new(p.WATCHDOG);
    let reset_reason = match watchdog.reset_reason() {
        Some(ResetReason::TimedOut) => "watchdog",
        Some(ResetReason::Forced) => "forced by the watchdog",
        None => "power on / reset",
    };

    // The 64-bit unique ID lives in the external flash chip.
    let mut uid = String::new();
    let mut id = [0u8; 8];
    let mut flash = Flash::<_, Blocking, FLASH_SIZE>::new_blocking(p.FLASH);
    if flash.blocking_unique_id(&mut id).is_ok() {
        for byte in id {
            let _ = write!(uid, "{byte:02X}");
        }
    }

    // USB CDC-ACM serial port.
    let driver = Driver::new(p.USB, Irqs);
    let mut config = embassy_usb::Config::new(0xc0de, 0xcafe);
    config.manufacturer = Some("yanarkaze");
    config.product = Some("yanarkaze shell");
    config.serial_number = Some("rp-pico");
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();
    static CDC_STATE: StaticCell<State> = StaticCell::new();
    let mut builder = Builder::new(
        driver,
        config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        MSOS_DESC.init([0; 256]),
        CONTROL_BUF.init([0; 64]),
    );
    let class = CdcAcmClass::new(&mut builder, CDC_STATE.init(State::new()), 64);
    let (sender, receiver) = class.split();
    let device = builder.build();

    let sys = PicoSystem { rng: RoscRng, reset_reason, uid };
    let rx = UsbRx { rx: receiver, buf: [0; 64], pos: 0, len: 0 };
    let led = PicoLed(Output::new(p.PIN_25, Level::Low));
    let sensor = DieSensor {
        adc: Adc::new(p.ADC, Irqs, adc::Config::default()),
        channel: Channel::new_temp_sensor(p.ADC_TEMP_SENSOR),
    };

    spawner.spawn(usb_task(device).unwrap());
    spawner.spawn(shell_task(sys, rx, UsbTx(sender), spawner).unwrap());
    spawner.spawn(led_task(led).unwrap());
    spawner.spawn(button_task(BootselButton(p.BOOTSEL)).unwrap());
    spawner.spawn(sensor_task(sensor).unwrap());
    spawner.spawn(watchdog_task(PicoWatchdog(watchdog)).unwrap());
}
