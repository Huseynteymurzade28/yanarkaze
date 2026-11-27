//! The rules of 2048 on a 4x4 board, without any terminal code.
//!
//! Tiles are stored as exponents (1 = 2, 2 = 4, ... 11 = 2048), so a whole board is 16 bytes.

use crate::oled::{CHAR_W, Display};

pub const N: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Board {
    /// `cells[row][col]`, 0 = empty, otherwise the tile is `2^value`.
    pub cells: [[u8; N]; N],
    pub score: u32,
}

/// Slides one line towards index 0 and merges equal neighbours once, as in the original
/// rules: `[2, 2, 2, 2]` becomes `[4, 4, 0, 0]`, not `[8, 0, 0, 0]`. Returns the points won.
fn slide(line: &mut [u8; N]) -> u32 {
    let mut out = [0u8; N];
    let mut len = 0;
    let mut merged_last = false;
    let mut points = 0;
    for &v in line.iter().filter(|&&v| v != 0) {
        if len > 0 && out[len - 1] == v && !merged_last {
            out[len - 1] += 1;
            points += 1u32 << out[len - 1];
            merged_last = true;
        } else {
            out[len] = v;
            len += 1;
            merged_last = false;
        }
    }
    *line = out;
    points
}

impl Board {
    /// A board with two starting tiles.
    pub fn new(mut rand: impl FnMut() -> u32) -> Self {
        let mut board = Board { cells: [[0; N]; N], score: 0 };
        board.spawn(&mut rand);
        board.spawn(&mut rand);
        board
    }

    pub fn tile(&self, row: usize, col: usize) -> u32 {
        match self.cells[row][col] {
            0 => 0,
            e => 1 << e,
        }
    }

    pub fn best_tile(&self) -> u32 {
        let max = self.cells.iter().flatten().copied().max().unwrap_or(0);
        if max == 0 { 0 } else { 1 << max }
    }

    /// Puts a 2 (90 %) or a 4 (10 %) on a random empty cell.
    fn spawn(&mut self, rand: &mut impl FnMut() -> u32) {
        let empty = self.cells.iter().flatten().filter(|&&v| v == 0).count();
        if empty == 0 {
            return;
        }
        let r = rand();
        let mut n = (r % empty as u32) as usize;
        let value = if (r >> 16) % 10 == 9 { 2 } else { 1 };
        for cell in self.cells.iter_mut().flatten() {
            if *cell == 0 {
                if n == 0 {
                    *cell = value;
                    return;
                }
                n -= 1;
            }
        }
    }

    /// Reads line `i` in the order the tiles slide (towards index 0).
    fn line(&self, m: Move, i: usize) -> [u8; N] {
        core::array::from_fn(|j| {
            let (r, c) = Self::coords(m, i, j);
            self.cells[r][c]
        })
    }

    fn set_line(&mut self, m: Move, i: usize, line: [u8; N]) {
        for (j, v) in line.into_iter().enumerate() {
            let (r, c) = Self::coords(m, i, j);
            self.cells[r][c] = v;
        }
    }

    fn coords(m: Move, i: usize, j: usize) -> (usize, usize) {
        match m {
            Move::Left => (i, j),
            Move::Right => (i, N - 1 - j),
            Move::Up => (j, i),
            Move::Down => (N - 1 - j, i),
        }
    }

    /// Slides every tile. Returns false (and changes nothing) if nothing could move; otherwise
    /// a new tile appears.
    pub fn play(&mut self, m: Move, mut rand: impl FnMut() -> u32) -> bool {
        let before = self.cells;
        for i in 0..N {
            let mut line = self.line(m, i);
            self.score += slide(&mut line);
            self.set_line(m, i, line);
        }
        if self.cells == before {
            return false;
        }
        self.spawn(&mut rand);
        true
    }

    /// True when no move can change the board.
    pub fn is_stuck(&self) -> bool {
        [Move::Up, Move::Down, Move::Left, Move::Right].into_iter().all(|m| {
            let mut copy = self.clone();
            (0..N).all(|i| {
                let mut line = copy.line(m, i);
                slide(&mut line);
                let same = line == copy.line(m, i);
                copy.set_line(m, i, line);
                same
            })
        })
    }
}

/// Cell pitch on a 128x64 OLED: 23x14 pixels inside, one-pixel grid lines.
const OLED_W: i32 = 24;
const OLED_H: i32 = 15;

/// What a tile says on the OLED: at most four characters, so 16384 and up become "16k".
fn oled_label(value: u32) -> heapless::String<6> {
    use core::fmt::Write;
    let mut s = heapless::String::new();
    let _ = if value >= 10_000 { write!(s, "{}k", value / 1024) } else { write!(s, "{value}") };
    s
}

/// Draws the board on a 128x64 OLED with the score on the right. Tiles of 128 and up are
/// drawn in inverse video, so the big ones stand out. `banner` goes in a box over the board.
pub fn render(board: &Board, best: u32, banner: Option<&str>, d: &mut Display) {
    use core::fmt::Write;
    d.clear();
    let (x0, y0) = (0, 1);
    for i in 0..=N as i32 {
        d.vline(x0 + i * OLED_W, y0, N as i32 * OLED_H + 1);
        d.hline(x0, y0 + i * OLED_H, N as i32 * OLED_W + 1);
    }
    for row in 0..N {
        for col in 0..N {
            let value = board.tile(row, col);
            if value == 0 {
                continue;
            }
            let (x, y) = (x0 + col as i32 * OLED_W + 1, y0 + row as i32 * OLED_H + 1);
            let label = oled_label(value);
            let w = label.len() as i32 * CHAR_W as i32 - 1;
            d.text(x + (OLED_W - 1 - w) / 2, y + (OLED_H - 1 - 7) / 2, &label, 1);
            if value >= 128 {
                d.invert_rect(x, y, OLED_W - 1, OLED_H - 1);
                // Dark grid lines around it, so neighbouring big tiles do not merge.
                d.fill_rect(x - 1, y - 1, OLED_W + 1, 1, false);
                d.fill_rect(x - 1, y + OLED_H - 1, OLED_W + 1, 1, false);
                d.fill_rect(x - 1, y - 1, 1, OLED_H + 1, false);
                d.fill_rect(x + OLED_W - 1, y - 1, 1, OLED_H + 1, false);
            }
        }
    }
    let panel = x0 + N as i32 * OLED_W + 3;
    let mut n: heapless::String<12> = heapless::String::new();
    d.text(panel, 2, "pts", 1);
    let _ = write!(n, "{}", board.score);
    d.text(panel, 12, &n, 1);
    d.text(panel, 32, "best", 1);
    n.clear();
    let _ = write!(n, "{}", best.max(board.score));
    d.text(panel, 42, &n, 1);

    if let Some(text) = banner {
        let w = text.chars().count() as i32 * CHAR_W as i32 + 8;
        let x = x0 + (N as i32 * OLED_W - w) / 2;
        let y = y0 + N as i32 * OLED_H / 2 - 8;
        d.fill_rect(x, y, w, 16, false);
        d.rect(x, y, w, 16);
        d.text(x + 5, y + 4, text, 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board(rows: [[u32; N]; N]) -> Board {
        let cells = rows.map(|row| row.map(|v| if v == 0 { 0 } else { v.trailing_zeros() as u8 }));
        Board { cells, score: 0 }
    }

    fn tiles(b: &Board) -> [[u32; N]; N] {
        core::array::from_fn(|r| core::array::from_fn(|c| b.tile(r, c)))
    }

    /// Always picks the first empty cell and a 2.
    fn first_cell() -> u32 {
        0
    }

    #[test]
    fn slide_merges_each_tile_once() {
        let cases = [
            ([1, 1, 1, 1], [2, 2, 0, 0], 8),
            ([0, 1, 0, 1], [2, 0, 0, 0], 4),
            ([1, 1, 2, 0], [2, 2, 0, 0], 4),
            ([2, 1, 1, 0], [2, 2, 0, 0], 4), // [4,2,2] -> [4,4], the new 4 does not merge again
            ([1, 2, 3, 4], [1, 2, 3, 4], 0),
        ];
        for (input, expected, points) in cases {
            let mut line = input;
            assert_eq!(slide(&mut line), points, "{input:?}");
            assert_eq!(line, expected, "{input:?}");
        }
    }

    #[test]
    fn moves_in_all_directions() {
        let start = board([[2, 0, 0, 2], [0, 0, 0, 0], [0, 0, 0, 0], [2, 0, 0, 0]]);

        let mut b = start.clone();
        assert!(b.play(Move::Left, first_cell));
        assert_eq!(tiles(&b)[0], [4, 2, 0, 0]); // merged, then the new 2 at the first empty cell
        assert_eq!(b.score, 4);

        let mut b = start.clone();
        b.play(Move::Right, first_cell);
        assert_eq!(tiles(&b)[0], [2, 0, 0, 4]);

        let mut b = start.clone();
        b.play(Move::Up, first_cell);
        assert_eq!(tiles(&b)[0], [4, 2, 0, 2]);

        let mut b = start;
        b.play(Move::Down, first_cell);
        assert_eq!(tiles(&b)[3], [4, 0, 0, 2]);
    }

    #[test]
    fn a_move_that_changes_nothing_spawns_nothing() {
        let mut b = board([[2, 4, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0]]);
        let before = b.clone();
        assert!(!b.play(Move::Left, first_cell));
        assert_eq!(b, before);
    }

    #[test]
    fn stuck_detection() {
        let stuck = board([[2, 4, 2, 4], [4, 2, 4, 2], [2, 4, 2, 4], [4, 2, 4, 2]]);
        assert!(stuck.is_stuck());
        let mergeable = board([[2, 4, 2, 4], [4, 2, 4, 2], [2, 4, 2, 4], [4, 2, 4, 4]]);
        assert!(!mergeable.is_stuck());
        let has_space = board([[2, 4, 2, 4], [4, 2, 4, 2], [2, 4, 2, 4], [4, 2, 4, 0]]);
        assert!(!has_space.is_stuck());
    }

    #[test]
    fn new_board_has_two_tiles() {
        let b = Board::new(first_cell);
        assert_eq!(b.cells.iter().flatten().filter(|&&v| v != 0).count(), 2);
        assert_eq!(b.best_tile(), 2);
    }

    #[test]
    fn oled_labels_fit_in_four_characters() {
        assert_eq!(oled_label(2), "2");
        assert_eq!(oled_label(2048), "2048");
        assert_eq!(oled_label(16384), "16k");
        assert_eq!(oled_label(131072), "128k");
    }

    #[test]
    fn renders_on_the_oled() {
        let mut b = Board { cells: [[0; N]; N], score: 0 };
        b.cells[0][0] = 1; // 2
        b.cells[3][3] = 7; // 128, inverse video
        let mut d = Display::new();
        render(&b, 0, None, &mut d);
        assert!(d.pixel(0, 1) && d.pixel(0, 61)); // grid corners
        assert!(!d.pixel(96, 61)); // the big tile's grid lines are dark
        assert!(!d.pixel(2, 2)); // small tile: dark background
        assert!(d.pixel(73, 47)); // big tile: lit background
    }
}
