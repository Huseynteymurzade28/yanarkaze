//! yanarkaze on the Raspberry Pi Pico 2 (RP2350A, running its Cortex-M33 cores).
//!
//! The Pico 2 has no USB-serial bridge, so the console is the chip's own USB port acting as
//! a CDC-ACM serial device (it shows up as /dev/ttyACM*).
//!   console  -> USB CDC-ACM
//!   LED      -> the green LED on GPIO25
//!   button   -> GPIO15 with the internal pull-up: connect it to GND (pin 20 to pin 18)
//!               (embassy-rp can only read the BOOTSEL button on the RP2040)
//!   sensor   -> the RP2350 internal temperature sensor
//!   watchdog -> RP2350 watchdog, 2 s timeout
//!   RNG      -> ring oscillator random bits (ROSC)
//!   uid      -> the 64-bit chip ID in OTP

#![no_std]
#![no_main]

use core::fmt::Write as _;

use cortex_m::peripheral::DWT;
use embassy_executor::Spawner;
use embassy_rp::adc::{self, Adc, Channel};
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::USB;
use embassy_rp::usb::{self, Driver};
use embassy_rp::watchdog::{ResetReason, Watchdog as RpWatchdog};
use embassy_rp::{bind_interrupts, otp};
use embassy_time::Duration;
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

const FLASH_SIZE: usize = 4 * 1024 * 1024;
const WATCHDOG_TIMEOUT: Duration = Duration::from_millis(2000);

static INFO: BoardInfo = BoardInfo {
    board: "Raspberry Pi Pico 2",
    chip: "RP2350A",
    cpu: "dual Cortex-M33",
    cpu_mhz: Some(150),
    fpu: true,
    flash_kib: Some(4096),
    ram_kib: Some(520),
    button: "a wire from GPIO15 (pin 20) to GND (pin 18)",
    hints: &[
        "console over native USB; the LED is on GPIO25",
        "button: touch GPIO15 (pin 20) to GND (pin 18)",
        "try: mandel (M33 + FPU vs the old Pico), dump 0x0 64 (boot ROM)",
    ],
};

static REGIONS: [MemRegion; 3] = [
    MemRegion { start: 0x0000_0000, end: 0x0000_8000, name: "boot ROM" },
    MemRegion { start: 0x1000_0000, end: 0x1000_0000 + FLASH_SIZE, name: "flash (XIP)" },
    MemRegion { start: 0x2000_0000, end: 0x2008_2000, name: "SRAM" },
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

/// A push button (or just a wire) from GPIO15 to GND; the internal pull-up keeps it high.
struct PinButton(Input<'static>);

impl Button for PinButton {
    async fn wait_for_press(&mut self) {
        self.0.wait_for_low().await;
    }

    async fn wait_for_release(&mut self) {
        self.0.wait_for_high().await;
    }
}

struct DieSensor {
    adc: Adc<'static, adc::Async>,
    channel: Channel<'static>,
}

impl Sensor for DieSensor {
    async fn read(&mut self) -> Reading {
        let raw = self.adc.read(&mut self.channel).await.unwrap_or(0) as f32;
        // RP2350 datasheet, section 12.4.6: T = 27 - (V - 0.706) / 0.001721
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

    fn cycle_counter(&self) -> Option<u32> {
        Some(DWT::cycle_count())
    }

    fn memory_regions(&self) -> &[MemRegion] {
        &REGIONS
    }

    fn enter_bootloader(&mut self) {
        // Shows up as a USB drive (and to picotool) again, as if BOOTSEL had been held.
        embassy_rp::rom_data::reset_to_usb_boot(0, 0);
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
async fn button_task(button: PinButton) {
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

    // Enable the DWT cycle counter, used by `mandel` to measure work in CPU cycles.
    let mut core = cortex_m::Peripherals::take().unwrap();
    core.DCB.enable_trace();
    core.DWT.enable_cycle_counter();

    // The 64-bit chip ID is burnt into OTP at the factory.
    let mut uid = String::new();
    if let Ok(id) = otp::get_chipid() {
        let _ = write!(uid, "{id:016X}");
    }

    // USB CDC-ACM serial port.
    let driver = Driver::new(p.USB, Irqs);
    let mut config = embassy_usb::Config::new(0xc0de, 0xcafe);
    config.manufacturer = Some("yanarkaze");
    config.product = Some("yanarkaze shell");
    config.serial_number = Some("rp-pico2");
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
    spawner.spawn(button_task(PinButton(Input::new(p.PIN_15, Pull::Up))).unwrap());
    spawner.spawn(sensor_task(sensor).unwrap());
    spawner.spawn(watchdog_task(PicoWatchdog(watchdog)).unwrap());
}
