//! Hardware playground commands: pins, GPIO, PWM, ADC and I2C.
//!
//! Everything goes through the optional pin and bus methods of [`System`], so a board only
//! implements what it has and the rest reports "not supported".

use core::fmt::Write as _;

use embedded_io_async::{Read, Write};
use heapless::String;

use crate::oled;
use crate::platform::{HwError, Pull, System};
use crate::shell::Shell;
use crate::state::{BUTTON_PRESSES, HISTORY_SECS, SAMPLE, TEMP_HISTORY, current_led_mode};
use portable_atomic::Ordering;

/// Accepts `16`, `gp16` and `GP16`.
pub(crate) fn parse_pin(s: &str) -> Option<u8> {
    let digits = s.strip_prefix("gp").or_else(|| s.strip_prefix("GP")).unwrap_or(s);
    digits.parse().ok()
}

/// Accepts `0x3c`, `3c` and `60`. I2C addresses are conventionally written in hex, so a bare
/// number with hex letters is hex too; a bare decimal number stays decimal.
pub(crate) fn parse_addr(s: &str) -> Option<u8> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u8::from_str_radix(hex, 16).ok();
    }
    s.parse().ok().or_else(|| u8::from_str_radix(s, 16).ok())
}

/// Common I2C parts, for the `i2c scan` output.
pub(crate) fn i2c_device_name(addr: u8) -> &'static str {
    match addr {
        0x3C | 0x3D => "SSD1306 / SH1106 OLED display",
        0x76 | 0x77 => "BME280 / BMP280 / BMP180 pressure sensor",
        0x68 | 0x69 => "MPU6050 IMU or DS3231 clock",
        0x27 | 0x3F => "PCF8574 LCD backpack",
        0x23 => "BH1750 light sensor or I/O expander",
        0x20..=0x26 => "PCF8574 / MCP23017 I/O expander",
        0x48..=0x4B => "ADS1115 ADC or TMP102 thermometer",
        0x40 => "INA219 current sensor or PCA9685 PWM driver",
        0x44 | 0x45 => "SHT3x humidity sensor",
        0x38 => "AHT20 humidity sensor",
        0x29 => "VL53L0X distance sensor",
        0x50..=0x57 => "AT24C EEPROM",
        _ => "",
    }
}

/// Perceived brightness is roughly quadratic, so a linear fade looks wrong without this.
pub(crate) fn gamma(percent: u32) -> u16 {
    let p = percent.min(100);
    (p * p / 10) as u16
}

fn bar(fraction: f32, width: usize) -> String<64> {
    let filled = ((fraction.clamp(0.0, 1.0) * width as f32) as usize).min(width);
    let mut s = String::new();
    for i in 0..width.min(60) {
        let _ = s.push(if i < filled { '#' } else { '.' });
    }
    s
}

impl<S: System, R: Read, W: Write> Shell<S, R, W> {
    async fn hw_error(&mut self, e: HwError) {
        out!(self.term, "error: {e}\r\n");
    }

    async fn pin_arg(&mut self, arg: Option<&str>, usage: &str) -> Option<u8> {
        match arg.and_then(parse_pin) {
            Some(pin) => Some(pin),
            None => {
                out!(self.term, "usage: {usage}\r\n");
                None
            }
        }
    }

    pub(crate) async fn cmd_pins(&mut self) {
        let pins = self.sys.pins();
        if pins.is_empty() {
            self.term.print("this board has no pins for the shell (yet)\r\n").await;
            return;
        }
        // Copy out first: printing needs `self` mutably.
        let mut rows: heapless::Vec<(u8, bool, bool, bool, &'static str), 48> = heapless::Vec::new();
        for p in pins {
            let _ = rows.push((p.pin, p.gpio, p.pwm, p.adc, p.note));
        }
        out!(self.term, "  {:<6} {:<5} {:<4} {:<4} {}\r\n", "PIN", "GPIO", "PWM", "ADC", "NOTE");
        for (pin, gpio, pwm, adc, note) in rows {
            let yes = |b: bool| if b { "yes" } else { "-" };
            out!(self.term, "  GP{pin:<4} {:<5} {:<4} {:<4} {note}\r\n", yes(gpio), yes(pwm), yes(adc));
        }
        let reserved = self.sys.reserved_pins();
        let i2c = self.sys.i2c_pins();
        let mut lines: heapless::Vec<(&'static str, &'static str), 8> = heapless::Vec::new();
        for &r in reserved {
            let _ = lines.push(r);
        }
        if !lines.is_empty() {
            self.term.print("taken by the board:\r\n").await;
            for (pin, what) in lines {
                out!(self.term, "  {pin:<6} {what}\r\n");
            }
        }
        if let Some(i2c) = i2c {
            out!(self.term, "I2C bus: {i2c}\r\n");
        }
    }

    pub(crate) async fn cmd_gpio<'a>(&mut self, mut args: impl Iterator<Item = &'a str>) {
        const USAGE: &str = "gpio <pin> on | off | toggle | read [up | down]";
        let Some(pin) = self.pin_arg(args.next(), USAGE).await else { return };
        let bit = 1u64 << (pin % 64);
        let result = match args.next().unwrap_or("read") {
            "on" | "high" | "1" => self.sys.gpio_write(pin, true).map(|()| Some(true)),
            "off" | "low" | "0" => self.sys.gpio_write(pin, false).map(|()| Some(false)),
            "toggle" => {
                let high = self.gpio_levels & bit == 0;
                self.sys.gpio_write(pin, high).map(|()| Some(high))
            }
            "read" | "in" => {
                let pull = match args.next() {
                    Some("up") => Pull::Up,
                    Some("down") => Pull::Down,
                    _ => Pull::None,
                };
                match self.sys.gpio_read(pin, pull) {
                    Ok(high) => {
                        let pulled = match pull {
                            Pull::None => "",
                            Pull::Up => " (pull-up)",
                            Pull::Down => " (pull-down)",
                        };
                        out!(self.term, "GP{pin} = {}{pulled}\r\n", if high { "1 (high)" } else { "0 (low)" });
                        return;
                    }
                    Err(e) => Err(e),
                }
            }
            _ => {
                out!(self.term, "usage: {USAGE}\r\n");
                return;
            }
        };
        match result {
            Ok(Some(high)) => {
                if high {
                    self.gpio_levels |= bit;
                } else {
                    self.gpio_levels &= !bit;
                }
                out!(self.term, "GP{pin} -> {}\r\n", if high { "high" } else { "low" });
            }
            Ok(None) => {}
            Err(e) => self.hw_error(e).await,
        }
    }

    pub(crate) async fn cmd_pwm<'a>(&mut self, mut args: impl Iterator<Item = &'a str>) {
        const USAGE: &str = "pwm <pin> <0-100 %> [hz]   or   pwm <pin> off";
        let Some(pin) = self.pin_arg(args.next(), USAGE).await else { return };
        let level = args.next();
        if level == Some("off") {
            let result = self.sys.pwm_set(pin, 1000, 0).and_then(|_| self.sys.gpio_write(pin, false));
            match result {
                Ok(()) => out!(self.term, "GP{pin} pwm off\r\n"),
                Err(e) => self.hw_error(e).await,
            }
            return;
        }
        let Some(percent) = level.and_then(|v| v.trim_end_matches('%').parse::<f32>().ok()) else {
            out!(self.term, "usage: {USAGE}\r\n");
            return;
        };
        let hz = args.next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(1000);
        if !(0.0..=100.0).contains(&percent) {
            self.hw_error(HwError::OutOfRange).await;
            return;
        }
        match self.sys.pwm_set(pin, hz, (percent * 10.0) as u16) {
            Ok(actual) => out!(self.term, "GP{pin} pwm {percent:.1}% at {actual} Hz\r\n"),
            Err(e) => self.hw_error(e).await,
        }
    }

    pub(crate) async fn cmd_fade<'a>(&mut self, mut args: impl Iterator<Item = &'a str>) {
        let Some(pin) = self.pin_arg(args.next(), "fade <pin> [period ms]").await else { return };
        let period = args.next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(2000).clamp(200, 20_000);
        if let Err(e) = self.sys.pwm_set(pin, 1000, 0) {
            self.hw_error(e).await;
            return;
        }
        out!(self.term, "GP{pin} breathing, one breath every {period} ms (any key stops)\r\n");
        self.term.flush().await;
        const STEPS: u32 = 50;
        'outer: loop {
            // Up and down, with gamma correction so it looks smooth to the eye.
            for step in (0..STEPS).chain((0..STEPS).rev()) {
                let _ = self.sys.pwm_set(pin, 1000, gamma(step * 100 / (STEPS - 1)));
                if self.key_or_timeout((period / (2 * STEPS)) as u64).await {
                    break 'outer;
                }
            }
        }
        let _ = self.sys.pwm_set(pin, 1000, 0);
    }

    pub(crate) async fn cmd_adc<'a>(&mut self, mut args: impl Iterator<Item = &'a str>) {
        let Some(pin) = self.pin_arg(args.next(), "adc <pin> [watch]").await else { return };
        let watch = args.next() == Some("watch");
        if watch {
            out!(self.term, "GP{pin} live (any key stops)\r\n");
        }
        loop {
            match self.sys.adc_read(pin).await {
                Ok(mv) => {
                    let b = bar(mv as f32 / 3300.0, 40);
                    let end = if watch { "\x1b[K" } else { "\r\n" };
                    out!(self.term, "\rGP{pin}: {mv:4} mV  {:3}%  {}{end}", mv * 100 / 3300, b.as_str());
                }
                Err(e) => {
                    self.hw_error(e).await;
                    return;
                }
            }
            if !watch || self.key_or_timeout(100).await {
                break;
            }
        }
        if watch {
            self.term.print("\r\n").await;
        }
    }

    pub(crate) async fn cmd_knob<'a>(&mut self, mut args: impl Iterator<Item = &'a str>) {
        const USAGE: &str = "knob <adc pin> <pwm pin>   e.g. knob 26 16";
        let Some(adc) = self.pin_arg(args.next(), USAGE).await else { return };
        let Some(pwm) = self.pin_arg(args.next(), USAGE).await else { return };
        out!(self.term, "turn the knob on GP{adc}: it sets the brightness on GP{pwm} (any key stops)\r\n");
        loop {
            let mv = match self.sys.adc_read(adc).await {
                Ok(mv) => mv,
                Err(e) => {
                    self.hw_error(e).await;
                    return;
                }
            };
            let percent = (mv * 100 / 3300).min(100);
            if let Err(e) = self.sys.pwm_set(pwm, 1000, gamma(percent)) {
                self.hw_error(e).await;
                return;
            }
            let b = bar(percent as f32 / 100.0, 40);
            out!(self.term, "\r  {mv:4} mV -> {percent:3}%  {}\x1b[K", b.as_str());
            if self.key_or_timeout(30).await {
                break;
            }
        }
        self.term.print("\r\n").await;
    }

    pub(crate) async fn cmd_i2c<'a>(&mut self, mut args: impl Iterator<Item = &'a str>) {
        const USAGE: &str = "i2c scan | read <addr> <reg> [n] | write <addr> <byte>... | speed <khz> | reset";
        match args.next() {
            Some("scan") => self.i2c_scan().await,
            Some("reset") => match self.sys.i2c_recover() {
                Ok((sda, scl)) => {
                    let level = |high: bool| if high { "high (free)" } else { "LOW" };
                    out!(self.term, "bus reset: SDA {}, SCL {}\r\n", level(sda), level(scl));
                    if !(sda && scl) {
                        self.term.print("a line held low means a short to GND, swapped or loose wires, or a\r\n").await;
                        self.term
                            .print("device that is stuck: check the wiring, unplug its power for a moment\r\n")
                            .await;
                    }
                }
                Err(e) => self.hw_error(e).await,
            },
            Some("speed") => match args.next().and_then(|v| v.parse::<u32>().ok()) {
                Some(khz @ 10..=1000) => match self.sys.i2c_set_speed(khz * 1000) {
                    Ok(hz) => out!(self.term, "I2C clock {} kHz\r\n", hz / 1000),
                    Err(e) => self.hw_error(e).await,
                },
                _ => self.term.print("usage: i2c speed <10-1000 kHz>, e.g. i2c speed 100\r\n").await,
            },
            Some("read") => {
                let (Some(addr), Some(reg)) = (args.next().and_then(parse_addr), args.next().and_then(parse_addr))
                else {
                    out!(self.term, "usage: {USAGE}\r\n");
                    return;
                };
                let n = args.next().and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, 32);
                let mut buf = [0u8; 32];
                match self.sys.i2c_write_read(addr, &[reg], &mut buf[..n]).await {
                    Ok(()) => {
                        let mut line: String<160> = String::new();
                        for b in &buf[..n] {
                            let _ = write!(line, "{b:02x} ");
                        }
                        out!(self.term, "0x{addr:02x} reg 0x{reg:02x}: {}\r\n", line.as_str());
                    }
                    Err(e) => self.hw_error(e).await,
                }
            }
            Some("write") => {
                let Some(addr) = args.next().and_then(parse_addr) else {
                    out!(self.term, "usage: {USAGE}\r\n");
                    return;
                };
                let mut bytes: heapless::Vec<u8, 32> = heapless::Vec::new();
                for v in args {
                    match parse_addr(v) {
                        Some(b) if bytes.push(b).is_ok() => {}
                        _ => {
                            out!(self.term, "bad byte '{v}' (use hex like 0x1f, at most 32 bytes)\r\n");
                            return;
                        }
                    }
                }
                if bytes.is_empty() {
                    out!(self.term, "usage: {USAGE}\r\n");
                    return;
                }
                match self.sys.i2c_write(addr, &bytes).await {
                    Ok(()) => out!(self.term, "wrote {} byte(s) to 0x{addr:02x}\r\n", bytes.len()),
                    Err(e) => self.hw_error(e).await,
                }
            }
            _ => out!(self.term, "usage: {USAGE}\r\n"),
        }
    }

    async fn i2c_scan(&mut self) {
        match self.sys.i2c_pins() {
            Some(pins) => out!(self.term, "scanning I2C ({pins})...\r\n"),
            None => {
                self.hw_error(HwError::Unsupported).await;
                return;
            }
        }
        // The classic i2cdetect grid. Addresses 0x00-0x07 and 0x78-0x7f are reserved.
        self.term.print("     0  1  2  3  4  5  6  7  8  9  a  b  c  d  e  f\r\n").await;
        let mut found: heapless::Vec<u8, 112> = heapless::Vec::new();
        let mut bus_error = false;
        for row in 0..8u8 {
            let mut line: String<64> = String::new();
            let _ = write!(line, "{:02x}:", row * 16);
            for col in 0..16u8 {
                let addr = row * 16 + col;
                if !(0x08..0x78).contains(&addr) {
                    let _ = line.push_str("   ");
                    continue;
                }
                let mut byte = [0u8; 1];
                match self.sys.i2c_read(addr, &mut byte).await {
                    Ok(()) => {
                        let _ = write!(line, " {addr:02x}");
                        let _ = found.push(addr);
                    }
                    Err(HwError::Bus | HwError::Timeout) => {
                        bus_error = true;
                        let _ = line.push_str(" !!");
                    }
                    Err(_) => {
                        let _ = line.push_str(" --");
                    }
                }
            }
            out!(self.term, "{}\r\n", line.as_str());
        }
        if found.is_empty() {
            self.term.print("no devices found").await;
            if bus_error {
                self.term.print(" (bus errors: check wiring, power and pull-ups)").await;
            }
            self.term.print("\r\n").await;
        }
        for addr in found {
            let name = i2c_device_name(addr);
            out!(self.term, "  0x{addr:02x}  {}\r\n", if name.is_empty() { "unknown device" } else { name });
        }
    }

    // -----------------------------------------------------------------------
    // OLED (SSD1306, 128x64, I2C address 0x3C)
    // -----------------------------------------------------------------------

    async fn oled_cmds(&mut self, cmds: &[u8]) -> Result<(), HwError> {
        let mut buf = [0u8; 32];
        buf[0] = oled::CMD;
        buf[1..=cmds.len()].copy_from_slice(cmds);
        self.sys.i2c_write(oled::ADDRESS, &buf[..=cmds.len()]).await
    }

    /// Initialises the display the first time it is used.
    async fn oled_init(&mut self) -> Result<(), HwError> {
        if !self.oled_ready {
            self.oled_cmds(&oled::INIT).await?;
            self.oled_ready = true;
        }
        Ok(())
    }

    /// Sends the whole framebuffer.
    pub(crate) async fn oled_flush(&mut self) -> Result<(), HwError> {
        self.oled_init().await?;
        self.oled_cmds(&oled::WINDOW).await?;
        let mut buf = [0u8; 1 + oled::WIDTH];
        buf[0] = oled::DATA;
        for page in 0..oled::HEIGHT / 8 {
            buf[1..].copy_from_slice(&self.oled.fb[page * oled::WIDTH..(page + 1) * oled::WIDTH]);
            self.sys.i2c_write(oled::ADDRESS, &buf).await?;
        }
        Ok(())
    }

    async fn oled_error(&mut self, e: HwError) {
        self.oled_ready = false; // re-initialise next time, the module may have been replugged
        match e {
            HwError::Nack | HwError::Bus | HwError::Timeout => {
                out!(self.term, "error: no OLED answered at 0x3c ({e})\r\n");
                if let Some(pins) = self.sys.i2c_pins() {
                    out!(self.term, "wiring: VCC to 3V3, GND to GND, {pins}\r\n");
                }
            }
            e => self.hw_error(e).await,
        }
    }

    fn oled_logo(&mut self) {
        let board = self.sys.info().board;
        let d = &mut self.oled;
        d.clear();
        d.text_centered(4, "yanarkaze", 2);
        d.hline(8, 25, oled::WIDTH as i32 - 16);
        d.text_centered(31, "fire + wind", 1);
        d.text_centered(43, board, 1);
        let mut version: String<16> = String::new();
        let _ = write!(version, "v{}", crate::VERSION);
        d.text_centered(55, &version, 1);
    }

    /// Shows the logo at boot if a display is attached; silent otherwise.
    pub(crate) async fn oled_boot(&mut self) {
        if self.oled_init().await.is_ok() {
            self.oled_logo();
            if self.oled_flush().await.is_err() {
                self.oled_ready = false;
            }
        }
    }

    pub(crate) async fn cmd_oled(&mut self, rest: &str) {
        const USAGE: &str =
            "oled text <msg> | temp | dash | life | logo | preview | clear | invert | contrast <0-255> | on | off";
        let (typed, arg) = crate::shell::split_word(rest);
        // Accept unambiguous abbreviations: `oled tex hi` is `oled text hi`.
        let Some(action) = crate::shell::resolve(typed, crate::shell::OLED_ACTIONS) else {
            match crate::shell::suggest(typed, crate::shell::OLED_ACTIONS.iter().copied()) {
                Some(close) if !typed.is_empty() => {
                    out!(self.term, "unknown oled action '{typed}' - did you mean 'oled {close}'?\r\n")
                }
                _ => out!(self.term, "usage: {USAGE}\r\n"),
            }
            return;
        };
        let result = match action {
            "text" if !arg.is_empty() => {
                self.oled.clear();
                self.oled.text_fit(arg);
                self.oled_flush().await
            }
            "clear" => {
                self.oled.clear();
                self.oled_flush().await
            }
            "logo" => {
                self.oled_logo();
                self.oled_flush().await
            }
            "invert" => {
                self.oled_inverted = !self.oled_inverted;
                let cmd = if self.oled_inverted { 0xA7 } else { 0xA6 };
                match self.oled_init().await {
                    Ok(()) => self.oled_cmds(&[cmd]).await,
                    Err(e) => Err(e),
                }
            }
            "contrast" => match arg.parse::<u8>() {
                Ok(level) => match self.oled_init().await {
                    Ok(()) => self.oled_cmds(&[0x81, level]).await,
                    Err(e) => Err(e),
                },
                Err(_) => {
                    self.term.print("usage: oled contrast <0-255>\r\n").await;
                    return;
                }
            },
            "on" | "off" => match self.oled_init().await {
                Ok(()) => self.oled_cmds(&[if action == "on" { 0xAF } else { 0xAE }]).await,
                Err(e) => Err(e),
            },
            "preview" => {
                self.oled_preview().await;
                return;
            }
            "temp" => self.oled_temp().await,
            "dash" => self.oled_dash().await,
            "life" => self.oled_life().await,
            _ => {
                out!(self.term, "usage: {USAGE}\r\n");
                return;
            }
        };
        if let Err(e) = result {
            self.oled_error(e).await;
        }
    }

    /// Draws the framebuffer in the terminal with braille characters (2x4 pixels each).
    async fn oled_preview(&mut self) {
        let mut edge: String<200> = String::new();
        for _ in 0..oled::WIDTH / 2 {
            let _ = edge.push('\u{2500}');
        }
        out!(self.term, "\u{250c}{}\u{2510}\r\n", edge.as_str());
        for row in 0..oled::HEIGHT / 4 {
            let mut line: String<256> = String::new();
            let _ = line.push('\u{2502}');
            for col in 0..oled::WIDTH / 2 {
                let (x, y) = (col * 2, row * 4);
                const DOTS: [[u32; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
                let mut bits = 0;
                for (dx, column) in DOTS.iter().enumerate() {
                    for (dy, bit) in column.iter().enumerate() {
                        if self.oled.pixel(x + dx, y + dy) {
                            bits |= bit;
                        }
                    }
                }
                let _ = line.push(char::from_u32(0x2800 + bits).unwrap_or(' '));
            }
            let _ = line.push_str("\u{2502}\r\n");
            self.term.print(&line).await;
        }
        out!(self.term, "\u{2514}{}\u{2518}\r\n", edge.as_str());
    }

    /// Live temperature: a big number and the last minute as a bar chart.
    async fn oled_temp(&mut self) -> Result<(), HwError> {
        self.term.print("showing the temperature on the OLED (any key stops)\r\n").await;
        loop {
            let now = SAMPLE.lock(|s| s.get()).reading.temp_c;
            let data: heapless::Vec<f32, HISTORY_SECS> = TEMP_HISTORY.lock(|h| h.borrow().iter().collect());
            let d = &mut self.oled;
            d.clear();
            let mut big: String<16> = String::new();
            let _ = write!(big, "{now:.1}{}C", oled::DEGREE);
            d.text_centered(2, &big, 3);
            // Chart in the bottom 28 rows, two pixels per second.
            let (top, height) = (35, 28);
            d.rect(0, top, oled::WIDTH as i32, height + 1);
            if !data.is_empty() {
                let (mut lo, mut hi) = data.iter().fold((f32::MAX, f32::MIN), |(l, h), &v| (l.min(v), h.max(v)));
                if hi - lo < 1.0 {
                    let mid = (hi + lo) / 2.0;
                    (lo, hi) = (mid - 0.5, mid + 0.5);
                }
                let x0 = oled::WIDTH as i32 - 2 - data.len() as i32 * 2;
                for (i, &v) in data.iter().enumerate() {
                    let h = (((v - lo) / (hi - lo)) * (height - 3) as f32) as i32 + 1;
                    d.fill_rect(x0 + i as i32 * 2, top + height - h, 2, h, true);
                }
            }
            self.oled_flush().await?;
            if self.key_or_timeout(1000).await {
                return Ok(());
            }
        }
    }

    /// A live status page.
    async fn oled_dash(&mut self) -> Result<(), HwError> {
        self.term.print("showing a dashboard on the OLED (any key stops)\r\n").await;
        loop {
            let ms = embassy_time::Instant::now().as_millis();
            let (h, m, s) = (ms / 3_600_000, (ms / 60_000) % 60, (ms / 1000) % 60);
            let temp = SAMPLE.lock(|s| s.get()).reading.temp_c;
            let led = current_led_mode().name();
            let presses = BUTTON_PRESSES.load(Ordering::Relaxed);
            let d = &mut self.oled;
            d.clear();
            // Header in inverse video: draw the text, then flip the bar.
            d.text_centered(1, "yanarkaze", 1);
            d.invert_rect(0, 0, oled::WIDTH as i32, 10);
            let mut line: String<32> = String::new();
            let _ = write!(line, "{h:02}:{m:02}:{s:02}");
            d.text_centered(14, &line, 2);
            line.clear();
            let _ = write!(line, "{temp:.1}{}C", oled::DEGREE);
            d.text(0, 36, "temp", 1);
            d.text(40, 36, &line, 1);
            d.text(0, 46, "led", 1);
            d.text(40, 46, led, 1);
            line.clear();
            let _ = write!(line, "{presses}");
            d.text(0, 56, "button", 1);
            d.text(40, 56, &line, 1);
            self.oled_flush().await?;
            if self.key_or_timeout(500).await {
                return Ok(());
            }
        }
    }

    /// Conway's Game of Life on all 8192 pixels; reseeds when it dies out or settles.
    async fn oled_life(&mut self) -> Result<(), HwError> {
        self.term.print("Game of Life on the OLED (any key stops)\r\n").await;
        let mut same = 0;
        let mut last = u32::MAX;
        loop {
            let count = self.oled.count();
            if count == 0 || same > 40 {
                for i in 0..self.oled.fb.len() {
                    // Two random bytes ANDed: about 25 % of the pixels start alive.
                    let r = self.sys.random_u32();
                    self.oled.fb[i] = (r as u8) & (r >> 8) as u8;
                }
                same = 0;
            } else {
                self.oled.life_step();
            }
            let count = self.oled.count();
            // Oscillators keep the count bouncing between a few values; treat that as settled.
            same = if count.abs_diff(last) <= 2 { same + 1 } else { 0 };
            last = count;
            self.oled_flush().await?;
            if self.key_or_timeout(20).await {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_and_addresses() {
        assert_eq!(parse_pin("16"), Some(16));
        assert_eq!(parse_pin("GP16"), Some(16));
        assert_eq!(parse_pin("gp3"), Some(3));
        assert_eq!(parse_pin("x"), None);
        assert_eq!(parse_addr("0x3c"), Some(0x3C));
        assert_eq!(parse_addr("3c"), Some(0x3C));
        assert_eq!(parse_addr("60"), Some(60));
        assert_eq!(parse_addr("0x100"), None);
    }

    #[test]
    fn gamma_curve() {
        assert_eq!(gamma(0), 0);
        assert_eq!(gamma(50), 250);
        assert_eq!(gamma(100), 1000);
        assert_eq!(gamma(150), 1000);
    }

    #[test]
    fn known_devices() {
        assert!(i2c_device_name(0x3C).contains("OLED"));
        assert_eq!(i2c_device_name(0x11), "");
    }
}
