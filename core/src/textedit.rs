//! `edit`: a full-screen text editor for ember files, in the spirit of nano.
//!
//! [`Text`] is the file being edited and the cursor in it, as plain logic with tests; the
//! shell part below draws it with ANSI escapes and feeds it keys.

use core::fmt::Write as _;
use core::ops::Range;

use embedded_io_async::{Read, Write};
use heapless::String;

use crate::ember;
use crate::files::ember as open_ember;
use crate::keys::{Key, KeyDecoder};
use crate::platform::System;
use crate::shell::Shell;

pub const MAX: usize = ember::MAX_SIZE;

fn is_continuation(b: u8) -> bool {
    b & 0xC0 == 0x80
}

/// A file being edited: its bytes (UTF-8, `\n` line ends) and the cursor, a byte offset that
/// is always on a character boundary.
pub struct Text {
    pub bytes: heapless::Vec<u8, MAX>,
    pub cursor: usize,
    /// The column up and down try to keep, across shorter lines.
    goal: Option<usize>,
    pub modified: bool,
}

impl Text {
    #[cfg(test)]
    pub fn new(content: &[u8]) -> Self {
        let mut text = Self::empty();
        text.load(content);
        text
    }

    pub const fn empty() -> Self {
        Self { bytes: heapless::Vec::new(), cursor: 0, goal: None, modified: false }
    }

    /// Replaces the whole text, cursor at the start.
    pub fn load(&mut self, content: &[u8]) {
        self.bytes.clear();
        // Carriage returns from files made elsewhere would only confuse the screen.
        for &b in content.iter().filter(|&&b| b != b'\r') {
            let _ = self.bytes.push(b);
        }
        self.cursor = 0;
        self.goal = None;
        self.modified = false;
    }

    fn line_start(&self, pos: usize) -> usize {
        self.bytes[..pos].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, pos: usize) -> usize {
        self.bytes[pos..].iter().position(|&b| b == b'\n').map_or(self.bytes.len(), |i| pos + i)
    }

    fn chars(&self, range: Range<usize>) -> usize {
        self.bytes[range].iter().filter(|&&b| !is_continuation(b)).count()
    }

    /// The cursor's line (0-based).
    pub fn line(&self) -> usize {
        self.bytes[..self.cursor].iter().filter(|&&b| b == b'\n').count()
    }

    /// The cursor's column, in characters.
    pub fn col(&self) -> usize {
        self.chars(self.line_start(self.cursor)..self.cursor)
    }

    pub fn line_count(&self) -> usize {
        self.bytes.iter().filter(|&&b| b == b'\n').count() + 1
    }

    /// The bytes of line `n`, without its `\n`.
    pub fn line_range(&self, n: usize) -> Option<Range<usize>> {
        let mut start = 0;
        for _ in 0..n {
            start = self.bytes[start..].iter().position(|&b| b == b'\n').map(|i| start + i + 1)?;
        }
        Some(start..self.line_end(start))
    }

    /// The offset `col` characters into the line starting at `start` (or its end).
    fn at_col(&self, start: usize, col: usize) -> usize {
        let end = self.line_end(start);
        let mut pos = start;
        for _ in 0..col {
            if pos >= end {
                break;
            }
            pos = self.next(pos);
        }
        pos
    }

    fn next(&self, pos: usize) -> usize {
        let mut p = pos + 1;
        while p < self.bytes.len() && is_continuation(self.bytes[p]) {
            p += 1;
        }
        p.min(self.bytes.len())
    }

    fn prev(&self, pos: usize) -> usize {
        let mut p = pos.saturating_sub(1);
        while p > 0 && is_continuation(self.bytes[p]) {
            p -= 1;
        }
        p
    }

    /// Inserts at the cursor. False when the file would outgrow [`MAX`].
    pub fn insert(&mut self, s: &[u8]) -> bool {
        if self.bytes.len() + s.len() > MAX {
            return false;
        }
        for (i, &b) in s.iter().enumerate() {
            let _ = self.bytes.insert(self.cursor + i, b);
        }
        self.cursor += s.len();
        self.goal = None;
        self.modified = true;
        true
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let from = self.prev(self.cursor);
            self.remove(from..self.cursor);
            self.cursor = from;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.bytes.len() {
            let to = self.next(self.cursor);
            self.remove(self.cursor..to);
        }
    }

    fn remove(&mut self, range: Range<usize>) {
        let n = range.len();
        self.bytes.copy_within(range.end.., range.start);
        self.bytes.truncate(self.bytes.len() - n);
        self.goal = None;
        self.modified = true;
    }

    pub fn left(&mut self) {
        self.cursor = self.prev(self.cursor);
        self.goal = None;
    }

    pub fn right(&mut self) {
        if self.cursor < self.bytes.len() {
            self.cursor = self.next(self.cursor);
        }
        self.goal = None;
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start(self.cursor);
        self.goal = None;
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end(self.cursor);
        self.goal = None;
    }

    /// Moves `lines` up (negative) or down, keeping the column where the lines allow.
    pub fn vertical(&mut self, lines: isize) {
        let goal = *self.goal.get_or_insert(self.col());
        let target = (self.line() as isize + lines).clamp(0, self.line_count() as isize - 1) as usize;
        if let Some(range) = self.line_range(target) {
            self.cursor = self.at_col(range.start, goal);
        }
        self.goal = Some(goal);
    }
}

/// What the screen shows: the first line and column in view.
#[derive(Clone, Copy, PartialEq, Default)]
struct View {
    top: usize,
    left: usize,
    rows: usize,
    cols: usize,
}

impl View {
    /// Scrolls just enough to keep the cursor in view.
    fn follow(&mut self, line: usize, col: usize) {
        if line < self.top {
            self.top = line;
        } else if line >= self.top + self.rows {
            self.top = line + 1 - self.rows;
        }
        if col < self.left {
            self.left = col;
        } else if col >= self.left + self.cols {
            self.left = col + 1 - self.cols;
        }
    }
}

impl<S: System, R: Read, W: Write> Shell<S, R, W> {
    pub(crate) async fn cmd_edit<'a>(&mut self, mut args: impl Iterator<Item = &'a str>) {
        let Some(name) = args.next() else {
            self.term.print("usage: edit <file>   (a new name makes a new file)\r\n").await;
            return;
        };
        if !ember::valid_name(name) {
            out!(self.term, "error: {}\r\n", ember::FsError::BadName);
            return;
        }
        // The buffers live in the shell, not in this function's future: async call chains
        // copy futures through the stack, and 16 KB of them overflowed it.
        let loaded = match open_ember(&mut self.sys) {
            Ok(mut fs) => match fs.read(name, &mut self.bufs.file).await {
                Ok(n) => Ok(Some(n)),
                Err(ember::FsError::NotFound) => Ok(None),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        let mut message = String::<64>::new();
        match loaded {
            Ok(Some(n)) => self.bufs.text.load(&self.bufs.file[..n]),
            Ok(None) => {
                self.bufs.text.load(b"");
                let _ = message.push_str("new file");
            }
            Err(e) => {
                out!(self.term, "error: {e}\r\n");
                return;
            }
        }
        let (rows, cols) = self.screen_size().await;
        let mut view = View { rows: rows.saturating_sub(1).max(1), cols: cols.max(20), ..View::default() };
        // The alternate screen: the shell's scrollback comes back untouched afterwards.
        self.term.print("\x1b[?1049h\x1b[2J").await;
        let mut keys = KeyDecoder::new();
        let mut drawn: Option<(View, usize, usize)> = None; // view, line count, cursor line
        let mut quit_armed = false;
        let text = &mut self.bufs.text;
        'edit: loop {
            view.follow(text.line(), text.col());
            let state = (view, text.line_count(), text.line());
            // Redraw everything when the view or the line structure changed; else only the
            // cursor's line.
            let full = drawn.is_none_or(|(v, count, _)| v != view || count != state.1);
            let lines = if full { view.top..view.top + view.rows } else { state.2..state.2 + 1 };
            for n in lines {
                draw_line(&mut self.term, text, view, n).await;
            }
            draw_status(&mut self.term, text, view, name, &message).await;
            out!(self.term, "\x1b[{};{}H", text.line() - view.top + 1, text.col() - view.left + 1);
            self.term.flush().await;
            drawn = Some(state);
            message.clear();

            let mut buf = [0u8; 32];
            let n = match self.rx.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => continue,
            };
            for &b in &buf[..n] {
                let Some(key) = keys.feed(b) else { continue };
                let quitting = matches!(key, Key::Char(0x11 | 0x18) | Key::Interrupt);
                match key {
                    Key::Char(0x13 | 0x0f) => {
                        // Ctrl+S (or nano's Ctrl+O): save.
                        let result = match open_ember(&mut self.sys) {
                            Ok(mut fs) => fs.write(name, &text.bytes).await,
                            Err(e) => Err(e),
                        };
                        match result {
                            Ok(()) => {
                                text.modified = false;
                                let _ = write!(message, "saved, {} bytes", text.bytes.len());
                            }
                            Err(e) => {
                                let _ = write!(message, "not saved: {e}");
                            }
                        }
                    }
                    _ if quitting => {
                        // Ctrl+Q / Ctrl+X / Ctrl+C. With unsaved changes, only when pressed twice.
                        if !text.modified || quit_armed {
                            break 'edit;
                        }
                        quit_armed = true;
                        let _ = message.push_str("unsaved: ^S saves, ^X again quits");
                        continue;
                    }
                    Key::Up => text.vertical(-1),
                    Key::Down => text.vertical(1),
                    Key::PageUp => text.vertical(-(view.rows as isize)),
                    Key::PageDown => text.vertical(view.rows as isize),
                    Key::Left => text.left(),
                    Key::Right => text.right(),
                    Key::Home | Key::Char(0x01) => text.home(),
                    Key::End | Key::Char(0x05) => text.end(),
                    Key::Backspace => text.backspace(),
                    Key::Delete => text.delete(),
                    Key::Enter => {
                        if !text.insert(b"\n") {
                            let _ = message.push_str("the file is full (8 KB)");
                        }
                    }
                    Key::Char(b'\t') => {
                        let _ = text.insert(b"    ");
                    }
                    Key::Char(c) if c >= 0x20 && !text.insert(&[c]) => {
                        let _ = message.push_str("the file is full (8 KB)");
                    }
                    _ => {}
                }
                quit_armed = false;
            }
        }
        self.term.print("\x1b[?1049l").await;
    }

    /// Asks the terminal how big it is (moving the cursor far away and asking where it
    /// ended up); 24x80 if it does not answer.
    async fn screen_size(&mut self) -> (usize, usize) {
        self.term.print("\x1b7\x1b[999;999H\x1b[6n\x1b8").await;
        self.term.flush().await;
        let mut reply: heapless::Vec<u8, 32> = heapless::Vec::new();
        let deadline = embassy_time::Instant::now() + embassy_time::Duration::from_millis(300);
        while !reply.ends_with(b"R") {
            let mut buf = [0u8; 16];
            match embassy_futures::select::select(self.rx.read(&mut buf), embassy_time::Timer::at(deadline)).await {
                embassy_futures::select::Either::First(Ok(n)) => {
                    for &b in &buf[..n] {
                        let _ = reply.push(b);
                    }
                }
                _ => break,
            }
        }
        parse_size(&reply).unwrap_or((24, 80))
    }
}

/// Line `n` of the text at its screen row, or `~` past the end.
async fn draw_line<W: Write>(term: &mut crate::term::Term<W>, text: &Text, view: View, n: usize) {
    if n < view.top || n >= view.top + view.rows {
        return;
    }
    out!(term, "\x1b[{};1H", n - view.top + 1);
    let mut line: String<512> = String::new();
    match text.line_range(n) {
        Some(range) => {
            let s = core::str::from_utf8(&text.bytes[range]).unwrap_or("?");
            for c in s.chars().skip(view.left).take(view.cols) {
                let _ = line.push(if c == '\t' { ' ' } else { c });
            }
        }
        None => {
            let _ = line.push_str("\x1b[2m~\x1b[0m");
        }
    }
    let _ = line.push_str("\x1b[K");
    term.print(&line).await;
}

async fn draw_status<W: Write>(term: &mut crate::term::Term<W>, text: &Text, view: View, name: &str, message: &str) {
    let mut status: String<256> = String::new();
    let modified = if text.modified { " [modified]" } else { "" };
    let _ = write!(
        status,
        " {name}{modified}  line {}/{}, col {}  ^S save  ^X quit  {message}",
        text.line() + 1,
        text.line_count(),
        text.col() + 1
    );
    let width = view.cols.min(status.len());
    out!(term, "\x1b[{};1H\x1b[7m{:<width$}\x1b[0m\x1b[K", view.rows + 1, &status[..width]);
}

/// Reads `ESC [ rows ; cols R`.
fn parse_size(reply: &[u8]) -> Option<(usize, usize)> {
    let start = reply.iter().position(|&b| b == 0x1b)?;
    let s = core::str::from_utf8(&reply[start..]).ok()?;
    let (rows, cols) = s.strip_prefix("\x1b[")?.strip_suffix('R')?.split_once(';')?;
    let (rows, cols) = (rows.parse().ok()?, cols.parse().ok()?);
    (rows >= 3 && cols >= 20).then_some((rows, cols))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_and_moving() {
        let mut t = Text::new(b"one\ntwo\nthree");
        assert_eq!((t.line_count(), t.line(), t.col()), (3, 0, 0));
        t.end();
        assert_eq!(t.col(), 3);
        t.vertical(2); // down to "three", same column
        assert_eq!((t.line(), t.col()), (2, 3));
        t.vertical(-1); // "two" is only 3 long
        assert_eq!((t.line(), t.col()), (1, 3));
        t.insert(b"!");
        t.vertical(1);
        t.vertical(-1); // the goal column was reset by typing
        assert_eq!(t.col(), 4);
        assert_eq!(t.bytes.as_slice(), b"one\ntwo!\nthree");
    }

    #[test]
    fn enter_backspace_delete_join_lines() {
        let mut t = Text::new(b"ab");
        t.right();
        t.insert(b"\n");
        assert_eq!((t.bytes.as_slice(), t.line_count()), (&b"a\nb"[..], 2));
        t.backspace();
        assert_eq!(t.bytes.as_slice(), b"ab");
        t.home();
        t.delete();
        assert_eq!(t.bytes.as_slice(), b"b");
        assert!(t.modified);
    }

    #[test]
    fn turkish_letters_are_one_character() {
        let mut t = Text::new("çiş\nab".as_bytes());
        t.end();
        assert_eq!(t.col(), 3);
        t.backspace(); // the whole ş (two bytes)
        assert_eq!(core::str::from_utf8(&t.bytes).unwrap(), "çi\nab");
        t.home();
        t.right();
        assert_eq!(t.cursor, 2); // past ç
        t.vertical(1);
        assert_eq!(t.col(), 1);
    }

    #[test]
    fn carriage_returns_are_dropped_and_size_is_capped() {
        let t = Text::new(b"a\r\nb\r\n");
        assert_eq!(t.bytes.as_slice(), b"a\nb\n");
        let mut big = Text::new(&[b'x'; MAX - 1]);
        assert!(big.insert(b"y"));
        assert!(!big.insert(b"z"));
    }

    #[test]
    fn view_follows_the_cursor() {
        let mut v = View { top: 0, left: 0, rows: 10, cols: 20 };
        v.follow(15, 0);
        assert_eq!(v.top, 6);
        v.follow(2, 30);
        assert_eq!((v.top, v.left), (2, 11));
    }

    #[test]
    fn terminal_size_reply() {
        assert_eq!(parse_size(b"\x1b[40;120R"), Some((40, 120)));
        assert_eq!(parse_size(b"x\x1b[24;80R"), Some((24, 80)));
        assert_eq!(parse_size(b"garbage"), None);
    }
}
