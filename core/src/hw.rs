//! Hardware playground commands: pins, GPIO, PWM, ADC and I2C.
//!
//! Everything goes through the optional pin and bus methods of [`System`], so a board only
//! implements what it has and the rest reports "not supported".

use core::fmt::Write as _;

use embedded_io_async::{Read, Write};
use heapless::String;

use crate::platform::{HwError, Pull, System};
use crate::shell::Shell;

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
        const USAGE: &str = "i2c scan | read <addr> <reg> [n] | write <addr> <byte>...";
        match args.next() {
            Some("scan") => self.i2c_scan().await,
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
                    Err(HwError::Bus) => {
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
