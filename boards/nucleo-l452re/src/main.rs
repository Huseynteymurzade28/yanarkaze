//! yanarkaze on the ST NUCLEO-L452RE.
//!
//! This file only wires the STM32 peripherals to the traits of `yanarkaze-core`:
//!   console  -> USART2 on the ST-LINK virtual COM port, TX via DMA, RX via a DMA ring buffer
//!   LED      -> LD2 on PA5 (NUCLEO-L452RE) and LD4 on PB13 (NUCLEO-L452RE-P)
//!   button   -> blue USER button on PC13 (EXTI interrupt)
//!   sensor   -> internal temperature sensor + VREFINT through ADC1
//!   watchdog -> IWDG, 2 s timeout
//!   RNG      -> hardware TRNG (48 MHz from HSI48)
//!   I2C      -> I2C1 on Arduino D15 (SCL, PB8) and D14 (SDA: PB9, or PB7 on the -P board)

#![no_std]
#![no_main]

use core::fmt::Write as _;

use cortex_m::peripheral::DWT;
use defmt::info;
use embassy_executor::Spawner;
use embassy_stm32::adc::{Adc, SampleTime, Temperature, VrefInt};
use embassy_embedded_hal::SetConfig;
use embassy_stm32::exti::{self, ExtiInput};
use embassy_stm32::i2c::{self, I2c};
use embassy_stm32::time::Hertz;
use embassy_stm32::gpio::{Input, Level, Output, Pull, Speed};
use embassy_stm32::mode::{Async, Blocking};
use embassy_stm32::peripherals::{ADC1, IWDG, RNG};
use embassy_stm32::rng::{self, Rng};
use embassy_stm32::usart::{self, RingBufferedUartRx, Uart, UartTx};
use embassy_stm32::wdg::IndependentWatchdog;
use embassy_stm32::{Config, Peri, bind_interrupts, interrupt, pac, peripherals};
use embassy_time::{Duration, Timer};
use heapless::String;
use static_cell::StaticCell;
use yanarkaze_core::{BoardInfo, Button, HwError, Led, MemRegion, Reading, Sensor, System, Watchdog};
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USART2 => usart::InterruptHandler<peripherals::USART2>;
    DMA1_CHANNEL6 => embassy_stm32::dma::InterruptHandler<peripherals::DMA1_CH6>;
    DMA1_CHANNEL7 => embassy_stm32::dma::InterruptHandler<peripherals::DMA1_CH7>;
    EXTI15_10 => exti::InterruptHandler<interrupt::typelevel::EXTI15_10>;
    RNG => rng::InterruptHandler<peripherals::RNG>;
});

const I2C_TIMEOUT: Duration = Duration::from_millis(50);
/// I2C1 SCL on GPIOB. SDA is chosen at boot, see `find_sda`.
const I2C_SCL: usize = 8;

const CPU_MHZ: u32 = 80;

static INFO: BoardInfo = BoardInfo {
    board: "NUCLEO-L452RE",
    chip: "STM32L452RE",
    cpu: "Cortex-M4F",
    cpu_mhz: Some(CPU_MHZ),
    fpu: true,
    flash_kib: Some(512),
    ram_kib: Some(160),
    button: "the blue USER button",
    hints: &["try: led morse SOS, reaction, mandel, dump 0x1FFF7590 12"],
};

/// Regions that are safe to read on the STM32L452 (anything else may HardFault).
static REGIONS: [MemRegion; 4] = [
    MemRegion { start: 0x0800_0000, end: 0x0808_0000, name: "flash" },
    MemRegion { start: 0x1FFF_0000, end: 0x1FFF_7400, name: "system memory / OTP" },
    MemRegion { start: 0x1FFF_7500, end: 0x1FFF_7600, name: "factory data (UID, calibration)" },
    MemRegion { start: 0x2000_0000, end: 0x2002_8000, name: "SRAM" },
];

// ---------------------------------------------------------------------------
// Trait implementations
// ---------------------------------------------------------------------------

/// The user LED is LD2 on PA5 (NUCLEO-L452RE) or LD4 on PB13 (NUCLEO-L452RE-P).
/// Both are driven so the same firmware works on either board.
struct NucleoLed {
    a: Output<'static>,
    b: Output<'static>,
}

impl Led for NucleoLed {
    fn set(&mut self, on: bool) {
        let level = if on { Level::High } else { Level::Low };
        self.a.set_level(level);
        self.b.set_level(level);
    }
}

/// The blue button is active low; the CPU sleeps until the EXTI interrupt fires.
struct UserButton(ExtiInput<'static, Async>);

impl Button for UserButton {
    async fn wait_for_press(&mut self) {
        self.0.wait_for_falling_edge().await;
    }

    async fn wait_for_release(&mut self) {
        self.0.wait_for_high().await;
    }
}

// Factory calibration values (STM32L452 datasheet), measured at VDDA = 3.0 V.
const VREFINT_CAL: *const u16 = 0x1FFF_75AA as *const u16;
const TS_CAL1: *const u16 = 0x1FFF_75A8 as *const u16; // 30 degC
const TS_CAL2: *const u16 = 0x1FFF_75CA as *const u16; // 130 degC
const CAL_VDDA_MV: f32 = 3000.0;

struct DieSensor {
    adc: Adc<'static, ADC1>,
    vrefint: VrefInt,
    temperature: Temperature,
    vref_cal: f32,
    ts_cal1: f32,
    ts_cal2: f32,
}

impl DieSensor {
    async fn new(adc: Peri<'static, ADC1>) -> Self {
        let adc = Adc::new(adc);
        let vrefint = adc.enable_vrefint();
        let temperature = adc.enable_temperature();
        // Workaround: stm32-metapac 21 swaps CH17SEL/CH18SEL for STM32L4. Per RM0394 the
        // temperature sensor is bit 23 and VBAT is bit 24, so fix the register by hand.
        pac::ADC1_COMMON.ccr().modify(|w| w.0 = (w.0 | (1 << 23)) & !(1 << 24));
        Timer::after_millis(1).await; // let the sensors stabilise

        // SAFETY: fixed factory calibration addresses in system memory.
        let (vref_cal, ts_cal1, ts_cal2) = unsafe {
            (VREFINT_CAL.read_volatile() as f32, TS_CAL1.read_volatile() as f32, TS_CAL2.read_volatile() as f32)
        };
        Self { adc, vrefint, temperature, vref_cal, ts_cal1, ts_cal2 }
    }
}

impl Sensor for DieSensor {
    async fn read(&mut self) -> Reading {
        let vref_raw = self.adc.blocking_read(&mut self.vrefint, SampleTime::CYCLES640_5) as f32;
        let ts_raw = self.adc.blocking_read(&mut self.temperature, SampleTime::CYCLES640_5) as f32;

        let vdda_mv = CAL_VDDA_MV * self.vref_cal / vref_raw;
        // Scale the reading to the calibration voltage, then interpolate between the two points.
        let ts_scaled = ts_raw * vdda_mv / CAL_VDDA_MV;
        let temp_c = (130.0 - 30.0) / (self.ts_cal2 - self.ts_cal1) * (ts_scaled - self.ts_cal1) + 30.0;
        Reading { temp_c, vdda_mv: Some(vdda_mv as u32) }
    }
}

struct Iwdg(IndependentWatchdog<'static, IWDG>);

impl Watchdog for Iwdg {
    fn start(&mut self) {
        self.0.unleash();
    }

    fn feed(&mut self) {
        self.0.pet();
    }
}

/// The console's TX with a bounded `flush`.
///
/// embassy-stm32 0.6's `RingBufferedUartRx` clears every USART status flag it saw when it
/// checks for errors, including TC (transmission complete), which belongs to TX. If a
/// transmission ends just before such a check, TC is gone and `UartTx::flush`, which waits
/// for it, never returns: the shell froze after a minute or two of snake. `write` already
/// waits for its DMA transfer, so all `flush` adds is the last two bytes leaving the shift
/// register (under 200 us at 115200 baud); a 2 ms cap is plenty, and the next transmission
/// sets TC again.
struct ConsoleTx(UartTx<'static, Async>);

impl embedded_io_async::ErrorType for ConsoleTx {
    type Error = usart::Error;
}

impl embedded_io_async::Write for ConsoleTx {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.0.write(buf).await?;
        Ok(buf.len())
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        // A timeout means TC was lost, not that anything failed.
        let _ = embassy_time::with_timeout(Duration::from_millis(2), self.0.flush()).await;
        Ok(())
    }
}

struct NucleoSystem {
    rng: Rng<'static, RNG>,
    reset_reason: &'static str,
    i2c: I2c<'static, Blocking, i2c::Master>,
    /// GPIOB pin number of SDA: 9 on the NUCLEO-L452RE, 7 on the NUCLEO-L452RE-P.
    sda: usize,
}

impl NucleoSystem {
    /// Maps the outcome of an I2C transfer, and frees the bus if it never finished.
    fn i2c_result(&mut self, r: Result<(), i2c::Error>) -> Result<(), HwError> {
        match r {
            Ok(()) => Ok(()),
            Err(i2c::Error::Nack) => Err(HwError::Nack),
            Err(i2c::Error::Timeout) => {
                let _ = self.recover_bus();
                Err(HwError::Timeout)
            }
            Err(_) => Err(HwError::Bus),
        }
    }

    /// The standard I2C bus recovery: with the pins switched to open-drain GPIO outputs,
    /// clock SCL until the device holding SDA low lets go, send a STOP, then hand the pins
    /// back to I2C1 and reset its state machine (clearing PE does that on this I2C block).
    fn recover_bus(&mut self) -> Result<(bool, bool), HwError> {
        use pac::gpio::vals::Moder;
        let gpio = pac::GPIOB;
        let sda = self.sda;
        let half_bit = || cortex_m::asm::delay(CPU_MHZ * 5); // ~5 us: a 100 kHz clock
        let high = |pin: usize| gpio.idr().read().idr(pin) == pac::gpio::vals::Idr::HIGH;
        let release = |pin: usize| gpio.bsrr().write(|w| w.set_bs(pin, true));
        let pull_low = |pin: usize| gpio.bsrr().write(|w| w.set_br(pin, true));

        pac::I2C1.cr1().modify(|w| w.set_pe(false));
        // The driver already set the pins to open drain; only the mode changes.
        release(I2C_SCL);
        release(sda);
        gpio.moder().modify(|w| {
            w.set_moder(I2C_SCL, Moder::OUTPUT);
            w.set_moder(sda, Moder::OUTPUT);
        });
        half_bit();
        for _ in 0..9 {
            if high(sda) {
                break;
            }
            pull_low(I2C_SCL);
            half_bit();
            release(I2C_SCL);
            half_bit();
        }
        // STOP: SDA goes high while SCL is high.
        pull_low(sda);
        half_bit();
        release(sda);
        half_bit();
        let free = (high(sda), high(I2C_SCL));

        gpio.moder().modify(|w| {
            w.set_moder(I2C_SCL, Moder::ALTERNATE);
            w.set_moder(sda, Moder::ALTERNATE);
        });
        pac::I2C1.cr1().modify(|w| w.set_pe(true));
        Ok(free)
    }
}

impl System for NucleoSystem {
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
        let mut id = String::new();
        let _ = id.write_str(embassy_stm32::uid::uid_hex());
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

    fn i2c_pins(&self) -> Option<&'static str> {
        Some(if self.sda == 7 {
            "SDA D14 = PB7, SCL D15 = PB8, the two pins above AREF"
        } else {
            "SDA D14 = PB9, SCL D15 = PB8, the two pins above AREF"
        })
    }

    fn i2c_set_speed(&mut self, hz: u32) -> Result<u32, HwError> {
        if !(10_000..=1_000_000).contains(&hz) {
            return Err(HwError::OutOfRange);
        }
        self.i2c.set_config(&Hertz(hz)).map_err(|_| HwError::OutOfRange)?;
        Ok(hz)
    }

    fn i2c_recover(&mut self) -> Result<(bool, bool), HwError> {
        self.recover_bus()
    }

    async fn i2c_write(&mut self, addr: u8, bytes: &[u8]) -> Result<(), HwError> {
        let r = self.i2c.blocking_write(addr, bytes);
        self.i2c_result(r)
    }

    async fn i2c_read(&mut self, addr: u8, buf: &mut [u8]) -> Result<(), HwError> {
        let r = self.i2c.blocking_read(addr, buf);
        self.i2c_result(r)
    }

    async fn i2c_write_read(&mut self, addr: u8, bytes: &[u8], buf: &mut [u8]) -> Result<(), HwError> {
        let r = self.i2c.blocking_write_read(addr, bytes, buf);
        self.i2c_result(r)
    }
}

/// Arduino D14 (SDA) is PB9 on the NUCLEO-L452RE but PB7 on the NUCLEO-L452RE-P, and the
/// two boards run the same firmware. An I2C module has pull-ups on its lines, so with the
/// MCU's weaker pull-downs on both candidates, the one wired to the module reads high.
/// With nothing attached both read low, and PB9 is the default. Returns the pin number.
fn find_sda(pb7: Peri<'_, peripherals::PB7>, pb9: Peri<'_, peripherals::PB9>) -> usize {
    let pb7 = Input::new(pb7, Pull::Down);
    let pb9 = Input::new(pb9, Pull::Down);
    cortex_m::asm::delay(CPU_MHZ * 20); // let the lines settle, ~20 us
    if pb7.is_high() && pb9.is_low() { 7 } else { 9 }
}

fn read_reset_reason() -> &'static str {
    let csr = pac::RCC.csr().read();
    let reason = if csr.iwdgrstf() {
        "watchdog (IWDG)"
    } else if csr.wwdgrstf() {
        "window watchdog"
    } else if csr.sftrstf() {
        "software reset"
    } else if csr.borrstf() {
        "power on"
    } else if csr.pinrstf() {
        "reset pin / debugger"
    } else {
        "unknown"
    };
    // Clear the flags so the next boot reports a fresh reason.
    pac::RCC.csr().modify(|w| w.set_rmvf(true));
    reason
}

// ---------------------------------------------------------------------------
// Tasks: thin wrappers, all the logic lives in yanarkaze-core
// ---------------------------------------------------------------------------

#[embassy_executor::task]
async fn shell_task(sys: NucleoSystem, rx: RingBufferedUartRx<'static>, tx: ConsoleTx, spawner: Spawner) {
    yanarkaze_core::shell::run(sys, rx, tx, spawner).await
}

#[embassy_executor::task]
async fn led_task(led: NucleoLed) {
    yanarkaze_core::led::run(led).await
}

#[embassy_executor::task]
async fn button_task(button: UserButton) {
    yanarkaze_core::button::run(button).await
}

#[embassy_executor::task]
async fn sensor_task(adc: Peri<'static, ADC1>) {
    yanarkaze_core::sensor::run(DieSensor::new(adc).await).await
}

#[embassy_executor::task]
async fn watchdog_task(wdg: Iwdg) {
    yanarkaze_core::watchdog::run(wdg).await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut config = Config::default();
    {
        use embassy_stm32::rcc::*;
        // 16 MHz HSI -> PLL -> 80 MHz system clock (the max for STM32L4).
        config.rcc.hsi = true;
        config.rcc.pll = Some(Pll {
            source: PllSource::HSI,
            prediv: PllPreDiv::DIV1,
            mul: PllMul::MUL10,
            divp: None,
            divq: None,
            divr: Some(PllRDiv::DIV2),
        });
        config.rcc.sys = Sysclk::PLL1_R;
        config.rcc.mux.adcsel = mux::Adcsel::SYS;
        // The true random number generator needs a 48 MHz clock.
        config.rcc.hsi48 = Some(Hsi48Config { sync_from_usb: false });
        config.rcc.mux.clk48sel = mux::Clk48sel::HSI48;
    }
    let p = embassy_stm32::init(config);

    // Enable the DWT cycle counter, used by `mandel` to measure work in CPU cycles.
    let mut core = cortex_m::Peripherals::take().unwrap();
    core.DCB.enable_trace();
    core.DWT.enable_cycle_counter();

    let reset_reason = read_reset_reason();
    info!("yanarkaze booting on NUCLEO-L452RE, reset reason: {}", reset_reason);

    // USART2 on PA2/PA3 is wired to the ST-LINK virtual COM port (/dev/ttyACM0).
    // RX uses a circular DMA buffer, so no byte is lost even while the shell is busy.
    static RX_DMA_BUF: StaticCell<[u8; 256]> = StaticCell::new();
    let mut uart_config = usart::Config::default();
    uart_config.baudrate = 115_200;
    let uart = Uart::new(p.USART2, p.PA3, p.PA2, p.DMA1_CH7, p.DMA1_CH6, Irqs, uart_config).unwrap();
    let (uart_tx, uart_rx) = uart.split();
    let uart_rx = uart_rx.into_ring_buffered(RX_DMA_BUF.init([0; 256]));

    let led =
        NucleoLed { a: Output::new(p.PA5, Level::Low, Speed::Low), b: Output::new(p.PB13, Level::Low, Speed::Low) };
    let button = UserButton(ExtiInput::new(p.PC13, p.EXTI13, Pull::None, Irqs));
    let wdg = Iwdg(IndependentWatchdog::new(p.IWDG, 2_000_000)); // 2 s timeout
    // I2C1 on the Arduino header's SCL/SDA (D15/D14). The internal pull-ups are weak
    // (about 40k); most OLED modules bring their own, which is what makes 400 kHz work.
    //
    // Blocking: with I2C1 on DMA2 (DMA1 channels 6 and 7 are the console's) the console
    // froze right at boot. That was probably the lost-TC bug `ConsoleTx` works around, but
    // DMA I2C has not been retried since. A full OLED frame at 400 kHz holds the executor
    // for about 25 ms; console input is safe meanwhile in its DMA ring buffer.
    let mut i2c_config = i2c::Config::default();
    i2c_config.frequency = Hertz(400_000);
    i2c_config.sda_pullup = true;
    i2c_config.scl_pullup = true;
    i2c_config.timeout = I2C_TIMEOUT;
    let (mut pb7, mut pb9) = (p.PB7, p.PB9);
    let sda = find_sda(pb7.reborrow(), pb9.reborrow());
    info!("I2C1 SDA on PB{}", sda);
    let i2c = if sda == 7 {
        I2c::new_blocking(p.I2C1, p.PB8, pb7, i2c_config)
    } else {
        I2c::new_blocking(p.I2C1, p.PB8, pb9, i2c_config)
    };
    let sys = NucleoSystem { rng: Rng::new(p.RNG, Irqs), reset_reason, i2c, sda };

    spawner.spawn(shell_task(sys, uart_rx, ConsoleTx(uart_tx), spawner).unwrap());
    spawner.spawn(led_task(led).unwrap());
    spawner.spawn(button_task(button).unwrap());
    spawner.spawn(sensor_task(p.ADC1).unwrap());
    spawner.spawn(watchdog_task(wdg).unwrap());

    info!("all tasks spawned");
}
