//! Whole-screen pictures for the OLED, each a pure function of its data so it can be unit
//! tested (and previewed in a test as ASCII art). The shell gathers the data and flushes.

use core::f32::consts::PI;
use core::fmt::Write;

use heapless::String;

use crate::oled::{CHAR_W, Display, HEIGHT, WIDTH};

/// Draws `text` with the bar behind it in inverse video, across the top 10 rows.
fn header(d: &mut Display, text: &str) {
    d.text(2, 1, text, 1);
    d.invert_rect(0, 0, WIDTH as i32, 10);
}

/// Draws `text` right-aligned so that it ends at `right` (exclusive).
fn text_right(d: &mut Display, right: i32, y: i32, text: &str) {
    let w = text.chars().count() as i32 * CHAR_W as i32 - 1;
    d.text(right - w, y, text, 1);
}

// ---------------------------------------------------------------------------
// top
// ---------------------------------------------------------------------------

/// One line of the `top` screen. `per_sec` is `None` before there is an interval to measure.
pub struct TopRow<'a> {
    pub name: &'a str,
    pub per_sec: Option<u32>,
}

/// Blocks in a task's activity bar: 1, 2, 4, 8, ... wakeups per second each add one,
/// like the terminal version, so a task waking 1000 times a second does not drown the rest.
pub fn activity_blocks(per_sec: u32) -> u32 {
    32 - per_sec.leading_zeros()
}

/// The `top` screen: uptime in the header, then one row per task with its name, an
/// activity bar and its wakeups per second.
pub fn top(d: &mut Display, uptime_s: u64, rows: &[TopRow]) {
    d.clear();
    let mut line: String<24> = String::new();
    let _ = write!(line, "top  up {:02}:{:02}:{:02}", uptime_s / 3600, (uptime_s / 60) % 60, uptime_s % 60);
    header(d, &line);
    // Columns: name (8 characters), bar (11 blocks of 5 pixels), rate (4 characters).
    const BAR_X: i32 = 48;
    const BLOCK: i32 = 5;
    for (i, row) in rows.iter().take(5).enumerate() {
        let y = 13 + i as i32 * 10;
        d.text(0, y, row.name.get(..8).unwrap_or(row.name), 1);
        line.clear();
        match row.per_sec {
            Some(rate) => {
                for b in 0..activity_blocks(rate).min(11) as i32 {
                    d.fill_rect(BAR_X + b * BLOCK, y, BLOCK - 1, 7, true);
                }
                let _ = write!(line, "{}", rate.min(9999));
            }
            None => {
                let _ = line.push('-');
            }
        }
        text_right(d, WIDTH as i32, y, &line);
    }
}

// ---------------------------------------------------------------------------
// mandel
// ---------------------------------------------------------------------------

pub const MANDEL_MAX_ITER: u32 = 64;

/// 4x4 ordered-dither thresholds (0..16): turns a brightness into a dot pattern.
const BAYER: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

/// Renders rows `rows` of the Mandelbrot set into the framebuffer (the caller yields to the
/// executor between batches). Inside the set is solid; outside, the closer a point comes to
/// the set (the more iterations it takes to escape), the denser the dither. Returns the
/// number of iterations computed, for the speed report.
pub fn mandel_rows(d: &mut Display, rows: core::ops::Range<usize>) -> u64 {
    // Square pixels: 64 rows span -1.1..1.1, which centres the set horizontally.
    const SCALE: f32 = 2.2 / HEIGHT as f32;
    const LEFT: f32 = -0.75 - SCALE * WIDTH as f32 / 2.0;
    let mut iterations = 0u64;
    for y in rows {
        let ci = -1.1 + SCALE * (y as f32 + 0.5);
        for x in 0..WIDTH {
            let cr = LEFT + SCALE * (x as f32 + 0.5);
            let (mut zr, mut zi, mut n) = (0.0f32, 0.0f32, 0u32);
            while n < MANDEL_MAX_ITER && zr * zr + zi * zi <= 4.0 {
                let t = zr * zr - zi * zi + cr;
                zi = 2.0 * zr * zi + ci;
                zr = t;
                n += 1;
            }
            iterations += n as u64;
            let on = if n == MANDEL_MAX_ITER {
                true
            } else {
                // Escape bands: 2-3 iterations is sparse, 16+ is nearly solid.
                let level = (n.saturating_sub(2) * 16 / 14).min(16) as u8;
                level > BAYER[y % 4][x % 4]
            };
            d.set(x as i32, y as i32, on);
        }
    }
    iterations
}

// ---------------------------------------------------------------------------
// gauge
// ---------------------------------------------------------------------------

/// sin(x) for x in [-PI, PI], Bhaskara I's approximation: error under 0.002, plenty for
/// pixels, and no libm needed.
fn sin(x: f32) -> f32 {
    let (sign, x) = if x < 0.0 { (-1.0, -x) } else { (1.0, x) };
    let p = x * (PI - x);
    sign * 16.0 * p / (5.0 * PI * PI - 4.0 * p)
}

/// cos(x) for x in [0, PI].
fn cos(x: f32) -> f32 {
    sin(PI / 2.0 - x)
}

/// A half-circle dial: `fraction` 0.0 points the needle left, 1.0 right. `value` goes big
/// under the dial's centre, `label` (keep it to 6 characters) small in the bottom left.
pub fn gauge(d: &mut Display, label: &str, value: &str, fraction: f32) {
    d.clear();
    let (cx, cy, r) = (64.0f32, 40.0f32, 38.0f32);
    let point = |angle: f32, radius: f32| ((cx + radius * cos(angle)) as i32, (cy - radius * sin(angle)) as i32);
    // The arc, one step per degree.
    for step in 0..=180 {
        let (x, y) = point(step as f32 * PI / 180.0, r);
        d.set(x, y, true);
    }
    // Ticks every 10 %, longer at 0, 50 and 100 %.
    for i in 0..=10 {
        let angle = PI - i as f32 * PI / 10.0;
        let inner = if i % 5 == 0 { r - 7.0 } else { r - 4.0 };
        let (x0, y0) = point(angle, inner);
        let (x1, y1) = point(angle, r);
        d.line(x0, y0, x1, y1);
    }
    let angle = PI - fraction.clamp(0.0, 1.0) * PI;
    let (nx, ny) = point(angle, r - 10.0);
    d.line(cx as i32, cy as i32, nx, ny);
    d.fill_rect(cx as i32 - 2, cy as i32 - 2, 5, 5, true); // the hub
    d.text(0, HEIGHT as i32 - 8, label, 1);
    let w = value.chars().count() as i32 * CHAR_W as i32 * 2 - 2;
    d.text((WIDTH as i32 - w) / 2, HEIGHT as i32 - 15, value, 2);
}

// ---------------------------------------------------------------------------
// env
// ---------------------------------------------------------------------------

/// A room sensor reading: `title` small on top, the temperature big, then humidity and
/// pressure when the sensor has them.
pub fn env(d: &mut Display, title: &str, reading: &crate::env::EnvReading) {
    d.clear();
    let centred = |d: &mut Display, y: i32, text: &str, scale: i32| {
        let w = text.chars().count() as i32 * CHAR_W as i32 * scale - scale;
        d.text((WIDTH as i32 - w) / 2, y, text, scale);
    };
    centred(d, 0, title, 1);
    let mut line: String<16> = String::new();
    let _ = write!(line, "{:.1}{}C", reading.temp_c, crate::oled::DEGREE);
    centred(d, 11, &line, 3);
    if let Some(rh) = reading.humidity {
        line.clear();
        let _ = write!(line, "{rh:.0}% RH");
        centred(d, 36, &line, 2);
    }
    if let Some(hpa) = reading.pressure_hpa {
        line.clear();
        let _ = write!(line, "{hpa:.1} hPa");
        centred(d, 56, &line, 1);
    }
}

// ---------------------------------------------------------------------------
// plot
// ---------------------------------------------------------------------------

/// The last `WIDTH` samples of something, one per screen column, for [`plot`].
pub struct Series {
    values: [f32; WIDTH],
    /// Index the next value goes to.
    next: usize,
    len: usize,
}

impl Series {
    pub const fn new() -> Self {
        Self { values: [0.0; WIDTH], next: 0, len: 0 }
    }

    pub fn push(&mut self, value: f32) {
        self.values[self.next] = value;
        self.next = (self.next + 1) % WIDTH;
        self.len = (self.len + 1).min(WIDTH);
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// The stored values, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = f32> + '_ {
        let start = (self.next + WIDTH - self.len) % WIDTH;
        (0..self.len).map(move |i| self.values[(start + i) % WIDTH])
    }

    pub fn last(&self) -> Option<f32> {
        (self.len > 0).then(|| self.values[(self.next + WIDTH - 1) % WIDTH])
    }

    /// Smallest and largest stored value.
    pub fn min_max(&self) -> Option<(f32, f32)> {
        self.iter().fold(None, |acc, v| match acc {
            None => Some((v, v)),
            Some((lo, hi)) => Some((lo.min(v), hi.max(v))),
        })
    }
}

impl Default for Series {
    fn default() -> Self {
        Self::new()
    }
}

/// How [`plot`] maps values to rows.
#[derive(Clone, Copy)]
pub enum Scale {
    /// Always this range (a potentiometer's 0..3300 mV).
    Fixed(f32, f32),
    /// Fit the data, but never zoom in closer than this span, so sensor noise stays flat.
    Auto { min_span: f32 },
}

/// Top and bottom rows of the chart area.
const PLOT_TOP: i32 = 12;
const PLOT_BOTTOM: i32 = 54;

/// The range `series` is drawn with.
pub fn plot_range(series: &Series, scale: Scale) -> (f32, f32) {
    match scale {
        Scale::Fixed(lo, hi) => (lo, hi),
        Scale::Auto { min_span } => {
            let (lo, hi) = series.min_max().unwrap_or((0.0, 0.0));
            if hi - lo >= min_span {
                (lo, hi)
            } else {
                let mid = (lo + hi) / 2.0;
                (mid - min_span / 2.0, mid + min_span / 2.0)
            }
        }
    }
}

/// The row a value is drawn at.
fn plot_row(value: f32, (lo, hi): (f32, f32)) -> i32 {
    let fraction = ((value - lo) / (hi - lo)).clamp(0.0, 1.0);
    PLOT_BOTTOM - (fraction * (PLOT_BOTTOM - PLOT_TOP) as f32 + 0.5) as i32
}

/// A scrolling line chart: `title` and the latest value in the header, the newest sample at
/// the right edge, and the lowest and highest value on screen along the bottom. Values are
/// printed with `decimals` digits after the point and `unit` after them.
pub fn plot(d: &mut Display, title: &str, series: &Series, scale: Scale, decimals: usize, unit: &str) {
    d.clear();
    let mut line: String<24> = String::new();
    d.text(2, 1, title, 1);
    if let Some(v) = series.last() {
        let _ = write!(line, "{v:.decimals$}{unit}");
        text_right(d, WIDTH as i32 - 2, 1, &line);
    }
    d.invert_rect(0, 0, WIDTH as i32, 10);
    // Dotted lines along the top and bottom of the chart.
    for x in (0..WIDTH as i32).step_by(4) {
        d.set(x, PLOT_TOP, true);
        d.set(x, PLOT_BOTTOM, true);
    }
    let range = plot_range(series, scale);
    let x0 = (WIDTH - series.len()) as i32;
    let mut prev: Option<(i32, i32)> = None;
    for (i, v) in series.iter().enumerate() {
        let (x, y) = (x0 + i as i32, plot_row(v, range));
        match prev {
            Some((px, py)) => d.line(px, py, x, y),
            None => d.set(x, y, true),
        }
        prev = Some((x, y));
    }
    if let Some((lo, hi)) = series.min_max() {
        line.clear();
        let _ = write!(line, "lo {lo:.decimals$}");
        d.text(0, HEIGHT as i32 - 7, &line, 1);
        line.clear();
        let _ = write!(line, "hi {hi:.decimals$}");
        text_right(d, WIDTH as i32, HEIGHT as i32 - 7, &line);
    }
}

// ---------------------------------------------------------------------------
// scope
// ---------------------------------------------------------------------------

/// Top and bottom rows of the scope's trace (0 V at the bottom, `full_mv` at the top).
const SCOPE_TOP: i32 = 11;
const SCOPE_BOTTOM: i32 = 63;

/// An oscilloscope screen: `window` (one sample per column) over a grid of 16-pixel
/// divisions, the trigger level as a tick on the left, and `header` in the bar on top.
pub fn scope(d: &mut Display, header: &str, window: &[u16], full_mv: u16, level_mv: Option<u16>) {
    d.clear();
    d.text(1, 1, header, 1);
    d.invert_rect(0, 0, WIDTH as i32, 10);
    let row = |mv: u16| {
        let fraction = (mv as f32 / full_mv as f32).clamp(0.0, 1.0);
        SCOPE_BOTTOM - (fraction * (SCOPE_BOTTOM - SCOPE_TOP) as f32 + 0.5) as i32
    };
    // Grid: dots every 4 pixels on the division lines, 4 divisions high.
    for x in (0..WIDTH as i32).step_by(16) {
        for y in (SCOPE_TOP..=SCOPE_BOTTOM).step_by(4) {
            d.set(x, y, true);
        }
    }
    for i in 0..=4 {
        let y = SCOPE_BOTTOM - i * (SCOPE_BOTTOM - SCOPE_TOP) / 4;
        for x in (0..WIDTH as i32).step_by(4) {
            d.set(x, y, true);
        }
    }
    if let Some(level) = level_mv {
        let y = row(level);
        d.line(0, y, 3, y);
    }
    let mut prev: Option<(i32, i32)> = None;
    for (x, &mv) in window.iter().take(WIDTH).enumerate() {
        let (x, y) = (x as i32, row(mv));
        match prev {
            // Vertical edges are drawn in full, so a square wave looks square.
            Some((px, py)) => d.line(px, py, x, y),
            None => d.set(x, y, true),
        }
        prev = Some((x, y));
    }
}

// ---------------------------------------------------------------------------
// menu
// ---------------------------------------------------------------------------

/// Rows of items that fit under the header.
pub const MENU_ROWS: usize = 5;

/// A list with the `selected` item in inverse video, scrolled so that it is on screen.
pub fn menu(d: &mut Display, title: &str, items: &[&str], selected: usize) {
    d.clear();
    header(d, title);
    let first = selected.saturating_sub(MENU_ROWS - 1);
    for (row, (i, item)) in items.iter().enumerate().skip(first).take(MENU_ROWS).enumerate() {
        let y = 13 + row as i32 * 10;
        d.text(4, y, item, 1);
        if i == selected {
            d.invert_rect(0, y - 1, WIDTH as i32, 9);
        }
    }
    // A scroll bar when not everything fits.
    if items.len() > MENU_ROWS {
        let track = (HEIGHT - 12) as i32;
        let h = (track * MENU_ROWS as i32 / items.len() as i32).max(3);
        let y = 12 + (track - h) * first as i32 / (items.len() - MENU_ROWS) as i32;
        d.fill_rect(WIDTH as i32 - 2, y, 2, h, true);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    /// The whole screen as ASCII art, for eyeballing with `--nocapture`.
    #[allow(dead_code)]
    fn ascii(d: &Display) -> std::string::String {
        let mut s = std::string::String::new();
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                s.push(if d.pixel(x, y) { '#' } else { '.' });
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn trig_is_close_enough() {
        for i in 0..=36 {
            let x = -PI + i as f32 * PI / 18.0;
            let exact = std::primitive::f32::sin(x);
            assert!((sin(x) - exact).abs() < 0.002, "sin({x})");
        }
        assert!((cos(0.0) - 1.0).abs() < 0.002 && (cos(PI) + 1.0).abs() < 0.002);
    }

    #[test]
    fn activity_is_logarithmic() {
        assert_eq!(activity_blocks(0), 0);
        assert_eq!(activity_blocks(1), 1);
        assert_eq!(activity_blocks(3), 2);
        assert_eq!(activity_blocks(1000), 10);
    }

    #[test]
    fn top_draws_a_row_per_task() {
        let mut d = Display::new();
        let rows = [TopRow { name: "shell", per_sec: Some(0) }, TopRow { name: "led", per_sec: Some(1000) }];
        top(&mut d, 3725, &rows);
        assert!(d.pixel(0, 0)); // inverse header
        // The idle shell has no bar; the busy LED task has ten blocks.
        assert!(!d.pixel(48, 13));
        assert!(d.pixel(48, 23) && d.pixel(48 + 9 * 5, 23) && !d.pixel(48 + 10 * 5, 23));
    }

    #[test]
    fn mandel_is_solid_inside_and_empty_far_out() {
        let mut d = Display::new();
        let n = mandel_rows(&mut d, 0..HEIGHT);
        assert!(n > 0);
        assert!(d.pixel(WIDTH / 2, HEIGHT / 2)); // c = -0.75 + 0i is in the set
        assert!(!d.pixel(0, 0) && !d.pixel(WIDTH - 1, HEIGHT - 1));
    }

    #[test]
    fn gauge_needle_follows_the_value() {
        let mut left = Display::new();
        gauge(&mut left, "", "", 0.0);
        let mut right = Display::new();
        gauge(&mut right, "", "", 1.0);
        // Needle along the horizontal radius: left half lit for 0 %, right half for 100 %.
        assert!(left.pixel(45, 40) && !left.pixel(83, 40));
        assert!(right.pixel(83, 40) && !right.pixel(45, 40));
    }

    #[test]
    fn series_keeps_the_newest_in_order() {
        let mut s = Series::new();
        assert!(s.last().is_none() && s.min_max().is_none());
        for i in 0..WIDTH + 3 {
            s.push(i as f32);
        }
        assert_eq!(s.len(), WIDTH);
        assert_eq!(s.iter().next(), Some(3.0));
        assert_eq!(s.last(), Some((WIDTH + 2) as f32));
        assert_eq!(s.min_max(), Some((3.0, (WIDTH + 2) as f32)));
    }

    #[test]
    fn plot_scales_and_scrolls() {
        let mut s = Series::new();
        s.push(0.0);
        s.push(100.0);
        let mut d = Display::new();
        plot(&mut d, "x", &s, Scale::Fixed(0.0, 100.0), 0, "");
        // Newest sample at the right edge, at the top; the one before at the bottom.
        assert!(d.pixel(WIDTH - 1, PLOT_TOP as usize));
        assert!(d.pixel(WIDTH - 2, PLOT_BOTTOM as usize));
        // A flat signal sits in the middle when autoscaled, not along an edge.
        let mut flat = Series::new();
        flat.push(21.5);
        assert_eq!(plot_range(&flat, Scale::Auto { min_span: 2.0 }), (20.5, 22.5));
        assert_eq!(plot_row(21.5, (20.5, 22.5)), (PLOT_TOP + PLOT_BOTTOM) / 2);
    }

    #[test]
    fn menu_scrolls_to_the_selection() {
        let items = ["a", "b", "c", "d", "e", "f", "g"];
        let mut d = Display::new();
        menu(&mut d, "menu", &items, 0);
        assert!(d.pixel(0, 12)); // first row highlighted
        let mut d = Display::new();
        menu(&mut d, "menu", &items, 6);
        // The last item is now the bottom row, highlighted.
        assert!(d.pixel(0, 12 + 4 * 10) && !d.pixel(0, 12));
    }

    #[test]
    #[ignore = "prints the screens; run with --ignored --nocapture"]
    fn preview() {
        let mut d = Display::new();
        let rows = [
            TopRow { name: "shell", per_sec: Some(3) },
            TopRow { name: "led", per_sec: Some(40) },
            TopRow { name: "button", per_sec: Some(0) },
            TopRow { name: "sensor", per_sec: Some(1) },
            TopRow { name: "watchdog", per_sec: Some(2) },
        ];
        top(&mut d, 3725, &rows);
        std::println!("{}", ascii(&d));
        let mut d = Display::new();
        mandel_rows(&mut d, 0..HEIGHT);
        std::println!("{}", ascii(&d));
        let mut d = Display::new();
        gauge(&mut d, "PA0 knob", "62%", 0.62);
        std::println!("{}", ascii(&d));
        let mut d = Display::new();
        let r = crate::env::EnvReading { temp_c: 22.43, humidity: Some(45.2), pressure_hpa: Some(1013.25) };
        env(&mut d, "BME280 at 0x76", &r);
        std::println!("{}", ascii(&d));
        let mut d = Display::new();
        let mut s = Series::new();
        for i in 0..90 {
            s.push(22.0 + (i as f32 / 9.0).sin() * 0.8 + i as f32 * 0.01);
        }
        plot(&mut d, "temp", &s, Scale::Auto { min_span: 2.0 }, 1, " C");
        std::println!("{}", ascii(&d));
        let mut d = Display::new();
        let wave: std::vec::Vec<u16> = (0..128).map(|i| if (i + 8) % 40 < 10 { 3300 } else { 0 }).collect();
        scope(&mut d, "1.00kHz 25% 3.3V 25u", &wave, 3300, Some(1650));
        std::println!("{}", ascii(&d));
        let mut d = Display::new();
        menu(&mut d, "tap: next  hold: ok", &["top", "mandel", "plot temp", "plot A0", "life", "dash", "exit"], 5);
        std::println!("{}", ascii(&d));
    }
}
