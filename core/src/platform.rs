//! The hardware abstraction: the only things a board has to provide.

use core::fmt;

use heapless::String;

/// A single on/off indicator LED.
pub trait Led {
    fn set(&mut self, on: bool);
}

/// A push button. Implementations should resolve `wait_for_press` as soon as the
/// button goes down (debouncing is handled by the core).
pub trait Button {
    async fn wait_for_press(&mut self);
    async fn wait_for_release(&mut self);
}

/// One sample from the on-chip sensors.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Reading {
    pub temp_c: f32,
    /// Supply voltage, if the chip can measure it.
    pub vdda_mv: Option<u32>,
}

/// A temperature sensor (usually the one inside the MCU).
pub trait Sensor {
    async fn read(&mut self) -> Reading;
}

/// A watchdog that resets the system if it is not fed in time.
pub trait Watchdog {
    fn start(&mut self);
    fn feed(&mut self);
}

/// A readable memory region, used by the `dump` command to avoid faulting addresses.
#[derive(Clone, Copy, Debug)]
pub struct MemRegion {
    pub start: usize,
    pub end: usize,
    pub name: &'static str,
}

/// Why a pin or bus operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HwError {
    /// The board does not offer this at all.
    Unsupported,
    /// No such pin on this board, or not one the shell may touch.
    NoSuchPin,
    /// The pin exists but is not able to do that (e.g. ADC on a digital-only pin).
    WrongPin,
    /// A value outside what the hardware can do (frequency, duty, ...).
    OutOfRange,
    /// No I2C device answered at that address.
    Nack,
    /// The bus did not complete (lost arbitration: noise, missing pull-ups, a short).
    Bus,
    /// The transfer never finished, usually because a device holds SDA or SCL low.
    Timeout,
}

impl fmt::Display for HwError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            HwError::Unsupported => "this board does not support that",
            HwError::NoSuchPin => "no such pin (see 'pins')",
            HwError::WrongPin => "that pin cannot do this (see 'pins')",
            HwError::OutOfRange => "value out of range",
            HwError::Nack => "no answer from the device",
            HwError::Bus => "bus error (check wiring and pull-ups)",
            HwError::Timeout => "bus timed out (a device may be holding a line low; try 'i2c reset')",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pull {
    None,
    Up,
    Down,
}

/// How a board names its pins; the shell parses and prints pin numbers this way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinStyle {
    /// `GP16`: the number is the GPIO number (Raspberry Pi Pico).
    Gp,
    /// `PA5`: the number is port * 16 + bit, so PA5 is 5 and PB3 is 19 (STM32).
    Port,
}

/// What a header pin can do, for the `pins` command.
#[derive(Clone, Copy, Debug)]
pub struct PinInfo {
    pub pin: u8,
    pub gpio: bool,
    pub pwm: bool,
    pub adc: bool,
    /// Board-specific remark, e.g. "PWM 3A" or "pin 21".
    pub note: &'static str,
}

/// Static facts about the board, shown by `info` and the boot banner.
pub struct BoardInfo {
    pub board: &'static str,
    pub chip: &'static str,
    pub cpu: &'static str,
    pub cpu_mhz: Option<u32>,
    pub fpu: bool,
    pub flash_kib: Option<u32>,
    pub ram_kib: Option<u32>,
    /// What the user presses for the "button", e.g. "the blue USER button".
    pub button: &'static str,
    /// Extra lines printed under the boot banner.
    pub hints: &'static [&'static str],
}

/// Everything else the shell needs from the platform.
pub trait System {
    fn info(&self) -> &BoardInfo;

    /// Why the system last reset, e.g. "power on" or "watchdog".
    fn reset_reason(&self) -> &'static str;

    /// A random number, from a hardware RNG where available.
    fn random_u32(&mut self) -> u32;

    /// A unique ID of the chip, as hex.
    fn unique_id(&self) -> Option<String<32>> {
        None
    }

    /// A free-running CPU cycle counter, if the core has one.
    fn cycle_counter(&self) -> Option<u32> {
        None
    }

    /// Memory regions that are safe to read with `dump`.
    fn memory_regions(&self) -> &[MemRegion] {
        &[]
    }

    /// Reads one byte of memory.
    ///
    /// # Safety
    /// `addr` must lie inside one of [`System::memory_regions`].
    unsafe fn read_byte(&self, addr: usize) -> u8 {
        unsafe { core::ptr::read_volatile(addr as *const u8) }
    }

    /// Restarts the system.
    fn reset(&mut self) -> !;

    /// Restarts into the chip's bootloader so new firmware can be flashed without pressing
    /// any buttons. Returns only if the board cannot do that.
    fn enter_bootloader(&mut self) {}

    // -- Pins and buses for the hardware commands. All optional: a board that does not
    // -- implement them simply reports `HwError::Unsupported`.

    /// The header pins the shell may use.
    fn pins(&self) -> &[PinInfo] {
        &[]
    }

    /// How this board names its pins.
    fn pin_style(&self) -> PinStyle {
        PinStyle::Gp
    }

    /// Other names users may type for pins, such as Arduino header labels: `("A0", 0)`.
    fn pin_aliases(&self) -> &[(&'static str, u8)] {
        &[]
    }

    /// Pins the board keeps for itself, with what they do (shown by `pins`).
    fn reserved_pins(&self) -> &[(&'static str, &'static str)] {
        &[]
    }

    /// Drives a pin as a push-pull output.
    fn gpio_write(&mut self, _pin: u8, _high: bool) -> Result<(), HwError> {
        Err(HwError::Unsupported)
    }

    /// Turns a pin into an input with the given pull and reads it.
    fn gpio_read(&mut self, _pin: u8, _pull: Pull) -> Result<bool, HwError> {
        Err(HwError::Unsupported)
    }

    /// Starts PWM on a pin, `duty` in per mille (0..=1000). Returns the frequency actually set.
    fn pwm_set(&mut self, _pin: u8, _freq_hz: u32, _duty: u16) -> Result<u32, HwError> {
        Err(HwError::Unsupported)
    }

    /// Reads an analog pin, in millivolts.
    async fn adc_read(&mut self, _pin: u8) -> Result<u32, HwError> {
        Err(HwError::Unsupported)
    }

    /// Where the I2C bus is, e.g. "SDA GP4 (pin 6), SCL GP5 (pin 7)".
    fn i2c_pins(&self) -> Option<&'static str> {
        None
    }

    /// Changes the I2C clock; returns the frequency set.
    fn i2c_set_speed(&mut self, _hz: u32) -> Result<u32, HwError> {
        Err(HwError::Unsupported)
    }

    /// Frees a stuck bus (clocks SCL until a device lets go of SDA, then sends a STOP) and
    /// resets the controller. Returns whether (SDA, SCL) are high (free) afterwards.
    fn i2c_recover(&mut self) -> Result<(bool, bool), HwError> {
        Err(HwError::Unsupported)
    }

    async fn i2c_write(&mut self, _addr: u8, _bytes: &[u8]) -> Result<(), HwError> {
        Err(HwError::Unsupported)
    }

    async fn i2c_read(&mut self, _addr: u8, _buf: &mut [u8]) -> Result<(), HwError> {
        Err(HwError::Unsupported)
    }

    /// Writes `bytes` (usually a register number), then reads `buf` in one transaction.
    async fn i2c_write_read(&mut self, _addr: u8, _bytes: &[u8], _buf: &mut [u8]) -> Result<(), HwError> {
        Err(HwError::Unsupported)
    }

    /// Blocks forever without yielding to the executor, so the watchdog fires.
    fn hang(&mut self) -> ! {
        loop {
            core::hint::spin_loop();
        }
    }
}
