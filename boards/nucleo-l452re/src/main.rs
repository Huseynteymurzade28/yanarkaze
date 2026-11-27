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
//!   pins     -> GPIO, ADC1 and TIM2 PWM on the Arduino pins that are the same on both boards
//!   settings -> the last 2 KiB flash page

#![no_std]
#![no_main]
// `PA + 0` is written out so it reads like the pin name PA0.
#![allow(clippy::identity_op)]

use core::fmt::Write as _;

use cortex_m::peripheral::DWT;
use defmt::info;
use embassy_executor::Spawner;
use embassy_stm32::adc::{Adc, AdcChannel, AnyAdcChannel, SampleTime, Temperature, VrefInt};
use embassy_embedded_hal::SetConfig;
use embassy_stm32::exti::{self, ExtiInput};
use embassy_stm32::flash::{self, FLASH_BASE, FLASH_SIZE, Flash, WRITE_SIZE};
use embassy_stm32::i2c::{self, I2c};
use embassy_stm32::time::Hertz;
use embassy_stm32::gpio::{Input, Level, Output, Pull, Speed};
use embassy_stm32::mode::{Async, Blocking};
use embassy_stm32::peripherals::{ADC1, IWDG, RNG};
use embassy_stm32::rng::{self, Rng};
use embassy_stm32::usart::{self, RingBufferedUartRx, Uart, UartTx};
use embassy_stm32::wdg::IndependentWatchdog;
use embassy_stm32::{Config, Peri, bind_interrupts, interrupt, pac, peripherals};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer};
use heapless::String;
use static_cell::StaticCell;
use yanarkaze_core::{
    BoardInfo, Button, HwError, Led, MemRegion, PinInfo, PinStyle, Pull as CorePull, Reading, Sensor, System, Watchdog,
};
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
    hints: &["try: led morse SOS, reaction, mandel, pins, knob A0 D3, dump 0x1FFF7590 12"],
};

// ---------------------------------------------------------------------------
// Pins for the hardware commands
// ---------------------------------------------------------------------------
//
// The NUCLEO-L452RE and the NUCLEO-L452RE-P route many Arduino pins to different MCU pins
// (A2, A3, D5, D7-D13 differ), and one firmware runs on both. So the shell only offers the
// Arduino pins that are the same on both boards. Pin numbers are port * 16 + bit
// (`PinStyle::Port`): PA0 = 0, PB3 = 19, PC1 = 33.

/// Settings live in the last flash page (2 KiB on the STM32L452), far past the firmware.
const SETTINGS_PAGE: u32 = 2048;
const SETTINGS_OFFSET: u32 = FLASH_SIZE as u32 - SETTINGS_PAGE;

/// Where the firmware image ends in flash: the initial values of `.data` are its last part.
fn image_end() -> u32 {
    unsafe extern "C" {
        static __sidata: u8;
        static __sdata: u8;
        static __edata: u8;
    }
    // Taking the address of an extern static is safe; only the addresses are used.
    let data_len = (&raw const __edata) as u32 - (&raw const __sdata) as u32;
    (&raw const __sidata) as u32 + data_len
}

const PA: u8 = 0;
const PB: u8 = 16;
const PC: u8 = 32;

static PINS: [PinInfo; 8] = [
    PinInfo { pin: PA + 0, gpio: true, pwm: false, adc: true, note: "A0 (ADC1 IN5)" },
    PinInfo { pin: PA + 1, gpio: true, pwm: false, adc: true, note: "A1 (ADC1 IN6)" },
    PinInfo { pin: PC + 1, gpio: true, pwm: false, adc: true, note: "A4 (ADC1 IN2)" },
    PinInfo { pin: PC + 0, gpio: true, pwm: false, adc: true, note: "A5 (ADC1 IN1)" },
    PinInfo { pin: PA + 10, gpio: true, pwm: false, adc: false, note: "D2" },
    PinInfo { pin: PB + 3, gpio: true, pwm: true, adc: false, note: "D3 (TIM2 CH2)" },
    PinInfo { pin: PB + 5, gpio: true, pwm: false, adc: false, note: "D4" },
    PinInfo { pin: PB + 10, gpio: true, pwm: true, adc: false, note: "D6 (TIM2 CH3, same frequency as D3)" },
];

static ALIASES: [(&str, u8); 8] = [
    ("A0", PA + 0),
    ("A1", PA + 1),
    ("A4", PC + 1),
    ("A5", PC + 0),
    ("D2", PA + 10),
    ("D3", PB + 3),
    ("D4", PB + 5),
    ("D6", PB + 10),
];

static RESERVED: [(&str, &str); 5] = [
    ("PA2/3", "console (USART2 to the ST-LINK), Arduino D1/D0"),
    ("PA5", "LED LD2 (NUCLEO-L452RE)"),
    ("PB13", "LED LD4 (NUCLEO-L452RE-P)"),
    ("PC13", "blue USER button"),
    ("D14/15", "I2C1 (SDA, SCL)"),
];

/// The ADC pins in `AdcBank::pins` order.
const ADC_PINS: [u8; 4] = [PA + 0, PA + 1, PC + 1, PC + 0];

/// PWM pins: (pin, TIM2 channel index 0..=3). Both are alternate function 1.
const PWM_PINS: [(u8, usize); 2] = [(PB + 3, 1), (PB + 10, 2)];
const TIM2_HZ: u32 = CPU_MHZ * 1_000_000; // APB1 is not divided, so TIM2 runs at SYSCLK

fn gpio_port(pin: u8) -> pac::gpio::Gpio {
    match pin / 16 {
        0 => pac::GPIOA,
        1 => pac::GPIOB,
        _ => pac::GPIOC,
    }
}

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

/// ADC1 with everything read through it, shared by the sensor task and the `adc` command.
struct AdcBank {
    adc: Adc<'static, ADC1>,
    vrefint: VrefInt,
    temperature: Temperature,
    vref_cal: f32,
    ts_cal1: f32,
    ts_cal2: f32,
    pins: [AnyAdcChannel<'static, ADC1>; 4],
}

type SharedAdc = Mutex<CriticalSectionRawMutex, AdcBank>;

impl AdcBank {
    async fn new(adc: Peri<'static, ADC1>, pins: [AnyAdcChannel<'static, ADC1>; 4]) -> Self {
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
        Self { adc, vrefint, temperature, vref_cal, ts_cal1, ts_cal2, pins }
    }

    /// The supply voltage, from the internal reference and its factory calibration.
    fn vdda_mv(&mut self) -> f32 {
        let vref_raw = self.adc.blocking_read(&mut self.vrefint, SampleTime::CYCLES640_5) as f32;
        CAL_VDDA_MV * self.vref_cal / vref_raw
    }
}

struct DieSensor(&'static SharedAdc);

impl Sensor for DieSensor {
    async fn read(&mut self) -> Reading {
        let mut bank = self.0.lock().await;
        let vdda_mv = bank.vdda_mv();
        let bank = &mut *bank;
        let ts_raw = bank.adc.blocking_read(&mut bank.temperature, SampleTime::CYCLES640_5) as f32;
        // Scale the reading to the calibration voltage, then interpolate between the two points.
        let ts_scaled = ts_raw * vdda_mv / CAL_VDDA_MV;
        let temp_c = (130.0 - 30.0) / (bank.ts_cal2 - bank.ts_cal1) * (ts_scaled - bank.ts_cal1) + 30.0;
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
    adc: &'static SharedAdc,
    flash: Flash<'static, flash::Blocking>,
}

impl NucleoSystem {
    fn check_pin(&self, pin: u8, can: impl Fn(&PinInfo) -> bool) -> Result<(), HwError> {
        match PINS.iter().find(|p| p.pin == pin) {
            Some(p) if can(p) => Ok(()),
            Some(_) => Err(HwError::WrongPin),
            None => Err(HwError::NoSuchPin),
        }
    }

    fn set_mode(pin: u8, mode: pac::gpio::vals::Moder) {
        gpio_port(pin).moder().modify(|w| w.set_moder((pin % 16) as usize, mode));
    }

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

    fn pins(&self) -> &[PinInfo] {
        &PINS
    }

    fn pin_style(&self) -> PinStyle {
        PinStyle::Port
    }

    fn pin_aliases(&self) -> &[(&'static str, u8)] {
        &ALIASES
    }

    fn reserved_pins(&self) -> &[(&'static str, &'static str)] {
        &RESERVED
    }

    fn gpio_write(&mut self, pin: u8, high: bool) -> Result<(), HwError> {
        use pac::gpio::vals::{Moder, Ot};
        self.check_pin(pin, |p| p.gpio)?;
        let (port, bit) = (gpio_port(pin), (pin % 16) as usize);
        // Level first, so the pin does not glitch when it becomes an output.
        port.bsrr().write(|w| if high { w.set_bs(bit, true) } else { w.set_br(bit, true) });
        port.otyper().modify(|w| w.set_ot(bit, Ot::PUSH_PULL));
        Self::set_mode(pin, Moder::OUTPUT); // also takes a pin back from PWM
        Ok(())
    }

    fn gpio_read(&mut self, pin: u8, pull: CorePull) -> Result<bool, HwError> {
        use pac::gpio::vals::{Moder, Pupdr};
        self.check_pin(pin, |p| p.gpio)?;
        let (port, bit) = (gpio_port(pin), (pin % 16) as usize);
        let pupdr = match pull {
            CorePull::None => Pupdr::FLOATING,
            CorePull::Up => Pupdr::PULL_UP,
            CorePull::Down => Pupdr::PULL_DOWN,
        };
        port.pupdr().modify(|w| w.set_pupdr(bit, pupdr));
        Self::set_mode(pin, Moder::INPUT);
        cortex_m::asm::delay(CPU_MHZ * 10); // ~10 us for the pull to charge the pin
        Ok(port.idr().read().idr(bit) == pac::gpio::vals::Idr::HIGH)
    }

    fn pwm_set(&mut self, pin: u8, freq_hz: u32, duty: u16) -> Result<u32, HwError> {
        use pac::gpio::vals::Moder;
        use pac::timer::vals::Ocm;
        self.check_pin(pin, |p| p.pwm)?;
        let ch = PWM_PINS.iter().find(|&&(p, _)| p == pin).map(|&(_, ch)| ch).ok_or(HwError::WrongPin)?;
        if !(1..=1_000_000).contains(&freq_hz) || duty > 1000 {
            return Err(HwError::OutOfRange);
        }
        pac::RCC.apb1enr1().modify(|w| w.set_tim2en(true));
        let tim = pac::TIM2;
        // TIM2 is 32 bits wide, so no prescaler is needed even at 1 Hz.
        let period = TIM2_HZ / freq_hz;
        tim.psc().write_value(0);
        tim.arr().write_value(period - 1);
        tim.ccr(ch).write_value((period as u64 * duty as u64 / 1000) as u32);
        tim.ccmr_output(ch / 2).modify(|w| {
            w.set_ocm(ch % 2, Ocm::PWM_MODE1);
            w.set_ocpe(ch % 2, true);
        });
        tim.ccer().modify(|w| w.set_cce(ch, true));
        tim.cr1().modify(|w| {
            w.set_arpe(true);
            w.set_cen(true);
        });
        tim.egr().write(|w| w.set_ug(true)); // load ARR and CCR now, not at the next overflow

        let (port, bit) = (gpio_port(pin), (pin % 16) as usize);
        port.afr(bit / 8).modify(|w| w.set_afr(bit % 8, 1)); // AF1 = TIM2
        Self::set_mode(pin, Moder::ALTERNATE);
        Ok(TIM2_HZ / period)
    }

    async fn adc_read(&mut self, pin: u8) -> Result<u32, HwError> {
        self.check_pin(pin, |p| p.adc)?;
        let index = ADC_PINS.iter().position(|&p| p == pin).ok_or(HwError::WrongPin)?;
        // `gpio` may have taken the pin; give it back to the ADC.
        Self::set_mode(pin, pac::gpio::vals::Moder::ANALOG);
        let mut bank = self.adc.lock().await;
        let vdda_mv = bank.vdda_mv();
        let bank = &mut *bank;
        let raw = bank.adc.blocking_read(&mut bank.pins[index], SampleTime::CYCLES247_5) as f32;
        Ok((raw * vdda_mv / 4095.0) as u32)
    }

    fn settings_load(&mut self, buf: &mut [u8]) -> Result<(), HwError> {
        let n = buf.len().min(SETTINGS_PAGE as usize);
        self.flash.blocking_read(SETTINGS_OFFSET, &mut buf[..n]).map_err(|_| HwError::Bus)
    }

    fn settings_store(&mut self, data: &[u8]) -> Result<(), HwError> {
        // Never erase a page the program lives in, should the firmware ever grow that far.
        if image_end() > FLASH_BASE as u32 + SETTINGS_OFFSET || data.len() > SETTINGS_PAGE as usize {
            return Err(HwError::OutOfRange);
        }
        let end = SETTINGS_OFFSET + SETTINGS_PAGE;
        self.flash.blocking_erase(SETTINGS_OFFSET, end).map_err(|_| HwError::Bus)?;
        // The flash is programmed 8 bytes at a time; the tail is padded with the erased value.
        for (i, chunk) in data.chunks(WRITE_SIZE).enumerate() {
            let mut word = [0xFFu8; WRITE_SIZE];
            word[..chunk.len()].copy_from_slice(chunk);
            let offset = SETTINGS_OFFSET + (i * WRITE_SIZE) as u32;
            self.flash.blocking_write(offset, &word).map_err(|_| HwError::Bus)?;
        }
        Ok(())
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

    // Blocking on purpose. DMA I2C was tried on embassy-stm32 0.6 and fails two ways: the
    // DMA read never notices a NACK (it waits out the timeout, so every empty address in a
    // scan reads as a bus error), and with the I2C interrupts bound the blocking calls report
    // each result one transfer late (the OLED shows up at 0x3d). A transfer takes at most
    // ~25 ms (a whole OLED frame at 400 kHz), and console input waits safely in its DMA ring.
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
async fn sensor_task(adc: &'static SharedAdc) {
    yanarkaze_core::sensor::run(DieSensor(adc)).await
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
    // Blocking: see the note above `i2c_write` for why not DMA.
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
    static ADC: StaticCell<SharedAdc> = StaticCell::new();
    let adc_pins = [p.PA0.degrade_adc(), p.PA1.degrade_adc(), p.PC1.degrade_adc(), p.PC0.degrade_adc()];
    let adc = &*ADC.init(Mutex::new(AdcBank::new(p.ADC1, adc_pins).await));
    let flash = Flash::new_blocking(p.FLASH);
    let sys = NucleoSystem { rng: Rng::new(p.RNG, Irqs), reset_reason, i2c, sda, adc, flash };

    spawner.spawn(shell_task(sys, uart_rx, ConsoleTx(uart_tx), spawner).unwrap());
    spawner.spawn(led_task(led).unwrap());
    spawner.spawn(button_task(button).unwrap());
    spawner.spawn(sensor_task(adc).unwrap());
    spawner.spawn(watchdog_task(wdg).unwrap());

    info!("all tasks spawned");
}
