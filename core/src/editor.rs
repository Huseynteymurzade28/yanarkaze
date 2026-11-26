//! A small line editor: cursor movement, history and tab completion.
//!
//! It is a pure state machine with no I/O of its own: [`Editor::feed`] takes one input byte
//! and appends the terminal output it causes (echo, redraws, completion lists) to `out`.
//! That keeps it trivially testable on the host.
//!
//! Keys:
//!
//! | key                         | action                              |
//! |-----------------------------|-------------------------------------|
//! | left / right                | move the cursor                     |
//! | Home, Ctrl+A / End, Ctrl+E  | jump to the start / end of the line |
//! | Backspace / Delete          | delete before / under the cursor    |
//! | Ctrl+W                      | delete the word before the cursor   |
//! | Ctrl+U / Ctrl+K             | delete to the start / end of line   |
//! | up, Ctrl+P / down, Ctrl+N   | browse the history                  |
//! | Tab                         | complete commands and arguments     |
//! | Ctrl+L                      | clear the screen                    |
//! | Ctrl+C                      | cancel the line                     |

use core::fmt::Write;

use heapless::{String, Vec};

pub const LINE_MAX: usize = 96;
pub const HISTORY: usize = 8;

/// Output produced by a single key, sized for the largest case (a list of completions).
pub type Out = String<640>;

/// Given the text before the word being completed (for example `""` or `"led "`), returns
/// every word that may go there. The editor filters them by what has been typed so far.
pub type Completer = fn(&str) -> &'static [&'static str];

enum Escape {
    None,
    /// Got ESC.
    Esc,
    /// Inside `ESC [ <param> ...`.
    Csi(u8),
    /// Got `ESC O` (what some terminals send for Home / End).
    Ss3,
}

pub struct Editor {
    prompt: &'static str,
    complete: Completer,
    line: String<LINE_MAX>,
    /// Cursor position as a byte index; only printable ASCII is accepted, so it is also
    /// the column.
    cursor: usize,
    history: Vec<String<LINE_MAX>, HISTORY>,
    /// Entry shown while browsing the history (`history.len()` = the line being typed).
    browse: usize,
    /// The line being typed, kept while browsing the history.
    draft: String<LINE_MAX>,
    escape: Escape,
    last_was_cr: bool,
}

impl Editor {
    pub fn new(prompt: &'static str, complete: Completer) -> Self {
        Self {
            prompt,
            complete,
            line: String::new(),
            cursor: 0,
            history: Vec::new(),
            browse: 0,
            draft: String::new(),
            escape: Escape::None,
            last_was_cr: false,
        }
    }

    #[cfg(test)]
    pub fn line(&self) -> &str {
        &self.line
    }

    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The history, oldest first.
    pub fn history(&self) -> impl Iterator<Item = &str> {
        self.history.iter().map(|h| h.as_str())
    }

    /// Writes the prompt and the line, leaving the terminal cursor at the edit position.
    /// Call it after printing something on the line the editor is on.
    pub fn redraw(&self, out: &mut Out) {
        let _ = write!(out, "\r{}{}\x1b[K", self.prompt, self.line);
        let back = self.line.len() - self.cursor;
        if back > 0 {
            let _ = write!(out, "\x1b[{back}D");
        }
    }

    /// Handles one input byte. Returns the line when Enter was pressed; the editor is then
    /// empty again but does not print a new prompt (the caller does, after the command ran).
    pub fn feed(&mut self, b: u8, out: &mut Out) -> Option<String<LINE_MAX>> {
        match self.escape {
            Escape::Esc => {
                self.escape = match b {
                    b'[' => Escape::Csi(0),
                    b'O' => Escape::Ss3,
                    _ => Escape::None,
                };
                return None;
            }
            Escape::Csi(param) => {
                match b {
                    b'0'..=b'9' => self.escape = Escape::Csi(param.saturating_mul(10).saturating_add(b - b'0')),
                    0x40..=0x7e => {
                        self.escape = Escape::None;
                        self.csi(param, b, out);
                    }
                    _ => {} // separators like ';' in modified keys: ignore
                }
                return None;
            }
            Escape::Ss3 => {
                self.escape = Escape::None;
                self.csi(0, b, out);
                return None;
            }
            Escape::None => {}
        }

        let was_cr = core::mem::replace(&mut self.last_was_cr, b == b'\r');
        match b {
            0x1b => self.escape = Escape::Esc,
            b'\n' if was_cr => {} // CR LF counts as one Enter
            b'\r' | b'\n' => return Some(self.submit(out)),
            0x01 => self.move_to(0, out),               // Ctrl+A
            0x05 => self.move_to(self.line.len(), out), // Ctrl+E
            0x03 => {
                // Ctrl+C
                let _ = out.push_str("^C\r\n");
                self.reset_line();
                self.redraw(out);
            }
            0x08 | 0x7f => {
                if self.cursor > 0 {
                    self.delete_range(self.cursor - 1, self.cursor, out);
                }
            }
            0x0b => self.delete_range(self.cursor, self.line.len(), out), // Ctrl+K
            0x15 => self.delete_range(0, self.cursor, out),               // Ctrl+U
            0x17 => {
                // Ctrl+W: spaces before the cursor, then the word before them.
                let head = &self.line[..self.cursor];
                let start = head.trim_end().rfind(' ').map_or(0, |i| i + 1);
                self.delete_range(start, self.cursor, out);
            }
            0x0c => {
                // Ctrl+L
                let _ = out.push_str("\x1b[2J\x1b[H");
                self.redraw(out);
            }
            0x10 => self.history_step(-1, out), // Ctrl+P
            0x0e => self.history_step(1, out),  // Ctrl+N
            b'\t' => self.tab(out),
            0x20..=0x7e => self.insert(b, out),
            _ => {}
        }
        None
    }

    fn csi(&mut self, param: u8, b: u8, out: &mut Out) {
        match (b, param) {
            (b'A', _) => self.history_step(-1, out),
            (b'B', _) => self.history_step(1, out),
            (b'C', _) => self.move_to((self.cursor + 1).min(self.line.len()), out),
            (b'D', _) => self.move_to(self.cursor.saturating_sub(1), out),
            (b'H', _) | (b'~', 1 | 7) => self.move_to(0, out),
            (b'F', _) | (b'~', 4 | 8) => self.move_to(self.line.len(), out),
            (b'~', 3) if self.cursor < self.line.len() => self.delete_range(self.cursor, self.cursor + 1, out),
            _ => {}
        }
    }

    fn submit(&mut self, out: &mut Out) -> String<LINE_MAX> {
        let _ = out.push_str("\r\n");
        let line = core::mem::take(&mut self.line);
        let cmd = line.trim();
        if !cmd.is_empty() && self.history.last().map(|h| h.as_str()) != Some(cmd) {
            if self.history.is_full() {
                self.history.remove(0);
            }
            let _ = self.history.push(String::try_from(cmd).unwrap_or_default());
        }
        self.reset_line();
        line
    }

    fn reset_line(&mut self) {
        self.line.clear();
        self.draft.clear();
        self.cursor = 0;
        self.browse = self.history.len();
    }

    fn move_to(&mut self, pos: usize, out: &mut Out) {
        if pos < self.cursor {
            let _ = write!(out, "\x1b[{}D", self.cursor - pos);
        } else if pos > self.cursor {
            let _ = write!(out, "\x1b[{}C", pos - self.cursor);
        }
        self.cursor = pos;
    }

    fn insert(&mut self, b: u8, out: &mut Out) {
        if self.line.len() == LINE_MAX {
            return;
        }
        let at_end = self.cursor == self.line.len();
        let mut bytes: Vec<u8, LINE_MAX> = Vec::from_slice(self.line.as_bytes()).unwrap_or_default();
        let _ = bytes.insert(self.cursor, b);
        // Only printable ASCII ever gets in, so this stays valid UTF-8.
        self.line = String::from_utf8(bytes).unwrap_or_default();
        self.cursor += 1;
        if at_end {
            let _ = out.push(b as char); // the common case: just echo
        } else {
            self.redraw(out);
        }
    }

    fn delete_range(&mut self, start: usize, end: usize, out: &mut Out) {
        if start >= end {
            return;
        }
        let mut rest: String<LINE_MAX> = String::new();
        let _ = rest.push_str(&self.line[end..]);
        self.line.truncate(start);
        let _ = self.line.push_str(&rest);
        self.cursor = start;
        if rest.is_empty() && end - start == 1 {
            let _ = out.push_str("\x08 \x08"); // backspace at the end of the line
        } else {
            self.redraw(out);
        }
    }

    fn set_line(&mut self, text: &str, out: &mut Out) {
        self.line.clear();
        let _ = self.line.push_str(text);
        self.cursor = self.line.len();
        self.redraw(out);
    }

    fn history_step(&mut self, dir: i32, out: &mut Out) {
        let target = match dir {
            -1 if self.browse > 0 => self.browse - 1,
            1 if self.browse < self.history.len() => self.browse + 1,
            _ => return,
        };
        if self.browse == self.history.len() {
            self.draft = self.line.clone();
        }
        self.browse = target;
        let text = match self.history.get(target) {
            Some(entry) => entry.clone(),
            None => self.draft.clone(),
        };
        self.set_line(&text, out);
    }

    fn tab(&mut self, out: &mut Out) {
        let head = &self.line[..self.cursor];
        let word_start = head.rfind(' ').map_or(0, |i| i + 1);
        let (context, word) = head.split_at(word_start);
        let mut matches = (self.complete)(context).iter().copied().filter(|c| c.starts_with(word));

        let Some(first) = matches.next() else {
            let _ = out.push('\x07'); // nothing fits: bell
            return;
        };
        // Longest prefix shared by every match.
        let mut common = first.len();
        let mut count = 1;
        for m in matches.clone() {
            common = common.min(first.bytes().zip(m.bytes()).take_while(|(a, b)| a == b).count());
            count += 1;
        }

        if common > word.len() || count == 1 {
            let mut add: String<LINE_MAX> = String::new();
            let _ = add.push_str(&first[word.len()..common]);
            if count == 1 && self.line[self.cursor..].is_empty() {
                let _ = add.push(' ');
            }
            for b in add.bytes() {
                self.insert(b, out);
            }
        } else if count > 1 {
            // Ambiguous and nothing more to add: show the choices.
            let _ = out.push_str("\r\n");
            for m in core::iter::once(first).chain(matches) {
                let _ = write!(out, "{m}  ");
            }
            let _ = out.push_str("\r\n");
            self.redraw(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete(context: &str) -> &'static [&'static str] {
        match context.trim() {
            "" => &["help", "history", "led", "life", "info"],
            "led" => &["on", "off", "blink", "heartbeat", "morse"],
            _ => &[],
        }
    }

    fn editor() -> Editor {
        Editor::new("> ", complete)
    }

    /// Feeds `input` and returns the lines submitted along the way.
    fn type_in(ed: &mut Editor, input: &[u8]) -> std::vec::Vec<std::string::String> {
        let mut lines = std::vec::Vec::new();
        for &b in input {
            let mut out = Out::new();
            if let Some(line) = ed.feed(b, &mut out) {
                lines.push(line.as_str().into());
            }
        }
        lines
    }

    fn output(ed: &mut Editor, input: &[u8]) -> std::string::String {
        let mut all = std::string::String::new();
        for &b in input {
            let mut out = Out::new();
            ed.feed(b, &mut out);
            all.push_str(&out);
        }
        all
    }

    extern crate std;

    #[test]
    fn typing_and_enter() {
        let mut ed = editor();
        assert_eq!(type_in(&mut ed, b"temp\r\n"), ["temp"]);
        assert_eq!(type_in(&mut ed, b"ps\n"), ["ps"]);
        assert_eq!(ed.line(), "");
    }

    #[test]
    fn cr_lf_is_one_enter() {
        let mut ed = editor();
        assert_eq!(type_in(&mut ed, b"a\r\n\r\nb\r"), ["a", "", "b"]);
    }

    #[test]
    fn insert_in_the_middle() {
        let mut ed = editor();
        type_in(&mut ed, b"hlo\x1b[D\x1b[Del");
        assert_eq!(ed.line(), "hello");
        assert_eq!(ed.cursor(), 3);
    }

    #[test]
    fn home_end_and_ctrl_keys() {
        let mut ed = editor();
        type_in(&mut ed, b"world\x01hello \x05!");
        assert_eq!(ed.line(), "hello world!");
        type_in(&mut ed, b"\x1b[H>\x1b[F<"); // xterm Home / End
        assert_eq!(ed.line(), ">hello world!<");
        type_in(&mut ed, b"\x1b[1~[\x1b[4~]\x1bOH(\x1bOF)"); // vt220 and SS3 variants
        assert_eq!(ed.line(), "([>hello world!<])");
    }

    #[test]
    fn deleting() {
        let mut ed = editor();
        type_in(&mut ed, b"abcdef\x7f");
        assert_eq!(ed.line(), "abcde");
        type_in(&mut ed, b"\x01\x1b[3~");
        assert_eq!(ed.line(), "bcde");
        type_in(&mut ed, b"\x1b[C\x0b");
        assert_eq!(ed.line(), "b");
        type_in(&mut ed, b"\x05xyz\x1b[D\x15");
        assert_eq!((ed.line(), ed.cursor()), ("z", 0));
    }

    #[test]
    fn ctrl_w_deletes_a_word() {
        let mut ed = editor();
        type_in(&mut ed, b"led morse  sos  \x17");
        assert_eq!(ed.line(), "led morse  ");
        type_in(&mut ed, b"\x17\x17");
        assert_eq!(ed.line(), "");
    }

    #[test]
    fn ctrl_c_cancels() {
        let mut ed = editor();
        assert!(output(&mut ed, b"oops\x03").contains("^C"));
        assert_eq!(ed.line(), "");
    }

    #[test]
    fn history_browsing_keeps_the_draft() {
        let mut ed = editor();
        type_in(&mut ed, b"one\rtwo\rtwo\r  \rdra");
        assert_eq!(ed.history().collect::<std::vec::Vec<_>>(), ["one", "two"]);
        type_in(&mut ed, b"\x1b[A");
        assert_eq!(ed.line(), "two");
        type_in(&mut ed, b"\x1b[A\x1b[A"); // stops at the oldest
        assert_eq!(ed.line(), "one");
        type_in(&mut ed, b"\x0e\x0e");
        assert_eq!(ed.line(), "dra");
        assert_eq!(type_in(&mut ed, b"\x10\r"), ["two"]);
    }

    #[test]
    fn history_drops_the_oldest() {
        let mut ed = editor();
        for i in 0..HISTORY + 2 {
            let mut out = Out::new();
            for b in std::format!("cmd{i}\r").bytes() {
                ed.feed(b, &mut out);
            }
        }
        assert_eq!(ed.history().count(), HISTORY);
        assert_eq!(ed.history().next(), Some("cmd2"));
    }

    #[test]
    fn line_length_is_capped() {
        let mut ed = editor();
        type_in(&mut ed, &[b'x'; LINE_MAX + 10]);
        assert_eq!(ed.line().len(), LINE_MAX);
    }

    #[test]
    fn tab_completes_a_unique_command() {
        let mut ed = editor();
        type_in(&mut ed, b"in\t");
        assert_eq!(ed.line(), "info ");
        type_in(&mut ed, b"\x15led\t"); // already complete: just add the space
        assert_eq!(ed.line(), "led ");
    }

    #[test]
    fn tab_extends_the_common_prefix_then_lists() {
        let mut ed = editor();
        type_in(&mut ed, b"h\t");
        assert_eq!(ed.line(), "h"); // help / history share nothing beyond "h"
        let shown = output(&mut ed, b"\t");
        assert!(shown.contains("help") && shown.contains("history"));
        type_in(&mut ed, b"\x7fl\t");
        assert_eq!(ed.line(), "l"); // led / life
        type_in(&mut ed, b"e\t");
        assert_eq!(ed.line(), "led ");
    }

    #[test]
    fn tab_completes_arguments() {
        let mut ed = editor();
        type_in(&mut ed, b"led he\t");
        assert_eq!(ed.line(), "led heartbeat ");
        type_in(&mut ed, b"\x15led o\t");
        assert_eq!(ed.line(), "led o");
        assert!(output(&mut ed, b"\t").contains("on  off"));
    }

    #[test]
    fn tab_without_match_rings_the_bell() {
        let mut ed = editor();
        assert_eq!(output(&mut ed, b"zz\t"), "zz\x07");
        assert_eq!(ed.line(), "zz");
    }

    #[test]
    fn redraw_puts_the_cursor_back() {
        let mut ed = editor();
        type_in(&mut ed, b"abcd\x1b[D\x1b[D");
        let mut out = Out::new();
        ed.redraw(&mut out);
        assert_eq!(out.as_str(), "\r> abcd\x1b[K\x1b[2D");
    }
}
