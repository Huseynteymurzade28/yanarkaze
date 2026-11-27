//! The colour OLED: an SSD1351 (128x128, 16-bit colour) on SPI, through the board's
//! `rgb_*` methods.
//!
//! There is no way to ask an SPI display whether it is there (nothing comes back), so it is
//! switched on with `oled color on`. Every `oled` screen then shows on it too, twice as
//! tall (the small OLED is 128x64) and in the colours of a fire.

use crate::oled::{HEIGHT, WIDTH};
use crate::platform::{HwError, System};

pub const SIZE: usize = 128;

/// 16-bit colour (5 bits red, 6 green, 5 blue).
pub const fn rgb565(r: u8, g: u8, b: u8) -> u16 {
    ((r as u16 & 0xF8) << 8) | ((g as u16 & 0xFC) << 3) | (b as u16 >> 3)
}

pub const BLACK: u16 = rgb565(0, 0, 0);

/// The SSD1351's start-up sequence (after the reset pulse the board gives it).
const INIT: &[(u8, &[u8])] = &[
    (0xFD, &[0x12]), // unlock the command interface
    (0xFD, &[0xB1]), // and the commands that are locked by default
    (0xAE, &[]),     // display off while setting up
    (0xB3, &[0xF1]), // clock divider and oscillator
    (0xCA, &[0x7F]), // 128 rows
    // 65k colours, colour order C-B-A (so data is R, G, B), scan from the bottom row with
    // odd/even rows split: the orientation the usual 1.5" modules want.
    (0xA0, &[0x74]),
    (0x15, &[0x00, 0x7F]),       // columns 0-127
    (0x75, &[0x00, 0x7F]),       // rows 0-127
    (0xA1, &[0x00]),             // start line
    (0xA2, &[0x00]),             // display offset
    (0xB5, &[0x00]),             // GPIOs off
    (0xAB, &[0x01]),             // internal VDD regulator
    (0xB1, &[0x32]),             // pre-charge phases
    (0xBE, &[0x05]),             // VCOMH
    (0xA6, &[]),                 // normal (not inverted)
    (0xC1, &[0xC8, 0x80, 0xC8]), // contrast for each colour
    (0xC7, &[0x0F]),             // master contrast
    (0xB4, &[0xA0, 0xB5, 0x55]), // segment low voltage
    (0xB6, &[0x01]),             // second pre-charge
    (0xAF, &[]),                 // display on
];

pub fn init<S: System>(sys: &mut S) -> Result<(), HwError> {
    sys.rgb_enable(true)?;
    for &(cmd, params) in INIT {
        sys.rgb_command(cmd, params)?;
    }
    fill(sys, 0, 0, SIZE, SIZE, BLACK)
}

/// Selects the rectangle the following pixel data fills, left to right, top to bottom.
fn window<S: System>(sys: &mut S, x: usize, y: usize, w: usize, h: usize) -> Result<(), HwError> {
    sys.rgb_command(0x15, &[x as u8, (x + w - 1) as u8])?;
    sys.rgb_command(0x75, &[y as u8, (y + h - 1) as u8])?;
    sys.rgb_command(0x5C, &[]) // write to RAM
}

pub fn fill<S: System>(sys: &mut S, x: usize, y: usize, w: usize, h: usize, color: u16) -> Result<(), HwError> {
    window(sys, x, y, w, h)?;
    let mut row = [0u8; 2 * SIZE];
    for px in row.chunks_mut(2) {
        px.copy_from_slice(&color.to_be_bytes());
    }
    for _ in 0..h {
        sys.rgb_data(&row[..2 * w])?;
    }
    Ok(())
}

/// The colour of lit pixels on row `y` of the 128 rows: the header bar amber, the rest a
/// fire, yellow at the top fading to orange-red at the bottom.
pub fn fire(y: usize) -> u16 {
    if y < 20 {
        return rgb565(255, 176, 0);
    }
    let t = (y - 20) as u32 * 255 / (SIZE as u32 - 21);
    rgb565(255, (230 - t * 170 / 255) as u8, (60 - t * 60 / 255) as u8)
}

/// Shows the small OLED's 128x64 framebuffer (its page layout: byte `x` of page `p` holds
/// rows `8p..8p+8`, bit 0 on top), every row twice so it fills the screen.
pub fn show_mono<S: System>(sys: &mut S, fb: &[u8]) -> Result<(), HwError> {
    window(sys, 0, 0, SIZE, SIZE)?;
    let mut row = [0u8; 2 * SIZE];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let on = fb[(y / 8) * WIDTH + x] & (1 << (y % 8)) != 0;
            // Both copies of the row share a colour, so the gradient steps in pairs.
            let color = if on { fire(2 * y) } else { BLACK };
            row[2 * x..2 * x + 2].copy_from_slice(&color.to_be_bytes());
        }
        sys.rgb_data(&row)?;
        sys.rgb_data(&row)?;
    }
    Ok(())
}

/// Colour bars, to check the wiring and the colour order: white, yellow, cyan, green,
/// magenta, red, blue, black across the top, then red, green and blue ramps.
pub fn test_pattern<S: System>(sys: &mut S) -> Result<(), HwError> {
    const BARS: [u16; 8] = [
        rgb565(255, 255, 255),
        rgb565(255, 255, 0),
        rgb565(0, 255, 255),
        rgb565(0, 255, 0),
        rgb565(255, 0, 255),
        rgb565(255, 0, 0),
        rgb565(0, 0, 255),
        BLACK,
    ];
    for (i, &c) in BARS.iter().enumerate() {
        fill(sys, i * 16, 0, 16, 64, c)?;
    }
    for band in 0..3 {
        window(sys, 0, 64 + band * 21, SIZE, 21)?;
        let mut row = [0u8; 2 * SIZE];
        for x in 0..SIZE {
            let v = (x * 255 / (SIZE - 1)) as u32;
            let (r, g, b) = match band {
                0 => (v, 0, 0),
                1 => (0, v, 0),
                _ => (0, 0, v),
            };
            row[2 * x..2 * x + 2].copy_from_slice(&rgb565(r as u8, g as u8, b as u8).to_be_bytes());
        }
        for _ in 0..21 {
            sys.rgb_data(&row)?;
        }
    }
    Ok(())
}

/// The colour for a Mandelbrot point that escaped after `n` of `max` iterations: deep blue
/// through red and orange to yellow as it gets closer to the set, which is black.
pub fn mandel_color(n: u32, max: u32) -> u16 {
    if n >= max {
        return BLACK;
    }
    let t = (n * 1024 / max) as i32; // 0..1023
    let ramp = |from: i32, to: i32, lo: i32, hi: i32| -> u8 {
        let k = (t - lo).clamp(0, hi - lo) * 255 / (hi - lo).max(1);
        (from + (to - from) * k / 255) as u8
    };
    match t {
        0..=127 => rgb565(0, 0, ramp(40, 160, 0, 127)),
        128..=383 => rgb565(ramp(0, 220, 128, 383), 0, ramp(160, 60, 128, 383)),
        384..=767 => rgb565(ramp(220, 255, 384, 767), ramp(0, 160, 384, 767), ramp(60, 0, 384, 767)),
        _ => rgb565(255, ramp(160, 255, 768, 1023), ramp(0, 200, 768, 1023)),
    }
}

/// Renders rows `rows` of the Mandelbrot set straight to the display (no framebuffer:
/// 128x128 in colour would take 32 KB). Returns the iterations computed.
pub fn mandel_rows<S: System>(sys: &mut S, rows: core::ops::Range<usize>) -> Result<u64, HwError> {
    const MAX: u32 = 96;
    const SCALE: f32 = 2.4 / SIZE as f32;
    window(sys, 0, rows.start, SIZE, rows.len())?;
    let mut iterations = 0u64;
    let mut row = [0u8; 2 * SIZE];
    for y in rows {
        let ci = -1.2 + SCALE * (y as f32 + 0.5);
        for x in 0..SIZE {
            let cr = -2.05 + SCALE * (x as f32 + 0.5);
            let (mut zr, mut zi, mut n) = (0.0f32, 0.0f32, 0u32);
            while n < MAX && zr * zr + zi * zi <= 4.0 {
                let t = zr * zr - zi * zi + cr;
                zi = 2.0 * zr * zi + ci;
                zr = t;
                n += 1;
            }
            iterations += n as u64;
            row[2 * x..2 * x + 2].copy_from_slice(&mandel_color(n, MAX).to_be_bytes());
        }
        sys.rgb_data(&row)?;
    }
    Ok(iterations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors() {
        assert_eq!(rgb565(255, 255, 255), 0xFFFF);
        assert_eq!(rgb565(255, 0, 0), 0xF800);
        assert_eq!(rgb565(0, 255, 0), 0x07E0);
        assert_eq!(rgb565(0, 0, 255), 0x001F);
        assert_eq!(mandel_color(96, 96), BLACK);
        // Deep blue far from the set, warm close to it.
        assert_eq!(mandel_color(0, 96) & 0xF800, 0);
        assert_eq!(mandel_color(95, 96) >> 11, 31);
    }
}
