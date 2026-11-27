//! Terminal output helpers on top of any `embedded_io_async::Write`.

use core::fmt::{self, Write as _};

use embedded_io_async::Write;
use heapless::String;

/// The most output `>` and `>>` collect from one command.
pub const CAPTURE_MAX: usize = 4096;

/// Where captured output goes: one buffer per level of nesting (`run x.ynk > a` with
/// `temp > b` inside it). They live in a static (see `shell::Statics`) and stay put: moving
/// 4 KB values around costs that much stack every time.
pub struct CaptureBufs {
    text: [heapless::Vec<u8, CAPTURE_MAX>; 2],
    /// More was printed into that buffer than fits.
    overflowed: [bool; 2],
}

impl CaptureBufs {
    pub const fn new() -> Self {
        Self { text: [heapless::Vec::new(), heapless::Vec::new()], overflowed: [false; 2] }
    }
}

pub struct Term<W: Write> {
    tx: W,
    bufs: &'static mut CaptureBufs,
    /// Captures running: 0 (output goes to the terminal), 1 or 2.
    depth: usize,
    /// Plain text only: colours, cursor moves and the title are dropped, `\r` too.
    escape: Escape,
}

#[derive(Clone, Copy, PartialEq)]
enum Escape {
    None,
    /// After ESC.
    Start,
    /// In `ESC [ ...`, which ends with a letter.
    Csi,
    /// In `ESC ] ...` (the window title), which ends with BEL.
    Osc,
}

/// Feeds terminal output into `text`, keeping the text and leaving out the escapes.
fn feed(escape: &mut Escape, text: &mut heapless::Vec<u8, CAPTURE_MAX>, overflowed: &mut bool, bytes: &[u8]) {
    for &b in bytes {
        *escape = match (*escape, b) {
            (Escape::None, 0x1b) => Escape::Start,
            (Escape::None, b'\r' | 0x07) => Escape::None,
            (Escape::None, _) => {
                if text.push(b).is_err() {
                    *overflowed = true;
                }
                Escape::None
            }
            (Escape::Start, b'[') => Escape::Csi,
            (Escape::Start, b']') => Escape::Osc,
            (Escape::Start, _) => Escape::None,
            (Escape::Csi, 0x40..=0x7e) => Escape::None,
            (Escape::Csi, _) => Escape::Csi,
            (Escape::Osc, 0x07) => Escape::None,
            (Escape::Osc, _) => Escape::Osc,
        };
    }
}

impl<W: Write> Term<W> {
    pub fn new(tx: W, bufs: &'static mut CaptureBufs) -> Self {
        Self { tx, bufs, depth: 0, escape: Escape::None }
    }

    /// From now on, output is collected instead of sent, until [`Self::end_capture`]. One
    /// capture can nest inside another; a third shares the second's buffer.
    pub fn start_capture(&mut self) {
        let level = self.depth.min(1);
        self.bufs.text[level].clear();
        self.bufs.overflowed[level] = false;
        self.depth = (self.depth + 1).min(2);
        self.escape = Escape::None;
    }

    /// Stops the innermost capture. Returns which buffer it filled (for [`Self::captured`])
    /// and whether some of the output did not fit.
    pub fn end_capture(&mut self) -> Option<(usize, bool)> {
        if self.depth == 0 {
            return None;
        }
        self.depth -= 1;
        Some((self.depth, self.bufs.overflowed[self.depth]))
    }

    /// The text a capture collected.
    pub fn captured(&self, level: usize) -> &[u8] {
        &self.bufs.text[level]
    }

    pub async fn write_bytes(&mut self, bytes: &[u8]) {
        if self.depth > 0 {
            let level = self.depth - 1;
            feed(&mut self.escape, &mut self.bufs.text[level], &mut self.bufs.overflowed[level], bytes);
            return;
        }
        // Some drivers (DMA based) reject zero-length transfers, so never send one.
        if !bytes.is_empty() {
            let _ = self.tx.write_all(bytes).await;
        }
    }

    pub async fn print(&mut self, s: &str) {
        self.write_bytes(s.as_bytes()).await;
    }

    pub async fn print_lines(&mut self, lines: &[&str]) {
        for line in lines {
            self.print(line).await;
            self.print("\r\n").await;
        }
    }

    pub async fn printf(&mut self, args: fmt::Arguments<'_>) {
        let mut s: String<256> = String::new();
        let _ = s.write_fmt(args);
        self.print(&s).await;
    }

    pub async fn flush(&mut self) {
        if self.depth == 0 {
            let _ = self.tx.flush().await;
        }
    }
}

/// `out!(term, "fmt", args...)` formats and prints, like `write!` for the terminal.
macro_rules! out {
    ($term:expr, $($arg:tt)*) => {
        $term.printf(format_args!($($arg)*)).await
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_keeps_plain_text() {
        let (mut escape, mut text, mut overflowed) = (Escape::None, heapless::Vec::new(), false);
        feed(&mut escape, &mut text, &mut overflowed, b"\x1b[1;31mhot\x1b[0m 30 C\r\n\x1b]0;title\x07next\x1b[K\r\n");
        assert_eq!(text.as_slice(), b"hot 30 C\nnext\n");
        assert!(!overflowed);
    }
}
