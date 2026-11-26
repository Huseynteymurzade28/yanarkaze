//! The rules of 2048 on a 4x4 board, without any terminal code.
//!
//! Tiles are stored as exponents (1 = 2, 2 = 4, ... 11 = 2048), so a whole board is 16 bytes.

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
}
