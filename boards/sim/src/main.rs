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
use yanarkaze_core::{BoardInfo, Button, Led, MemRegion, Reading, Sensor, System, Watchdog};

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

    async fn wait_for_release(&mut self) {}
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
