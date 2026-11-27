//! Terminal output helpers on top of any `embedded_io_async::Write`.

use core::fmt::{self, Write as _};

use embedded_io_async::Write;
use heapless::String;

/// The most output `>` and `>>` collect from one command.
pub const CAPTURE_MAX: usize = 4096;

pub struct Term<W: Write> {
    tx: W,
    /// While a command's output goes to a file instead: the text so far.
    capture: Option<Capture>,
}

/// Output being collected for a file: plain text, without the colours and cursor moves
/// meant for the terminal, and with `\n` line ends.
pub struct Capture {
    pub text: heapless::Vec<u8, CAPTURE_MAX>,
    /// More was printed than fits.
    pub overflowed: bool,
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

impl Capture {
    fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.escape = match (self.escape, b) {
                (Escape::None, 0x1b) => Escape::Start,
                (Escape::None, b'\r' | 0x07) => Escape::None,
                (Escape::None, _) => {
                    if self.text.push(b).is_err() {
                        self.overflowed = true;
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
}

impl<W: Write> Term<W> {
    pub fn new(tx: W) -> Self {
        Self { tx, capture: None }
    }

    /// From now on, output is collected instead of sent, until [`Self::end_capture`].
    pub fn start_capture(&mut self) {
        self.capture = Some(Capture { text: heapless::Vec::new(), overflowed: false, escape: Escape::None });
    }

    pub fn end_capture(&mut self) -> Option<Capture> {
        self.capture.take()
    }

    pub async fn write_bytes(&mut self, bytes: &[u8]) {
        if let Some(capture) = &mut self.capture {
            capture.feed(bytes);
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
        if self.capture.is_none() {
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
        let mut c = Capture { text: heapless::Vec::new(), overflowed: false, escape: Escape::None };
        c.feed(b"\x1b[1;31mhot\x1b[0m 30 C\r\n\x1b]0;title\x07next\x1b[K\r\n");
        assert_eq!(c.text.as_slice(), b"hot 30 C\nnext\n");
        assert!(!c.overflowed);
    }
}
