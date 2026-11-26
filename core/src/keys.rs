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
}

impl KeyDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, b: u8) -> Option<Key> {
        match self.state {
            State::Esc => {
                self.state = if matches!(b, b'[' | b'O') { State::Sequence } else { State::Ground };
                None
            }
            State::Sequence => {
                if !(0x40..=0x7e).contains(&b) {
                    return None; // parameter bytes
                }
                self.state = State::Ground;
                match b {
                    b'A' => Some(Key::Up),
                    b'B' => Some(Key::Down),
                    b'C' => Some(Key::Right),
                    b'D' => Some(Key::Left),
                    _ => None,
                }
            }
            State::Ground => match b {
                0x1b => {
                    self.state = State::Esc;
                    None
                }
                b'\r' | b'\n' => Some(Key::Enter),
                0x03 => Some(Key::Interrupt),
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
}
