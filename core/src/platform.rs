//! The hardware abstraction: the only things a board has to provide.

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

    /// Blocks forever without yielding to the executor, so the watchdog fires.
    fn hang(&mut self) -> ! {
        loop {
            core::hint::spin_loop();
        }
    }
}
