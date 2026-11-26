//! Terminal output helpers on top of any `embedded_io_async::Write`.

use core::fmt::{self, Write as _};

use embedded_io_async::Write;
use heapless::String;

pub struct Term<W: Write> {
    tx: W,
}

impl<W: Write> Term<W> {
    pub fn new(tx: W) -> Self {
        Self { tx }
    }

    pub async fn write_bytes(&mut self, bytes: &[u8]) {
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
        let _ = self.tx.flush().await;
    }
}

/// `out!(term, "fmt", args...)` formats and prints, like `write!` for the terminal.
macro_rules! out {
    ($term:expr, $($arg:tt)*) => {
        $term.printf(format_args!($($arg)*)).await
    };
}
