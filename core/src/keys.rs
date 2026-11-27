//! Decodes raw terminal bytes into keys, for full-screen programs like games.
//!
//! The decoder keeps its state between calls, so an escape sequence split across two reads
//! is still recognised.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Backspace,
    Enter,
    /// Ctrl+C.
    Interrupt,
    Char(u8),
}

#[derive(Default)]
enum State {
    #[default]
    Ground,
    Esc,
    /// Inside `ESC [ ...` or `ESC O ...`.
    Sequence,
}

#[derive(Default)]
pub struct KeyDecoder {
    state: State,
    /// The first number in the sequence so far (`ESC [ 3 ~` is Delete).
    param: u8,
    /// The last byte was `\r`, so a `\n` right after it is the same Enter.
    after_cr: bool,
}

impl KeyDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, b: u8) -> Option<Key> {
        let after_cr = core::mem::replace(&mut self.after_cr, b == b'\r');
        match self.state {
            State::Esc => {
                self.state = if matches!(b, b'[' | b'O') { State::Sequence } else { State::Ground };
                self.param = 0;
                None
            }
            State::Sequence => {
                if !(0x40..=0x7e).contains(&b) {
                    // Parameter bytes: keep the first number, skip modifiers after a ';'.
                    match b {
                        b'0'..=b'9' if self.param < 100 => self.param = self.param * 10 + (b - b'0'),
                        b';' => self.param += 100, // stop collecting
                        _ => {}
                    }
                    return None;
                }
                self.state = State::Ground;
                let param = self.param % 100;
                match b {
                    b'A' => Some(Key::Up),
                    b'B' => Some(Key::Down),
                    b'C' => Some(Key::Right),
                    b'D' => Some(Key::Left),
                    b'H' => Some(Key::Home),
                    b'F' => Some(Key::End),
                    b'~' => match param {
                        1 | 7 => Some(Key::Home),
                        4 | 8 => Some(Key::End),
                        3 => Some(Key::Delete),
                        5 => Some(Key::PageUp),
                        6 => Some(Key::PageDown),
                        _ => None,
                    },
                    _ => None,
                }
            }
            State::Ground => match b {
                0x1b => {
                    self.state = State::Esc;
                    None
                }
                b'\n' if after_cr => None,
                b'\r' | b'\n' => Some(Key::Enter),
                0x03 => Some(Key::Interrupt),
                0x7f | 0x08 => Some(Key::Backspace),
                _ => Some(Key::Char(b)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrows_and_chars() {
        let mut d = KeyDecoder::new();
        let keys: heapless::Vec<Key, 8> = b"\x1b[Aq\x1bOD\x03\r".iter().filter_map(|&b| d.feed(b)).collect();
        assert_eq!(keys, [Key::Up, Key::Char(b'q'), Key::Left, Key::Interrupt, Key::Enter]);
    }

    #[test]
    fn sequence_split_across_reads() {
        let mut d = KeyDecoder::new();
        assert_eq!(d.feed(0x1b), None);
        assert_eq!(d.feed(b'['), None);
        assert_eq!(d.feed(b'1'), None); // modifier parameters are skipped
        assert_eq!(d.feed(b';'), None);
        assert_eq!(d.feed(b'5'), None);
        assert_eq!(d.feed(b'C'), Some(Key::Right));
    }

    #[test]
    fn editing_keys() {
        let mut d = KeyDecoder::new();
        let keys: heapless::Vec<Key, 16> =
            b"\x1b[3~\x1b[H\x1b[4~\x1b[5~\x1b[6~\x1bOF\x7f\r\n\n".iter().filter_map(|&b| d.feed(b)).collect();
        assert_eq!(
            keys,
            [
                Key::Delete,
                Key::Home,
                Key::End,
                Key::PageUp,
                Key::PageDown,
                Key::End,
                Key::Backspace,
                Key::Enter, // \r\n is one Enter
                Key::Enter
            ]
        );
    }
}
