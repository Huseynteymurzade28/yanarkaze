//! A 128x64 SSD1306 OLED: framebuffer, 5x7 font, drawing and the controller's commands.
//!
//! Everything here is plain memory work; the shell sends [`INIT`] and the framebuffer over
//! I2C. The framebuffer uses the controller's own layout (8 pages of 128 columns, one byte
//! per column holding 8 vertical pixels, LSB on top), so it goes out without conversion.

pub const WIDTH: usize = 128;
pub const HEIGHT: usize = 64;
pub const ADDRESS: u8 = 0x3C;

/// I2C control bytes: what follows is commands / pixel data.
pub const CMD: u8 = 0x00;
pub const DATA: u8 = 0x40;

/// Power-up sequence for a 128x64 module with the internal charge pump.
pub const INIT: [u8; 26] = [
    0xAE, // display off
    0xD5, 0x80, // clock divider
    0xA8, 0x3F, // multiplex: 64 lines
    0xD3, 0x00, // no display offset
    0x40, // start line 0
    0x8D, 0x14, // charge pump on
    0x20, 0x00, // horizontal addressing
    0xA1, // column 127 is SEG0 (so the image is not mirrored)
    0xC8, // scan COM from the bottom (so it is not upside down)
    0xDA, 0x12, // COM pins: alternative, no remap
    0x81, 0xCF, // contrast
    0xD9, 0xF1, // pre-charge
    0xDB, 0x40, // VCOMH level
    0xA4, // show the RAM contents
    0xA6, // normal (not inverted)
    0x2E, // no scrolling
    0xAF, // display on
];

/// Sent before every frame: the whole screen as the write window.
pub const WINDOW: [u8; 6] = [0x21, 0, (WIDTH - 1) as u8, 0x22, 0, (HEIGHT / 8 - 1) as u8];

/// 5x7 glyphs for ASCII 0x20..=0x7E, one byte per column, LSB on top. 0x7F is a degree sign.
const FONT: [[u8; 5]; 96] = [
    [0x00, 0x00, 0x00, 0x00, 0x00], // ' '
    [0x00, 0x00, 0x5F, 0x00, 0x00], // !
    [0x00, 0x07, 0x00, 0x07, 0x00], // "
    [0x14, 0x7F, 0x14, 0x7F, 0x14], // #
    [0x24, 0x2A, 0x7F, 0x2A, 0x12], // $
    [0x23, 0x13, 0x08, 0x64, 0x62], // %
    [0x36, 0x49, 0x56, 0x20, 0x50], // &
    [0x00, 0x05, 0x03, 0x00, 0x00], // '
    [0x00, 0x1C, 0x22, 0x41, 0x00], // (
    [0x00, 0x41, 0x22, 0x1C, 0x00], // )
    [0x2A, 0x1C, 0x7F, 0x1C, 0x2A], // *
    [0x08, 0x08, 0x3E, 0x08, 0x08], // +
    [0x00, 0x50, 0x30, 0x00, 0x00], // ,
    [0x08, 0x08, 0x08, 0x08, 0x08], // -
    [0x00, 0x60, 0x60, 0x00, 0x00], // .
    [0x20, 0x10, 0x08, 0x04, 0x02], // /
    [0x3E, 0x51, 0x49, 0x45, 0x3E], // 0
    [0x00, 0x42, 0x7F, 0x40, 0x00], // 1
    [0x42, 0x61, 0x51, 0x49, 0x46], // 2
    [0x21, 0x41, 0x45, 0x4B, 0x31], // 3
    [0x18, 0x14, 0x12, 0x7F, 0x10], // 4
    [0x27, 0x45, 0x45, 0x45, 0x39], // 5
    [0x3C, 0x4A, 0x49, 0x49, 0x30], // 6
    [0x01, 0x71, 0x09, 0x05, 0x03], // 7
    [0x36, 0x49, 0x49, 0x49, 0x36], // 8
    [0x06, 0x49, 0x49, 0x29, 0x1E], // 9
    [0x00, 0x36, 0x36, 0x00, 0x00], // :
    [0x00, 0x56, 0x36, 0x00, 0x00], // ;
    [0x08, 0x14, 0x22, 0x41, 0x00], // <
    [0x14, 0x14, 0x14, 0x14, 0x14], // =
    [0x00, 0x41, 0x22, 0x14, 0x08], // >
    [0x02, 0x01, 0x51, 0x09, 0x06], // ?
    [0x32, 0x49, 0x79, 0x41, 0x3E], // @
    [0x7E, 0x11, 0x11, 0x11, 0x7E], // A
    [0x7F, 0x49, 0x49, 0x49, 0x36], // B
    [0x3E, 0x41, 0x41, 0x41, 0x22], // C
    [0x7F, 0x41, 0x41, 0x22, 0x1C], // D
    [0x7F, 0x49, 0x49, 0x49, 0x41], // E
    [0x7F, 0x09, 0x09, 0x09, 0x01], // F
    [0x3E, 0x41, 0x49, 0x49, 0x7A], // G
    [0x7F, 0x08, 0x08, 0x08, 0x7F], // H
    [0x00, 0x41, 0x7F, 0x41, 0x00], // I
    [0x20, 0x40, 0x41, 0x3F, 0x01], // J
    [0x7F, 0x08, 0x14, 0x22, 0x41], // K
    [0x7F, 0x40, 0x40, 0x40, 0x40], // L
    [0x7F, 0x02, 0x0C, 0x02, 0x7F], // M
    [0x7F, 0x04, 0x08, 0x10, 0x7F], // N
    [0x3E, 0x41, 0x41, 0x41, 0x3E], // O
    [0x7F, 0x09, 0x09, 0x09, 0x06], // P
    [0x3E, 0x41, 0x51, 0x21, 0x5E], // Q
    [0x7F, 0x09, 0x19, 0x29, 0x46], // R
    [0x46, 0x49, 0x49, 0x49, 0x31], // S
    [0x01, 0x01, 0x7F, 0x01, 0x01], // T
    [0x3F, 0x40, 0x40, 0x40, 0x3F], // U
    [0x1F, 0x20, 0x40, 0x20, 0x1F], // V
    [0x3F, 0x40, 0x38, 0x40, 0x3F], // W
    [0x63, 0x14, 0x08, 0x14, 0x63], // X
    [0x07, 0x08, 0x70, 0x08, 0x07], // Y
    [0x61, 0x51, 0x49, 0x45, 0x43], // Z
    [0x00, 0x7F, 0x41, 0x41, 0x00], // [
    [0x02, 0x04, 0x08, 0x10, 0x20], // backslash
    [0x00, 0x41, 0x41, 0x7F, 0x00], // ]
    [0x04, 0x02, 0x01, 0x02, 0x04], // ^
    [0x40, 0x40, 0x40, 0x40, 0x40], // _
    [0x00, 0x01, 0x02, 0x04, 0x00], // `
    [0x20, 0x54, 0x54, 0x54, 0x78], // a
    [0x7F, 0x48, 0x44, 0x44, 0x38], // b
    [0x38, 0x44, 0x44, 0x44, 0x20], // c
    [0x38, 0x44, 0x44, 0x48, 0x7F], // d
    [0x38, 0x54, 0x54, 0x54, 0x18], // e
    [0x08, 0x7E, 0x09, 0x01, 0x02], // f
    [0x0C, 0x52, 0x52, 0x52, 0x3E], // g
    [0x7F, 0x08, 0x04, 0x04, 0x78], // h
    [0x00, 0x44, 0x7D, 0x40, 0x00], // i
    [0x20, 0x40, 0x44, 0x3D, 0x00], // j
    [0x7F, 0x10, 0x28, 0x44, 0x00], // k
    [0x00, 0x41, 0x7F, 0x40, 0x00], // l
    [0x7C, 0x04, 0x18, 0x04, 0x78], // m
    [0x7C, 0x08, 0x04, 0x04, 0x78], // n
    [0x38, 0x44, 0x44, 0x44, 0x38], // o
    [0x7C, 0x14, 0x14, 0x14, 0x08], // p
    [0x08, 0x14, 0x14, 0x18, 0x7C], // q
    [0x7C, 0x08, 0x04, 0x04, 0x08], // r
    [0x48, 0x54, 0x54, 0x54, 0x20], // s
    [0x04, 0x3F, 0x44, 0x40, 0x20], // t
    [0x3C, 0x40, 0x40, 0x20, 0x7C], // u
    [0x1C, 0x20, 0x40, 0x20, 0x1C], // v
    [0x3C, 0x40, 0x30, 0x40, 0x3C], // w
    [0x44, 0x28, 0x10, 0x28, 0x44], // x
    [0x0C, 0x50, 0x50, 0x50, 0x3C], // y
    [0x44, 0x64, 0x54, 0x4C, 0x44], // z
    [0x00, 0x08, 0x36, 0x41, 0x00], // {
    [0x00, 0x00, 0x7F, 0x00, 0x00], // |
    [0x00, 0x41, 0x36, 0x08, 0x00], // }
    [0x08, 0x04, 0x08, 0x10, 0x08], // ~
    [0x00, 0x06, 0x09, 0x09, 0x06], // degree sign (0x7F)
];

/// The character that draws a degree sign.
pub const DEGREE: char = '\x7f';

/// Width of one character cell at scale 1 (5 pixels plus a gap).
pub const CHAR_W: usize = 6;
pub const CHAR_H: usize = 8;

pub struct Display {
    pub fb: [u8; WIDTH * HEIGHT / 8],
}

impl Display {
    pub const fn new() -> Self {
        Self { fb: [0; WIDTH * HEIGHT / 8] }
    }

    pub fn clear(&mut self) {
        self.fb.fill(0);
    }

    pub fn pixel(&self, x: usize, y: usize) -> bool {
        x < WIDTH && y < HEIGHT && self.fb[(y / 8) * WIDTH + x] & (1 << (y % 8)) != 0
    }

    pub fn set(&mut self, x: i32, y: i32, on: bool) {
        if (0..WIDTH as i32).contains(&x) && (0..HEIGHT as i32).contains(&y) {
            let (x, y) = (x as usize, y as usize);
            let byte = &mut self.fb[(y / 8) * WIDTH + x];
            if on {
                *byte |= 1 << (y % 8);
            } else {
                *byte &= !(1 << (y % 8));
            }
        }
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, on: bool) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.set(xx, yy, on);
            }
        }
    }

    /// Flips every pixel in the rectangle: text drawn first becomes inverse video.
    pub fn invert_rect(&mut self, x: i32, y: i32, w: i32, h: i32) {
        for yy in y..y + h {
            for xx in x..x + w {
                if (0..WIDTH as i32).contains(&xx) && (0..HEIGHT as i32).contains(&yy) {
                    let lit = self.pixel(xx as usize, yy as usize);
                    self.set(xx, yy, !lit);
                }
            }
        }
    }

    pub fn hline(&mut self, x: i32, y: i32, w: i32) {
        self.fill_rect(x, y, w, 1, true);
    }

    pub fn vline(&mut self, x: i32, y: i32, h: i32) {
        self.fill_rect(x, y, 1, h, true);
    }

    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32) {
        self.hline(x, y, w);
        self.hline(x, y + h - 1, w);
        self.vline(x, y, h);
        self.vline(x + w - 1, y, h);
    }

    /// Draws one character with its top-left corner at (x, y), `scale` times the font size.
    pub fn char(&mut self, x: i32, y: i32, c: char, scale: i32) {
        let index = match c as u32 {
            0x20..=0x7F => c as usize - 0x20,
            _ => '?' as usize - 0x20,
        };
        for (col, bits) in FONT[index].iter().enumerate() {
            for row in 0..7 {
                if bits & (1 << row) != 0 {
                    self.fill_rect(x + col as i32 * scale, y + row * scale, scale, scale, true);
                }
            }
        }
    }

    /// Draws a line of text and returns the x where it ended.
    pub fn text(&mut self, x: i32, y: i32, text: &str, scale: i32) -> i32 {
        let mut x = x;
        for c in text.chars() {
            self.char(x, y, c, scale);
            x += CHAR_W as i32 * scale;
        }
        x
    }

    /// Draws text horizontally centred on the screen.
    pub fn text_centered(&mut self, y: i32, text: &str, scale: i32) {
        let w = text.chars().count() as i32 * CHAR_W as i32 * scale - scale; // no trailing gap
        self.text((WIDTH as i32 - w) / 2, y, text, scale);
    }

    /// Word-wraps `text` at the largest scale (up to 4) where it fits, and centres it.
    /// Returns the scale used.
    pub fn text_fit(&mut self, text: &str) -> i32 {
        for scale in (1..=4).rev() {
            let cols = WIDTH / (CHAR_W * scale as usize);
            let rows = HEIGHT / (CHAR_H * scale as usize);
            let mut lines: heapless::Vec<heapless::String<32>, 8> = heapless::Vec::new();
            if !wrap(text, cols, rows, &mut lines) {
                continue;
            }
            let top = (HEIGHT as i32 - lines.len() as i32 * CHAR_H as i32 * scale) / 2;
            for (i, line) in lines.iter().enumerate() {
                self.text_centered(top + i as i32 * CHAR_H as i32 * scale, line, scale);
            }
            return scale;
        }
        // Too long even at scale 1: show what fits.
        let mut y = 0;
        for chunk in text.as_bytes().chunks(WIDTH / CHAR_W) {
            if y + CHAR_H as i32 > HEIGHT as i32 {
                break;
            }
            self.text(0, y, core::str::from_utf8(chunk).unwrap_or(""), 1);
            y += CHAR_H as i32;
        }
        1
    }

    /// One generation of Conway's Game of Life on the whole screen (a torus).
    pub fn life_step(&mut self) {
        let old = self.fb;
        let alive = |x: usize, y: usize| old[(y / 8) * WIDTH + x] & (1 << (y % 8)) != 0;
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let mut n = 0;
                for dy in [HEIGHT - 1, 0, 1] {
                    for dx in [WIDTH - 1, 0, 1] {
                        if (dx, dy) != (0, 0) && alive((x + dx) % WIDTH, (y + dy) % HEIGHT) {
                            n += 1;
                        }
                    }
                }
                let on = matches!((alive(x, y), n), (true, 2) | (_, 3));
                self.set(x as i32, y as i32, on);
            }
        }
    }

    pub fn count(&self) -> u32 {
        self.fb.iter().map(|b| b.count_ones()).sum()
    }
}

impl Default for Display {
    fn default() -> Self {
        Self::new()
    }
}

/// Greedy word wrap into at most `rows` lines of `cols` characters. False if it does not fit.
fn wrap<const N: usize>(
    text: &str,
    cols: usize,
    rows: usize,
    lines: &mut heapless::Vec<heapless::String<32>, N>,
) -> bool {
    let mut line: heapless::String<32> = heapless::String::new();
    for word in text.split_whitespace() {
        if word.len() > cols {
            return false;
        }
        let needed = if line.is_empty() { word.len() } else { line.len() + 1 + word.len() };
        if needed > cols && lines.push(core::mem::take(&mut line)).is_err() {
            return false;
        }
        if !line.is_empty() {
            let _ = line.push(' ');
        }
        let _ = line.push_str(word);
    }
    if !line.is_empty() && lines.push(line).is_err() {
        return false;
    }
    lines.len() <= rows
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::String as StdString;

    /// Renders a region as text, '#' for lit pixels.
    fn ascii(d: &Display, x0: usize, y0: usize, w: usize, h: usize) -> StdString {
        let mut s = StdString::new();
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                s.push(if d.pixel(x, y) { '#' } else { '.' });
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn pixels_use_the_controller_layout() {
        let mut d = Display::new();
        d.set(3, 10, true); // page 1, bit 2
        assert_eq!(d.fb[WIDTH + 3], 0b100);
        assert!(d.pixel(3, 10));
        d.set(3, 10, false);
        assert_eq!(d.count(), 0);
        d.set(-1, 5, true); // off-screen is ignored
        d.set(128, 5, true);
        assert_eq!(d.count(), 0);
    }

    #[test]
    fn glyphs_look_right() {
        let mut d = Display::new();
        d.text(0, 0, "A1", 1);
        assert_eq!(
            ascii(&d, 0, 0, 11, 7),
            "\
.###....#..
#...#..##..
#...#...#..
#...#...#..
#####...#..
#...#...#..
#...#..###.
"
        );
    }

    #[test]
    fn scaled_text_and_centering() {
        let mut d = Display::new();
        d.char(0, 0, '|', 2);
        assert!(d.pixel(4, 0) && d.pixel(5, 13) && !d.pixel(3, 0) && !d.pixel(4, 14));
        let mut d = Display::new();
        d.text_centered(0, "I", 1); // 5 wide -> starts at (128 - 5) / 2 = 61
        assert!(d.pixel(63, 0) && !d.pixel(60, 0));
    }

    #[test]
    fn text_fit_picks_the_biggest_scale() {
        let mut d = Display::new();
        assert_eq!(d.text_fit("Hi"), 4);
        let mut d = Display::new();
        assert_eq!(d.text_fit("hello world"), 4); // 2 lines of 5 columns
        let mut d = Display::new();
        assert_eq!(d.text_fit("hello there!"), 3); // "there!" needs 6 columns
        let mut d = Display::new();
        assert_eq!(d.text_fit("the quick brown fox jumps over the lazy dog"), 1);
    }

    #[test]
    fn wrapping() {
        let mut lines: heapless::Vec<heapless::String<32>, 8> = heapless::Vec::new();
        assert!(wrap("aa bb cc", 5, 2, &mut lines));
        assert_eq!(lines, ["aa bb", "cc"]);
        let mut lines: heapless::Vec<heapless::String<32>, 8> = heapless::Vec::new();
        assert!(!wrap("toolongword", 5, 2, &mut lines));
    }

    #[test]
    fn inverse_video_keeps_the_text_visible() {
        let mut d = Display::new();
        d.text(1, 1, "I", 1);
        d.invert_rect(0, 0, 10, 10);
        assert!(d.pixel(0, 0)); // background lit
        assert!(!d.pixel(3, 1)); // the stroke of the 'I' is dark
        assert!(!d.pixel(10, 0)); // outside the rectangle untouched
        assert_eq!(d.count(), 100 - 7 - 2 * 2); // 'I': 7-pixel stem plus 2-pixel serifs top and bottom
    }

    #[test]
    fn life_blinker() {
        let mut d = Display::new();
        for x in 10..13 {
            d.set(x, 20, true);
        }
        d.life_step();
        assert!(d.pixel(11, 19) && d.pixel(11, 20) && d.pixel(11, 21));
        assert_eq!(d.count(), 3);
        d.life_step();
        assert!(d.pixel(10, 20) && d.pixel(12, 20));
    }
}
