//! yanarkaze desktop simulator: the exact same core, running in your terminal.
//!
//! Hardware is simulated with plain std:
//!   console  -> raw-mode terminal (a reader thread feeds an async pipe)
//!   button   -> Ctrl+B
//!   sensor   -> a simulated die temperature; Ctrl+T puts your "finger" on the chip
//!   LED      -> shown in the terminal title bar
//!   watchdog -> a monitor thread that restarts the process if the executor stalls

use std::io::{Read as _, Write as _};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, process, thread};

use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::pipe::Pipe;
use embassy_sync::signal::Signal;
use heapless::String;
use yanarkaze_core::{BoardInfo, Button, HwError, Led, MemRegion, PinInfo, Pull, Reading, Sensor, System, Watchdog};

const RESET_REASON_ENV: &str = "YANARKAZE_RESET_REASON";
const WATCHDOG_TIMEOUT_MS: u64 = 2000;

static STDIN_PIPE: Pipe<CriticalSectionRawMutex, 1024> = Pipe::new();
static BUTTON: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static FINGER_ON_CHIP: AtomicBool = AtomicBool::new(false);
static LAST_FEED_MS: AtomicU64 = AtomicU64::new(0);
static STARTED: OnceLock<Instant> = OnceLock::new();

/// Pretend ROM, so `dump` has something to show.
static SIM_ROM: &[u8] = b"yanarkaze desktop simulator ROM -- on real boards `dump` shows flash, RAM and factory data. \
burning since the Caspian winds started blowing.";

fn now_ms() -> u64 {
    STARTED.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    print!("\x1b[0m\x1b[?25h\x1b]0;\x07");
    let _ = std::io::stdout().flush();
}

/// Re-executes this program, telling the new instance why it restarted.
fn restart(reason: &str) -> ! {
    restore_terminal();
    let exe = env::current_exe().expect("cannot find the simulator executable");
    let mut cmd = process::Command::new(exe);
    cmd.args(env::args_os().skip(1)).env(RESET_REASON_ENV, reason);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("restart failed: {err}");
        process::exit(1);
    }
    #[cfg(not(unix))]
    {
        let code = cmd.status().ok().and_then(|s| s.code()).unwrap_or(1);
        process::exit(code);
    }
}

fn quit() -> ! {
    restore_terminal();
    println!("\r\nbye! the fire goes out... for now.\r");
    process::exit(0);
}

/// Reads the keyboard on a normal thread and forwards bytes into the async world.
fn spawn_stdin_reader() {
    thread::spawn(|| {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 64];
        loop {
            let n = match stdin.read(&mut buf) {
                Ok(0) | Err(_) => {
                    // stdin closed (e.g. piped input ended): give the shell a moment to finish.
                    thread::sleep(Duration::from_millis(500));
                    quit();
                }
                Ok(n) => n,
            };
            for &b in &buf[..n] {
                match b {
                    0x02 => BUTTON.signal(()), // Ctrl+B
                    0x14 => {
                        // Ctrl+T
                        FINGER_ON_CHIP.fetch_xor(true, Ordering::Relaxed);
                    }
                    0x04 => quit(), // Ctrl+D
                    _ => {
                        while STDIN_PIPE.try_write(&[b]).is_err() {
                            thread::sleep(Duration::from_millis(1));
                        }
                    }
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Console
// ---------------------------------------------------------------------------

struct SimRx;

impl embedded_io_async::ErrorType for SimRx {
    type Error = core::convert::Infallible;
}

impl embedded_io_async::Read for SimRx {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        Ok(STDIN_PIPE.read(buf).await)
    }
}

struct SimTx;

impl embedded_io_async::ErrorType for SimTx {
    type Error = core::convert::Infallible;
}

impl embedded_io_async::Write for SimTx {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let _ = std::io::stdout().write_all(buf);
        Ok(buf.len())
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        let _ = std::io::stdout().flush();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Simulated hardware
// ---------------------------------------------------------------------------

struct TitleBarLed;

impl Led for TitleBarLed {
    fn set(&mut self, on: bool) {
        let dot = if on { "\u{25cf}" } else { "\u{25cb}" };
        print!("\x1b]0;yanarkaze  {dot} LED\x07");
        let _ = std::io::stdout().flush();
    }
}

struct KeyboardButton;

impl Button for KeyboardButton {
    async fn wait_for_press(&mut self) {
        BUTTON.wait().await;
    }

    /// A key press has no "up" event, so it acts like a button held for 60 ms (long enough
    /// for the core's debouncing to accept it).
    async fn wait_for_release(&mut self) {
        embassy_time::Timer::after_millis(60).await;
    }
}

/// xorshift64*: plenty for dice and the matrix rain.
struct XorShift(u64);

impl XorShift {
    fn from_time() -> Self {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        Self((nanos ^ ((process::id() as u64) << 32)) | 1)
    }

    fn next_u32(&mut self) -> u32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }
}

/// A die that drifts slowly with the "room" and heats up under your finger (Ctrl+T).
struct SimSensor {
    temp_c: f32,
    rng: XorShift,
}

impl Sensor for SimSensor {
    async fn read(&mut self) -> Reading {
        let t = now_ms() as f32 / 1000.0;
        let room = 26.0 + 1.2 * (t / 30.0).sin();
        let target = if FINGER_ON_CHIP.load(Ordering::Relaxed) { room + 9.0 } else { room };
        self.temp_c += (target - self.temp_c) * 0.06; // thermal inertia
        let noise = (self.rng.next_u32() % 100) as f32 / 1000.0 - 0.05;
        Reading { temp_c: self.temp_c + noise, vdda_mv: Some(3300) }
    }
}

struct ThreadWatchdog;

impl Watchdog for ThreadWatchdog {
    fn start(&mut self) {
        LAST_FEED_MS.store(now_ms(), Ordering::Relaxed);
        thread::spawn(|| {
            loop {
                thread::sleep(Duration::from_millis(100));
                if now_ms() - LAST_FEED_MS.load(Ordering::Relaxed) > WATCHDOG_TIMEOUT_MS {
                    print!("\r\n\x1b[1;31m[sim] watchdog timeout, restarting\x1b[0m\r\n");
                    restart("watchdog (simulated)");
                }
            }
        });
    }

    fn feed(&mut self) {
        LAST_FEED_MS.store(now_ms(), Ordering::Relaxed);
    }
}

struct SimSystem {
    info: BoardInfo,
    reset_reason: &'static str,
    rng: XorShift,
    regions: [MemRegion; 1],
    /// Output levels of the simulated pins (bit n = GPn).
    levels: u32,
    /// The last bytes written to each fake I2C device: its pending command.
    i2c_cmd: [Vec<u8>; 128],
    /// The last PWM output set (frequency, duty in 0.1 %), as if wired to every analog pin
    /// for `scope`.
    pwm: Option<(u32, u16)>,
    /// The log file, read on first use.
    log: Option<Vec<u8>>,
}

/// Pretend header: GP0-15 for gpio / pwm, GP26-28 analog.
static SIM_PINS: [PinInfo; 19] = {
    let mut pins = [PinInfo { pin: 0, gpio: true, pwm: true, adc: false, note: "simulated" }; 19];
    let mut i = 0;
    while i < 19 {
        if i < 16 {
            pins[i].pin = i as u8;
        } else {
            pins[i] = PinInfo { pin: 10 + i as u8, gpio: false, pwm: false, adc: true, note: "simulated knob" };
        }
        i += 1;
    }
    pins
};

/// Fake I2C devices: an OLED, and one of each room sensor `env` knows, answering like the
/// real chips so the drivers run end to end.
const SIM_I2C: [u8; 4] = [0x3C, 0x76, 0x44, 0x38];

/// Where the simulator keeps `save`d settings: $YANARKAZE_SETTINGS, or ~/.yanarkaze-settings.
fn sim_settings_path() -> std::path::PathBuf {
    match env::var_os("YANARKAZE_SETTINGS") {
        Some(path) => path.into(),
        None => env::home_dir().unwrap_or_default().join(".yanarkaze-settings"),
    }
}

/// The temperature log's "flash": a file of `SIM_LOG_PAGES` pages, erased to 0xFF.
const SIM_LOG_PAGES: usize = 16;
const SIM_LOG_PAGE: usize = 2048;

fn sim_log_path() -> std::path::PathBuf {
    match env::var_os("YANARKAZE_LOG") {
        Some(path) => path.into(),
        None => env::home_dir().unwrap_or_default().join(".yanarkaze-log"),
    }
}

/// The log file's bytes, created erased if it is missing or the wrong size.
fn sim_log_bytes() -> Vec<u8> {
    match std::fs::read(sim_log_path()) {
        Ok(b) if b.len() == SIM_LOG_PAGES * SIM_LOG_PAGE => b,
        _ => vec![0xFF; SIM_LOG_PAGES * SIM_LOG_PAGE],
    }
}

/// Slowly drifting fake room conditions: about 22-24 degC and 40-50 %RH.
fn sim_room() -> (f32, f32) {
    let t = STARTED.get_or_init(Instant::now).elapsed().as_secs_f32();
    (23.0 + (t / 20.0).sin(), 45.0 + 5.0 * (t / 33.0).cos())
}

/// A BME280's registers. The calibration is the BMP280 datasheet's example plus typical
/// humidity values; the raw readings are worked backwards from `sim_room` roughly (the
/// fake only needs to move plausibly, not be exact).
fn sim_bme280_regs() -> [u8; 256] {
    let mut r = [0u8; 256];
    r[0xD0] = 0x60;
    let tp: [i32; 12] = [27504, 26435, -1000, 36477, -10685, 3024, 2855, 140, -7, 15500, -14600, 6000];
    for (i, v) in tp.iter().enumerate() {
        r[0x88 + 2 * i..0x8A + 2 * i].copy_from_slice(&(*v as i16).to_le_bytes());
    }
    r[0xA1] = 75; // H1
    r[0xE1..0xE8].copy_from_slice(&[0x72, 0x01, 0x00, 0x13, 0x29, 0x03, 0x1E]); // H2..H6
    let (temp, rh) = sim_room();
    // Near the datasheet's example point, adc_T moves about 5150 counts per degree.
    let adc_t = (519888.0 + (temp - 25.08) * 5150.0) as u32;
    let adc_p = 415148u32;
    let adc_h = (14000.0 + rh * 300.0) as u32;
    r[0xF7..0xFA].copy_from_slice(&[(adc_p >> 12) as u8, (adc_p >> 4) as u8, (adc_p << 4) as u8]);
    r[0xFA..0xFD].copy_from_slice(&[(adc_t >> 12) as u8, (adc_t >> 4) as u8, (adc_t << 4) as u8]);
    r[0xFD..0xFF].copy_from_slice(&(adc_h as u16).to_be_bytes());
    r
}

fn sim_pin(pin: u8, what: fn(&PinInfo) -> bool) -> Result<(), HwError> {
    match SIM_PINS.iter().find(|p| p.pin == pin) {
        Some(p) if what(p) => Ok(()),
        Some(_) => Err(HwError::WrongPin),
        None => Err(HwError::NoSuchPin),
    }
}

impl SimSystem {
    fn new() -> Self {
        let host: &'static str = Box::leak(format!("{} {}", env::consts::ARCH, env::consts::OS).into_boxed_str());
        let reset_reason: &'static str =
            Box::leak(env::var(RESET_REASON_ENV).unwrap_or_else(|_| "power on".into()).into_boxed_str());
        let rom = SIM_ROM.as_ptr() as usize;
        Self {
            info: BoardInfo {
                board: "desktop simulator",
                chip: host,
                cpu: "host CPU",
                cpu_mhz: None,
                fpu: true,
                flash_kib: None,
                ram_kib: None,
                button: "Ctrl+B",
                hints: &[
                    "Ctrl+B press the button   Ctrl+T touch the chip (heats it up)   Ctrl+D quit",
                    "the LED blinks in your terminal's title bar",
                ],
            },
            reset_reason,
            rng: XorShift::from_time(),
            regions: [MemRegion { start: rom, end: rom + SIM_ROM.len(), name: "simulated ROM" }],
            levels: 0,
            i2c_cmd: std::array::from_fn(|_| Vec::new()),
            pwm: None,
            log: None,
        }
    }
}

impl System for SimSystem {
    fn info(&self) -> &BoardInfo {
        &self.info
    }

    fn reset_reason(&self) -> &'static str {
        self.reset_reason
    }

    fn random_u32(&mut self) -> u32 {
        self.rng.next_u32()
    }

    fn unique_id(&self) -> Option<String<32>> {
        let mut id = String::new();
        let _ = core::fmt::Write::write_fmt(&mut id, format_args!("sim-{:08x}", process::id()));
        Some(id)
    }

    fn memory_regions(&self) -> &[MemRegion] {
        &self.regions
    }

    fn reset(&mut self) -> ! {
        restart("software reset");
    }

    fn pins(&self) -> &[PinInfo] {
        &SIM_PINS
    }

    fn gpio_write(&mut self, pin: u8, high: bool) -> Result<(), HwError> {
        sim_pin(pin, |p| p.gpio)?;
        if high {
            self.levels |= 1 << pin;
        } else {
            self.levels &= !(1 << pin);
        }
        Ok(())
    }

    /// Inputs read back what was last written, or the pull when nothing was.
    fn gpio_read(&mut self, pin: u8, pull: Pull) -> Result<bool, HwError> {
        sim_pin(pin, |p| p.gpio)?;
        Ok(match pull {
            Pull::Up => true,
            Pull::Down => false,
            Pull::None => self.levels & (1 << pin) != 0,
        })
    }

    fn pwm_set(&mut self, pin: u8, freq_hz: u32, duty: u16) -> Result<u32, HwError> {
        sim_pin(pin, |p| p.pwm)?;
        if !(10..=1_000_000).contains(&freq_hz) || duty > 1000 {
            return Err(HwError::OutOfRange);
        }
        self.pwm = (duty > 0).then_some((freq_hz, duty));
        Ok(freq_hz)
    }

    /// With a PWM output running, every analog pin sees it (as if wired with a jumper), with
    /// a little noise and an RC-rounded edge; otherwise the knob.
    async fn adc_capture(&mut self, pin: u8, buf: &mut [u16], interval_us: u32) -> Result<u32, HwError> {
        sim_pin(pin, |p| p.adc)?;
        let Some((freq, duty)) = self.pwm else {
            let mv = self.adc_read(pin).await? as u16;
            buf.fill(mv);
            return Ok(interval_us * 1000);
        };
        let period_ns = 1e9 / freq as f64;
        let t0 = (now_ms() * 1_000_000) as f64;
        let mut level = 0.0f64;
        for (i, v) in buf.iter_mut().enumerate() {
            let t = t0 + i as f64 * interval_us as f64 * 1000.0;
            let high = (t % period_ns) / period_ns < duty as f64 / 1000.0;
            let target = if high { 3300.0 } else { 0.0 };
            level += (target - level) * 0.8;
            let noise = (self.rng.next_u32() % 30) as f64 - 15.0;
            *v = (level + noise).clamp(0.0, 3300.0) as u16;
        }
        Ok(interval_us * 1000)
    }

    /// A knob that someone keeps turning slowly back and forth (a 6 s triangle wave).
    async fn adc_read(&mut self, pin: u8) -> Result<u32, HwError> {
        sim_pin(pin, |p| p.adc)?;
        let phase = (embassy_time::Instant::now().as_millis() + pin as u64 * 1000) % 6000;
        let up = if phase < 3000 { phase } else { 6000 - phase };
        Ok((up * 3300 / 3000) as u32)
    }

    fn i2c_pins(&self) -> Option<&'static str> {
        Some("simulated bus: an OLED, a BME280, an SHT31 and an AHT20")
    }

    fn i2c_set_speed(&mut self, hz: u32) -> Result<u32, HwError> {
        Ok(hz)
    }

    fn i2c_recover(&mut self) -> Result<(bool, bool), HwError> {
        Ok((true, true))
    }

    fn settings_load(&mut self, buf: &mut [u8]) -> Result<(), HwError> {
        buf.fill(0xFF); // like erased flash
        if let Ok(data) = std::fs::read(sim_settings_path()) {
            let n = data.len().min(buf.len());
            buf[..n].copy_from_slice(&data[..n]);
        }
        Ok(())
    }

    fn settings_store(&mut self, data: &[u8]) -> Result<(), HwError> {
        let path = sim_settings_path();
        let result = if data.is_empty() { std::fs::remove_file(&path).or(Ok(())) } else { std::fs::write(&path, data) };
        result.map_err(|_| HwError::Bus)
    }

    fn log_geometry(&self) -> Option<(usize, usize)> {
        Some((SIM_LOG_PAGES, SIM_LOG_PAGE))
    }

    fn log_read(&mut self, offset: usize, buf: &mut [u8; 8]) -> Result<(), HwError> {
        let bytes = self.log.get_or_insert_with(sim_log_bytes);
        buf.copy_from_slice(bytes.get(offset..offset + 8).ok_or(HwError::OutOfRange)?);
        Ok(())
    }

    /// Like flash, a write can only clear bits; the file is rewritten each time.
    fn log_write(&mut self, offset: usize, record: &[u8; 8]) -> Result<(), HwError> {
        let bytes = self.log.get_or_insert_with(sim_log_bytes);
        let word = bytes.get_mut(offset..offset + 8).ok_or(HwError::OutOfRange)?;
        for (w, r) in word.iter_mut().zip(record) {
            *w &= r;
        }
        std::fs::write(sim_log_path(), &*bytes).map_err(|_| HwError::Bus)
    }

    fn log_erase(&mut self, page: usize) -> Result<(), HwError> {
        let bytes = self.log.get_or_insert_with(sim_log_bytes);
        bytes.get_mut(page * SIM_LOG_PAGE..(page + 1) * SIM_LOG_PAGE).ok_or(HwError::OutOfRange)?.fill(0xFF);
        std::fs::write(sim_log_path(), &*bytes).map_err(|_| HwError::Bus)
    }

    async fn i2c_write(&mut self, addr: u8, bytes: &[u8]) -> Result<(), HwError> {
        if !SIM_I2C.contains(&addr) {
            return Err(HwError::Nack);
        }
        self.i2c_cmd[addr as usize & 0x7F] = bytes.to_vec();
        Ok(())
    }

    async fn i2c_read(&mut self, addr: u8, buf: &mut [u8]) -> Result<(), HwError> {
        use yanarkaze_core::env::crc8;
        if !SIM_I2C.contains(&addr) {
            return Err(HwError::Nack);
        }
        buf.fill(0);
        let cmd = self.i2c_cmd[addr as usize & 0x7F].as_slice();
        let (temp, rh) = sim_room();
        let mut data: Vec<u8> = match (addr, cmd) {
            // SHT3x: status register, or a measurement.
            (0x44, [0xF3, 0x2D]) => vec![0x80, 0x10],
            (0x44, [0x24, 0x00]) => {
                let st = ((temp + 45.0) / 175.0 * 65535.0) as u16;
                let srh = (rh / 100.0 * 65535.0) as u16;
                [st.to_be_bytes(), srh.to_be_bytes()].concat()
            }
            (0x44, _) => return Err(HwError::Nack), // nothing to read without a command
            // AHT20: calibrated and idle, plus the measurement after 0xAC 0x33 0x00.
            (0x38, [0xAC, 0x33, 0x00]) => {
                let srh = (rh / 100.0 * 1048576.0) as u32;
                let st = ((temp + 50.0) / 200.0 * 1048576.0) as u32;
                vec![
                    0x18,
                    (srh >> 12) as u8,
                    (srh >> 4) as u8,
                    ((srh << 4) as u8 & 0xF0) | ((st >> 16) as u8 & 0x0F),
                    (st >> 8) as u8,
                    st as u8,
                ]
            }
            (0x38, _) => vec![0x18],
            _ => vec![],
        };
        // Sensirion puts a CRC after every word, Aosong one after the whole frame.
        if addr == 0x44 {
            data = data.chunks(2).flat_map(|w| [w[0], w[1], crc8(w)]).collect();
        } else if addr == 0x38 && data.len() == 6 {
            data.push(crc8(&data));
        }
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(())
    }

    async fn i2c_write_read(&mut self, addr: u8, bytes: &[u8], buf: &mut [u8]) -> Result<(), HwError> {
        if !SIM_I2C.contains(&addr) {
            return Err(HwError::Nack);
        }
        buf.fill(0);
        if let (0x76, [reg]) = (addr, bytes) {
            let regs = sim_bme280_regs();
            let start = *reg as usize;
            let n = buf.len().min(256 - start);
            buf[..n].copy_from_slice(&regs[start..start + n]);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

#[embassy_executor::task]
async fn shell_task(sys: SimSystem, spawner: Spawner) {
    yanarkaze_core::shell::run(sys, SimRx, SimTx, spawner).await
}

#[embassy_executor::task]
async fn led_task() {
    yanarkaze_core::led::run(TitleBarLed).await
}

#[embassy_executor::task]
async fn button_task() {
    yanarkaze_core::button::run(KeyboardButton).await
}

#[embassy_executor::task]
async fn sensor_task() {
    yanarkaze_core::sensor::run(SimSensor { temp_c: 26.0, rng: XorShift::from_time() }).await
}

#[embassy_executor::task]
async fn watchdog_task() {
    yanarkaze_core::watchdog::run(ThreadWatchdog).await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    now_ms(); // start the clock
    // Raw mode gives us every key press (arrows, Ctrl+B, ...). It fails when stdin is not a
    // terminal, e.g. with piped input, which is fine for scripted runs.
    let _ = crossterm::terminal::enable_raw_mode();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
    spawn_stdin_reader();

    spawner.spawn(shell_task(SimSystem::new(), spawner).unwrap());
    spawner.spawn(led_task().unwrap());
    spawner.spawn(button_task().unwrap());
    spawner.spawn(sensor_task().unwrap());
    spawner.spawn(watchdog_task().unwrap());
}
