//! Oscilloscope maths for `scope`: triggering, frequency, duty cycle and peak-to-peak, all on
//! a captured buffer of millivolt samples, so it can be tested without hardware.

/// Samples shown on screen (one per OLED column).
pub const WINDOW: usize = 128;
/// Samples captured per frame: twice the window, so that a trigger point can be found with a
/// whole window after it.
pub const CAPTURE: usize = 2 * WINDOW;
/// Where the trigger point sits in the window: one division (16 samples) in from the left.
pub const PRE_TRIGGER: usize = 16;
/// Time bases, in microseconds between samples, for `+` and `-`.
pub const INTERVALS_US: [u32; 11] = [5, 10, 20, 50, 100, 200, 500, 1000, 2000, 5000, 10000];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Analysis {
    pub min_mv: u16,
    pub max_mv: u16,
    /// The level a rising edge has to cross to trigger.
    pub level_mv: u16,
    /// Index of the first sample of the window to show: `PRE_TRIGGER` before a rising edge, or
    /// `None` when there was no edge to trigger on (the window then starts at 0).
    pub start: Option<usize>,
    pub freq_hz: Option<f32>,
    /// Fraction of each period spent above the level, 0.0 to 1.0.
    pub duty: Option<f32>,
}

/// A signal whose span is smaller than this is flat, as far as triggering goes: noise.
const MIN_SPAN_MV: u16 = 100;

/// Analyses `samples` taken `interval_ns` apart. `level_mv` is the trigger level; `None`
/// puts it halfway between the lowest and highest sample.
pub fn analyze(samples: &[u16], interval_ns: u32, level_mv: Option<u16>) -> Analysis {
    let min_mv = samples.iter().copied().min().unwrap_or(0);
    let max_mv = samples.iter().copied().max().unwrap_or(0);
    let level = level_mv.unwrap_or(((min_mv as u32 + max_mv as u32) / 2) as u16);
    let mut a = Analysis { min_mv, max_mv, level_mv: level, start: None, freq_hz: None, duty: None };
    if max_mv - min_mv < MIN_SPAN_MV {
        return a;
    }
    // Hysteresis: a rising edge counts once the signal has been clearly below the level and
    // then reaches it, so noise around the level does not trigger again and again.
    let hysteresis = ((max_mv - min_mv) / 10).max(20);
    let low = level.saturating_sub(hysteresis);
    let mut armed = false;
    let mut first: Option<usize> = None;
    let mut last = 0;
    let mut edges = 0u32;
    for (i, &v) in samples.iter().enumerate() {
        if v < low {
            armed = true;
        } else if armed && v >= level {
            armed = false;
            edges += 1;
            if first.is_none() {
                first = Some(i);
            }
            last = i;
            if a.start.is_none() && i >= PRE_TRIGGER && i - PRE_TRIGGER + WINDOW <= samples.len() {
                a.start = Some(i - PRE_TRIGGER);
            }
        }
    }
    if let (Some(first), true) = (first, edges >= 2) {
        let period_samples = (last - first) as f32 / (edges - 1) as f32;
        a.freq_hz = Some(1e9 / (period_samples * interval_ns as f32));
        let above = samples[first..last].iter().filter(|&&v| v >= level).count();
        a.duty = Some(above as f32 / (last - first) as f32);
    }
    a
}

/// The next time base up (`faster` false) or down the list from `interval_us`.
pub fn step_interval(interval_us: u32, faster: bool) -> u32 {
    let i = INTERVALS_US.iter().position(|&v| v >= interval_us).unwrap_or(INTERVALS_US.len() - 1);
    let i = if faster { i.saturating_sub(1) } else { (i + 1).min(INTERVALS_US.len() - 1) };
    INTERVALS_US[i]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A square wave: `period` samples, `high` of them at 3300 mV, starting at `phase`.
    fn square(period: usize, high: usize, phase: usize) -> [u16; CAPTURE] {
        let mut s = [0u16; CAPTURE];
        for (i, v) in s.iter_mut().enumerate() {
            *v = if (i + phase) % period < high { 3300 } else { 0 };
        }
        s
    }

    #[test]
    fn square_wave_frequency_and_duty() {
        // 40 samples per period at 25 us a sample: 1 ms, 1 kHz; high for a quarter.
        let s = square(40, 10, 7);
        let a = analyze(&s, 25_000, None);
        assert_eq!((a.min_mv, a.max_mv, a.level_mv), (0, 3300, 1650));
        let f = a.freq_hz.unwrap();
        assert!((f - 1000.0).abs() < 1.0, "{f}");
        assert!((a.duty.unwrap() - 0.25).abs() < 0.01);
        // The window starts PRE_TRIGGER before a rising edge.
        let start = a.start.unwrap();
        assert_eq!(s[start + PRE_TRIGGER - 1], 0);
        assert_eq!(s[start + PRE_TRIGGER], 3300);
    }

    #[test]
    fn flat_or_noisy_signals_do_not_trigger() {
        let mut s = [1650u16; CAPTURE];
        for (i, v) in s.iter_mut().enumerate() {
            *v += (i % 3) as u16 * 20; // 40 mV of noise
        }
        let a = analyze(&s, 10_000, None);
        assert_eq!((a.start, a.freq_hz), (None, None));
    }

    #[test]
    fn noise_on_the_edges_counts_once() {
        // A slow square wave with a glitch right after each rising edge.
        let mut s = square(64, 32, 0);
        for i in (0..CAPTURE).step_by(64) {
            if i + 1 < CAPTURE {
                s[i + 1] = 1600; // dips just under the level, not under the hysteresis
            }
        }
        let a = analyze(&s, 1000, None);
        let f = a.freq_hz.unwrap();
        assert!((f - 1e9 / (64.0 * 1000.0)).abs() < 1.0, "{f}");
    }

    #[test]
    fn a_fixed_level_is_used() {
        let s = square(40, 20, 0);
        assert_eq!(analyze(&s, 1000, Some(500)).level_mv, 500);
    }

    #[test]
    fn time_bases_step_through_the_list() {
        assert_eq!(step_interval(20, true), 10);
        assert_eq!(step_interval(20, false), 50);
        assert_eq!(step_interval(5, true), 5);
        assert_eq!(step_interval(10000, false), 10000);
        assert_eq!(step_interval(30, false), 100); // an odd value snaps to the list
    }
}
