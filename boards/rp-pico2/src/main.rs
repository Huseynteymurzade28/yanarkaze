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
//!   pins     -> GP0-3, GP6-14 and GP16-22 for gpio / pwm, GP26-28 for adc,
//!               I2C0 on GP4 (SDA) and GP5 (SCL)

#![no_std]
#![no_main]

use core::fmt::Write as _;

use cortex_m::peripheral::DWT;
use embassy_embedded_hal::SetConfig;
use embassy_executor::Spawner;
use embassy_rp::adc::{self, Adc, Channel};
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Flex, Input, Level, Output, Pull};
use embassy_rp::i2c::{self, AbortReason, I2c};
use embassy_rp::peripherals::{I2C0, USB};
use embassy_rp::usb::{self, Driver};
use embassy_rp::watchdog::{ResetReason, Watchdog as RpWatchdog};
use embassy_rp::{bind_interrupts, otp, pac};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, with_timeout};
use embassy_usb::class::cdc_acm::{CdcAcmClass, Receiver, Sender, State};
use embassy_usb::{Builder, UsbDevice};
use heapless::String;
use panic_halt as _;
use static_cell::StaticCell;
use yanarkaze_core::{BoardInfo, Button, HwError, Led, MemRegion, PinInfo, Reading, Sensor, System, Watchdog};

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
    ADC_IRQ_FIFO => adc::InterruptHandler;
    I2C0_IRQ => i2c::InterruptHandler<I2C0>;
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
        "button: touch GPIO15 (pin 20) to GND (pin 18); 'pins' shows the header",
        "try: mandel (M33 + FPU vs the old Pico), dump 0x0 64 (boot ROM)",
    ],
};

static REGIONS: [MemRegion; 3] = [
    MemRegion { start: 0x0000_0000, end: 0x0000_8000, name: "boot ROM" },
    MemRegion { start: 0x1000_0000, end: 0x1000_0000 + FLASH_SIZE, name: "flash (XIP)" },
    MemRegion { start: 0x2000_0000, end: 0x2008_2000, name: "SRAM" },
];

/// Header pins for the hardware commands. The notes give the physical pin and PWM channel;
/// the two channels of a slice (e.g. GP6 and GP7, both slice 3) share one frequency.
static PINS: [PinInfo; 23] = [
    PinInfo { pin: 0, gpio: true, pwm: true, adc: false, note: "pin 1, PWM 0A" },
    PinInfo { pin: 1, gpio: true, pwm: true, adc: false, note: "pin 2, PWM 0B" },
    PinInfo { pin: 2, gpio: true, pwm: true, adc: false, note: "pin 4, PWM 1A" },
    PinInfo { pin: 3, gpio: true, pwm: true, adc: false, note: "pin 5, PWM 1B" },
    PinInfo { pin: 6, gpio: true, pwm: true, adc: false, note: "pin 9, PWM 3A" },
    PinInfo { pin: 7, gpio: true, pwm: true, adc: false, note: "pin 10, PWM 3B" },
    PinInfo { pin: 8, gpio: true, pwm: true, adc: false, note: "pin 11, PWM 4A" },
    PinInfo { pin: 9, gpio: true, pwm: true, adc: false, note: "pin 12, PWM 4B" },
    PinInfo { pin: 10, gpio: true, pwm: true, adc: false, note: "pin 14, PWM 5A" },
    PinInfo { pin: 11, gpio: true, pwm: true, adc: false, note: "pin 15, PWM 5B" },
    PinInfo { pin: 12, gpio: true, pwm: true, adc: false, note: "pin 16, PWM 6A" },
    PinInfo { pin: 13, gpio: true, pwm: true, adc: false, note: "pin 17, PWM 6B" },
    PinInfo { pin: 14, gpio: true, pwm: true, adc: false, note: "pin 19, PWM 7A" },
    PinInfo { pin: 16, gpio: true, pwm: true, adc: false, note: "pin 21, PWM 0A" },
    PinInfo { pin: 17, gpio: true, pwm: true, adc: false, note: "pin 22, PWM 0B" },
    PinInfo { pin: 18, gpio: true, pwm: true, adc: false, note: "pin 24, PWM 1A" },
    PinInfo { pin: 19, gpio: true, pwm: true, adc: false, note: "pin 25, PWM 1B" },
    PinInfo { pin: 20, gpio: true, pwm: true, adc: false, note: "pin 26, PWM 2A" },
    PinInfo { pin: 21, gpio: true, pwm: true, adc: false, note: "pin 27, PWM 2B" },
    PinInfo { pin: 22, gpio: true, pwm: true, adc: false, note: "pin 29, PWM 3A" },
    PinInfo { pin: 26, gpio: false, pwm: false, adc: true, note: "pin 31, ADC0" },
    PinInfo { pin: 27, gpio: false, pwm: false, adc: true, note: "pin 32, ADC1" },
    PinInfo { pin: 28, gpio: false, pwm: false, adc: true, note: "pin 34, ADC2 (AGND is pin 33)" },
];

static RESERVED: [(&str, &str); 4] = [
    ("GP4-5", "I2C0 SDA / SCL (pins 6 and 7)"),
    ("GP15", "the button (pin 20)"),
    ("GP23-25", "power supply mode, VBUS sense, the LED"),
    ("GP29", "VSYS / 3 for the ADC"),
];

const PWM_HZ: core::ops::RangeInclusive<u32> = 10..=1_000_000;
const I2C_TIMEOUT: Duration = Duration::from_millis(50);
const I2C_SDA: usize = 4;
const I2C_SCL: usize = 5;
/// The pins' I2C function number (F3) in IO_BANK0.
const FUNC_I2C: u8 = 3;

/// The ADC is shared: the sensor task reads the temperature, the shell reads the pins.
struct AdcBank {
    adc: Adc<'static, adc::Async>,
    temp: Channel<'static>,
    /// GP26, GP27, GP28.
    pins: [Channel<'static>; 3],
}

type SharedAdc = Mutex<CriticalSectionRawMutex, AdcBank>;

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
    adc: &'static SharedAdc,
}

impl Sensor for DieSensor {
    async fn read(&mut self) -> Reading {
        let mut bank = self.adc.lock().await;
        let bank = &mut *bank;
        let raw = bank.adc.read(&mut bank.temp).await.unwrap_or(0) as f32;
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
    /// The user pins, indexed by GPIO number.
    gpios: [Option<Flex<'static>>; 30],
    /// Pins currently muxed to PWM (bit n = GPIOn).
    pwm_pins: u32,
    adc: &'static SharedAdc,
    i2c: I2c<'static, I2C0, i2c::Async>,
    i2c_config: i2c::Config,
}

impl PicoSystem {
    fn flex(&mut self, pin: u8) -> Result<&mut Flex<'static>, HwError> {
        let is_adc = PINS.iter().any(|p| p.pin == pin && p.adc);
        match self.gpios.get_mut(pin as usize).and_then(Option::as_mut) {
            Some(flex) => Ok(flex),
            None if is_adc => Err(HwError::WrongPin),
            None => Err(HwError::NoSuchPin),
        }
    }

    /// Hands a pin back to the GPIO block after it was used for PWM.
    fn unmux_pwm(&mut self, pin: u8) {
        if self.pwm_pins & (1 << pin) != 0 {
            self.pwm_pins &= !(1 << pin);
            pac::IO_BANK0
                .gpio(pin as usize)
                .ctrl()
                .write(|w| w.set_funcsel(pac::io::vals::Gpio0ctrlFuncsel::SIOB_PROC_0 as _));
        }
    }

    /// Maps the outcome of a timed I2C transfer, and frees the bus if it went wrong: a
    /// transfer cut short by the timeout can leave a device holding SDA low.
    fn i2c_result(&mut self, r: Result<Result<(), i2c::Error>, embassy_time::TimeoutError>) -> Result<(), HwError> {
        let e = match r {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(i2c::Error::Abort(AbortReason::NoAcknowledge))) => return Err(HwError::Nack),
            Ok(Err(i2c::Error::AddressOutOfRange(_))) => return Err(HwError::OutOfRange),
            Ok(Err(_)) => return Err(HwError::Bus),
            Err(_) => HwError::Timeout,
        };
        // Only a transfer that never finished can leave a device holding the bus.
        let _ = self.recover_bus();
        Err(e)
    }

    /// The standard I2C bus recovery: with the pins taken over as open-drain outputs, clock
    /// SCL until the device that holds SDA low has shifted out its byte, then send a STOP.
    fn recover_bus(&mut self) -> Result<(bool, bool), HwError> {
        let (sio, io) = (pac::SIO, pac::IO_BANK0);
        let (sda, scl) = (1u32 << I2C_SDA, 1u32 << I2C_SCL);
        let half_bit = || cortex_m::asm::delay(750); // ~5 us: a 100 kHz clock
        let sda_high = || sio.gpio_in(0).read() & sda != 0;
        let scl_high = || sio.gpio_in(0).read() & scl != 0;

        // Output value 0; "high" is output disabled, so the pull-ups do it (open drain).
        sio.gpio_out(0).value_clr().write_value(sda | scl);
        sio.gpio_oe(0).value_clr().write_value(sda | scl);
        for pin in [I2C_SDA, I2C_SCL] {
            io.gpio(pin).ctrl().write(|w| w.set_funcsel(pac::io::vals::Gpio0ctrlFuncsel::SIOB_PROC_0 as _));
        }
        half_bit();
        for _ in 0..9 {
            if sda_high() {
                break;
            }
            sio.gpio_oe(0).value_set().write_value(scl);
            half_bit();
            sio.gpio_oe(0).value_clr().write_value(scl);
            half_bit();
        }
        // STOP: SDA goes high while SCL is high.
        sio.gpio_oe(0).value_set().write_value(sda);
        half_bit();
        sio.gpio_oe(0).value_clr().write_value(sda);
        half_bit();
        let free = (sda_high(), scl_high());

        // The controller watched the lines move without it and now believes the bus is
        // busy; re-enabling it does not clear that. Reset the whole block, then configure
        // it again (that also restores the registers that only a reset initialises).
        let resets = pac::RESETS;
        resets.reset().modify(|w| w.set_i2c0(true));
        resets.reset().modify(|w| w.set_i2c0(false));
        while !resets.reset_done().read().i2c0() {}

        for pin in [I2C_SDA, I2C_SCL] {
            io.gpio(pin).ctrl().write(|w| w.set_funcsel(FUNC_I2C));
        }
        self.i2c.set_config(&self.i2c_config).map_err(|_| HwError::Bus)?;
        Ok(free)
    }
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

    fn pins(&self) -> &[PinInfo] {
        &PINS
    }

    fn reserved_pins(&self) -> &[(&'static str, &'static str)] {
        &RESERVED
    }

    fn gpio_write(&mut self, pin: u8, high: bool) -> Result<(), HwError> {
        self.flex(pin)?;
        self.unmux_pwm(pin);
        let flex = self.flex(pin)?;
        flex.set_level(if high { Level::High } else { Level::Low });
        flex.set_as_output();
        Ok(())
    }

    fn gpio_read(&mut self, pin: u8, pull: yanarkaze_core::Pull) -> Result<bool, HwError> {
        self.flex(pin)?;
        self.unmux_pwm(pin);
        let flex = self.flex(pin)?;
        flex.set_as_input();
        flex.set_pull(match pull {
            yanarkaze_core::Pull::None => Pull::None,
            yanarkaze_core::Pull::Up => Pull::Up,
            yanarkaze_core::Pull::Down => Pull::Down,
        });
        if pull == yanarkaze_core::Pull::Down {
            // RP2350 erratum E9: with the input buffer enabled, a pad that has been high
            // leaks enough current to hold itself at ~2 V against the weak pull-down. With the
            // buffer off the pull-down can drain the pin; then enable it and read at once.
            let pad = pac::PADS_BANK0.gpio(pin as usize);
            pad.modify(|w| w.set_ie(false));
            cortex_m::asm::delay(15_000); // ~100 us
            pad.modify(|w| w.set_ie(true));
            return Ok(flex.is_high());
        }
        cortex_m::asm::delay(1_500); // ~10 us for the pull to charge the pin
        Ok(flex.is_high())
    }

    fn pwm_set(&mut self, pin: u8, freq_hz: u32, duty: u16) -> Result<u32, HwError> {
        self.flex(pin)?;
        if !PWM_HZ.contains(&freq_hz) || duty > 1000 {
            return Err(HwError::OutOfRange);
        }
        // Pick the smallest divider that lets the 16-bit counter reach the frequency, which
        // keeps the duty resolution as fine as possible.
        let sys = embassy_rp::clocks::clk_sys_freq() as u64;
        let freq = freq_hz as u64;
        let div = sys.div_ceil(freq * 65_536).clamp(1, 255);
        let top = (sys / (div * freq)).clamp(2, 65_536) - 1;
        let compare = ((top + 1) * duty as u64 / 1000) as u16;

        let slice = pac::PWM.ch(((pin >> 1) & 7) as usize);
        slice.div().write(|w| {
            w.set_int(div as u8);
            w.set_frac(0);
        });
        slice.top().write(|w| w.set_top(top as u16));
        slice.cc().modify(|w| if pin % 2 == 0 { w.set_a(compare) } else { w.set_b(compare) });
        slice.csr().write(|w| w.set_en(true));
        if self.pwm_pins & (1 << pin) == 0 {
            self.pwm_pins |= 1 << pin;
            pac::IO_BANK0
                .gpio(pin as usize)
                .ctrl()
                .write(|w| w.set_funcsel(pac::io::vals::Gpio0ctrlFuncsel::PWM_A_0 as _));
        }
        Ok((sys / (div * (top + 1))) as u32)
    }

    async fn adc_read(&mut self, pin: u8) -> Result<u32, HwError> {
        let index = match pin {
            26..=28 => (pin - 26) as usize,
            _ if self.gpios.get(pin as usize).is_some_and(Option::is_some) => return Err(HwError::WrongPin),
            _ => return Err(HwError::NoSuchPin),
        };
        let mut bank = self.adc.lock().await;
        let bank = &mut *bank;
        let raw = bank.adc.read(&mut bank.pins[index]).await.map_err(|_| HwError::Bus)?;
        Ok(raw as u32 * 3300 / 4096)
    }

    fn i2c_pins(&self) -> Option<&'static str> {
        Some("SDA GP4 = pin 6, SCL GP5 = pin 7")
    }

    fn i2c_set_speed(&mut self, hz: u32) -> Result<u32, HwError> {
        let mut config = self.i2c_config;
        config.frequency = hz;
        self.i2c.set_config(&config).map_err(|_| HwError::OutOfRange)?;
        self.i2c_config = config;
        Ok(hz)
    }

    fn i2c_recover(&mut self) -> Result<(bool, bool), HwError> {
        self.recover_bus()
    }

    async fn i2c_write(&mut self, addr: u8, bytes: &[u8]) -> Result<(), HwError> {
        let r = with_timeout(I2C_TIMEOUT, self.i2c.write_async(addr, bytes.iter().copied())).await;
        self.i2c_result(r)
    }

    async fn i2c_read(&mut self, addr: u8, buf: &mut [u8]) -> Result<(), HwError> {
        let r = with_timeout(I2C_TIMEOUT, self.i2c.read_async(addr, buf)).await;
        self.i2c_result(r)
    }

    async fn i2c_write_read(&mut self, addr: u8, bytes: &[u8], buf: &mut [u8]) -> Result<(), HwError> {
        let r = with_timeout(I2C_TIMEOUT, self.i2c.write_read_async(addr, bytes.iter().copied(), buf)).await;
        self.i2c_result(r)
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

    // The ADC: the temperature sensor plus GP26-28.
    static ADC: StaticCell<SharedAdc> = StaticCell::new();
    let adc = &*ADC.init(Mutex::new(AdcBank {
        adc: Adc::new(p.ADC, Irqs, adc::Config::default()),
        temp: Channel::new_temp_sensor(p.ADC_TEMP_SENSOR),
        pins: [
            Channel::new_pin(p.PIN_26, Pull::None),
            Channel::new_pin(p.PIN_27, Pull::None),
            Channel::new_pin(p.PIN_28, Pull::None),
        ],
    }));

    // I2C0 on GP4/GP5. Most breakout boards have pull-ups; the internal ones help if not.
    let mut i2c_config = i2c::Config::default();
    i2c_config.frequency = 400_000;
    i2c_config.sda_pullup = true;
    i2c_config.scl_pullup = true;
    let i2c = I2c::new_async(p.I2C0, p.PIN_5, p.PIN_4, Irqs, i2c_config);

    let mut gpios: [Option<Flex<'static>>; 30] = Default::default();
    macro_rules! user_pins {
        ($($n:literal => $pin:ident),* $(,)?) => {
            $( gpios[$n] = Some(Flex::new(p.$pin)); )*
        };
    }
    user_pins!(
        0 => PIN_0, 1 => PIN_1, 2 => PIN_2, 3 => PIN_3, 6 => PIN_6, 7 => PIN_7, 8 => PIN_8,
        9 => PIN_9, 10 => PIN_10, 11 => PIN_11, 12 => PIN_12, 13 => PIN_13, 14 => PIN_14,
        16 => PIN_16, 17 => PIN_17, 18 => PIN_18, 19 => PIN_19, 20 => PIN_20, 21 => PIN_21,
        22 => PIN_22,
    );

    let sys = PicoSystem { rng: RoscRng, reset_reason, uid, gpios, pwm_pins: 0, adc, i2c, i2c_config };
    let rx = UsbRx { rx: receiver, buf: [0; 64], pos: 0, len: 0 };
    let led = PicoLed(Output::new(p.PIN_25, Level::Low));
    let sensor = DieSensor { adc };

    spawner.spawn(usb_task(device).unwrap());
    spawner.spawn(shell_task(sys, rx, UsbTx(sender), spawner).unwrap());
    spawner.spawn(led_task(led).unwrap());
    spawner.spawn(button_task(PinButton(Input::new(p.PIN_15, Pull::Up))).unwrap());
    spawner.spawn(sensor_task(sensor).unwrap());
    spawner.spawn(watchdog_task(PicoWatchdog(watchdog)).unwrap());
}
