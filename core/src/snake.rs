//! The rules of Snake, without any terminal or timing code.
//!
//! [`Game::step`] advances one tick and reports exactly which cells changed, so the shell
//! only redraws those (a full frame per tick would be too slow over a 115200 baud UART).

use heapless::Deque;

use crate::oled::{CHAR_W, Display};

pub const W: u8 = 24;
pub const H: u8 = 14;
const CELLS: usize = W as usize * H as usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pos {
    pub x: u8,
    pub y: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    fn opposite(self) -> Dir {
        match self {
            Dir::Up => Dir::Down,
            Dir::Down => Dir::Up,
            Dir::Left => Dir::Right,
            Dir::Right => Dir::Left,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// The snake moved. `old_head` is now body, `tail` (if any) is now empty, and `food`
    /// is where new food appeared after the snake ate.
    Moved { head: Pos, old_head: Pos, tail: Option<Pos>, food: Option<Pos> },
    /// Hit a wall or itself.
    Crashed,
    /// Filled the whole board.
    Won,
}

pub struct Game {
    /// Front = head.
    body: Deque<Pos, CELLS>,
    dir: Dir,
    /// Turns typed faster than the snake moves, applied one per tick.
    turns: Deque<Dir, 3>,
    food: Pos,
    score: u32,
}

impl Game {
    /// A three-cell snake in the middle of the board, heading right.
    pub fn new(mut rand: impl FnMut() -> u32) -> Self {
        let mut body = Deque::new();
        let (x, y) = (W / 2, H / 2);
        for dx in 0..3 {
            let _ = body.push_back(Pos { x: x - dx, y });
        }
        let mut game = Game { body, dir: Dir::Right, turns: Deque::new(), food: Pos { x: 0, y: 0 }, score: 0 };
        game.food = game.free_cell(&mut rand).unwrap_or(Pos { x: 0, y: 0 });
        game
    }

    pub fn body(&self) -> impl Iterator<Item = Pos> + '_ {
        self.body.iter().copied()
    }

    pub fn head(&self) -> Pos {
        *self.body.front().unwrap()
    }

    pub fn food(&self) -> Pos {
        self.food
    }

    pub fn score(&self) -> u32 {
        self.score
    }

    pub fn len(&self) -> usize {
        self.body.len()
    }

    /// Queues a turn. Reversing into yourself and repeating a direction are ignored.
    pub fn steer(&mut self, dir: Dir) {
        let last = self.turns.back().copied().unwrap_or(self.dir);
        if dir != last && dir != last.opposite() {
            let _ = self.turns.push_back(dir);
        }
    }

    fn occupied(&self, pos: Pos) -> bool {
        self.body.iter().any(|&p| p == pos)
    }

    fn free_cell(&self, rand: &mut impl FnMut() -> u32) -> Option<Pos> {
        if self.body.len() == CELLS {
            return None;
        }
        // Random guesses are fast while the board is mostly empty...
        for _ in 0..32 {
            let r = rand();
            let pos = Pos { x: ((r & 0xffff) % W as u32) as u8, y: ((r >> 16) % H as u32) as u8 };
            if !self.occupied(pos) {
                return Some(pos);
            }
        }
        // ...then pick the n-th free cell, which always terminates.
        let free = CELLS - self.body.len();
        let n = rand() as usize % free;
        (0..H).flat_map(|y| (0..W).map(move |x| Pos { x, y })).filter(|&p| !self.occupied(p)).nth(n)
    }

    pub fn step(&mut self, mut rand: impl FnMut() -> u32) -> Step {
        if let Some(dir) = self.turns.pop_front() {
            self.dir = dir;
        }
        let old_head = self.head();
        let head = match self.dir {
            Dir::Up if old_head.y > 0 => Pos { y: old_head.y - 1, ..old_head },
            Dir::Down if old_head.y + 1 < H => Pos { y: old_head.y + 1, ..old_head },
            Dir::Left if old_head.x > 0 => Pos { x: old_head.x - 1, ..old_head },
            Dir::Right if old_head.x + 1 < W => Pos { x: old_head.x + 1, ..old_head },
            _ => return Step::Crashed,
        };

        let eats = head == self.food;
        // The tail moves out of the way first, so following it closely is allowed.
        let tail = if eats { None } else { self.body.pop_back() };
        if self.occupied(head) {
            return Step::Crashed;
        }
        let _ = self.body.push_front(head);

        let food = if eats {
            self.score += 1;
            match self.free_cell(&mut rand) {
                Some(food) => {
                    self.food = food;
                    Some(food)
                }
                None => return Step::Won,
            }
        } else {
            None
        };
        Step::Moved { head, old_head, tail, food }
    }
}

/// Pixel size of one board cell on the OLED: 24x14 cells make a 96x56 board.
const OLED_CELL: i32 = 4;
const OLED_X: i32 = 1;
const OLED_Y: i32 = 4;

/// Draws the whole game on a 128x64 OLED: the board in a box on the left, the score and
/// best on the right, and `banner` (e.g. "PAUSED") in a box over the board.
pub fn render(game: &Game, best: u32, banner: Option<&str>, d: &mut Display) {
    use core::fmt::Write;
    d.clear();
    let (bw, bh) = (W as i32 * OLED_CELL, H as i32 * OLED_CELL);
    d.rect(OLED_X - 1, OLED_Y - 1, bw + 2, bh + 2);
    let cell = |p: Pos| (OLED_X + p.x as i32 * OLED_CELL, OLED_Y + p.y as i32 * OLED_CELL);
    for (i, p) in game.body().enumerate() {
        let (x, y) = cell(p);
        if i == 0 {
            d.fill_rect(x, y, OLED_CELL, OLED_CELL, true);
        } else {
            // One pixel smaller, so the segments stay visible.
            d.fill_rect(x, y, OLED_CELL - 1, OLED_CELL - 1, true);
        }
    }
    // Food: a small plus, which reads differently from a body segment.
    let (x, y) = cell(game.food());
    d.fill_rect(x + 1, y, 2, OLED_CELL, true);
    d.fill_rect(x, y + 1, OLED_CELL, 2, true);

    let panel = OLED_X + bw + 3;
    let mut n: heapless::String<12> = heapless::String::new();
    d.text(panel, OLED_Y, "pts", 1);
    let _ = write!(n, "{}", game.score());
    d.text(panel, OLED_Y + 10, &n, 1);
    d.text(panel, OLED_Y + 28, "best", 1);
    n.clear();
    let _ = write!(n, "{}", best.max(game.score()));
    d.text(panel, OLED_Y + 38, &n, 1);

    if let Some(text) = banner {
        let w = text.chars().count() as i32 * CHAR_W as i32 + 8;
        let (x, y) = (OLED_X + (bw - w) / 2, OLED_Y + bh / 2 - 8);
        d.fill_rect(x, y, w, 16, false);
        d.rect(x, y, w, 16);
        d.text(x + 5, y + 4, text, 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed "random" sequence.
    fn rng<const N: usize>(values: [u32; N]) -> impl FnMut() -> u32 {
        let mut i = 0;
        move || {
            let v = values[i % N];
            i += 1;
            v
        }
    }

    fn pos(x: u8, y: u8) -> Pos {
        Pos { x, y }
    }

    #[test]
    fn starts_in_the_middle() {
        let game = Game::new(rng([0]));
        assert_eq!(game.head(), pos(12, 7));
        assert_eq!(game.len(), 3);
        assert_eq!(game.food(), pos(0, 0));
    }

    #[test]
    fn moves_and_reports_changed_cells() {
        let mut game = Game::new(rng([0]));
        let step = game.step(rng([0]));
        assert_eq!(step, Step::Moved { head: pos(13, 7), old_head: pos(12, 7), tail: Some(pos(10, 7)), food: None });
        assert_eq!(game.len(), 3);
    }

    #[test]
    fn eating_grows_and_places_new_food() {
        // Food at (14, 7): two cells ahead. Then the next food at (5, 1).
        let food_a = 14 | (7 << 16);
        let food_b = 5 | (1 << 16);
        let mut game = Game::new(rng([food_a]));
        assert_eq!(game.food(), pos(14, 7));
        game.step(rng([0]));
        let step = game.step(rng([food_b]));
        assert_eq!(step, Step::Moved { head: pos(14, 7), old_head: pos(13, 7), tail: None, food: Some(pos(5, 1)) });
        assert_eq!((game.len(), game.score()), (4, 1));
    }

    #[test]
    fn cannot_reverse() {
        let mut game = Game::new(rng([0]));
        game.steer(Dir::Left);
        game.step(rng([0]));
        assert_eq!(game.head(), pos(13, 7));
    }

    #[test]
    fn quick_turns_are_queued() {
        let mut game = Game::new(rng([0]));
        game.steer(Dir::Up);
        game.steer(Dir::Left); // a U-turn in two ticks
        game.step(rng([0]));
        game.step(rng([0]));
        assert_eq!(game.head(), pos(11, 6));
    }

    #[test]
    fn walls_are_deadly() {
        let mut game = Game::new(rng([0]));
        game.steer(Dir::Up);
        for _ in 0..7 {
            assert!(matches!(game.step(rng([0])), Step::Moved { .. }));
        }
        assert_eq!(game.step(rng([0])), Step::Crashed);
    }

    #[test]
    fn biting_yourself_is_deadly_but_following_the_tail_is_not() {
        // Grow to 5 by placing food straight ahead twice.
        let mut game = Game::new(rng([13 | (7 << 16)]));
        game.step(rng([14 | (7 << 16)]));
        game.step(rng([0]));
        assert_eq!(game.len(), 5);
        // A tight square: up, left, then down into its own body.
        for dir in [Dir::Up, Dir::Left] {
            game.steer(dir);
            assert!(matches!(game.step(rng([0])), Step::Moved { .. }));
        }
        game.steer(Dir::Down);
        assert_eq!(game.step(rng([0])), Step::Crashed);

        // Length 4 in a 2x2 loop: the head always enters the cell the tail just left.
        let mut game = Game::new(rng([13 | (7 << 16)]));
        game.step(rng([0]));
        for _ in 0..3 {
            for dir in [Dir::Up, Dir::Left, Dir::Down, Dir::Right] {
                game.steer(dir);
                assert!(matches!(game.step(rng([0])), Step::Moved { .. }));
            }
        }
    }

    #[test]
    fn food_never_lands_on_the_snake() {
        // The RNG keeps pointing at the head, so the fallback scan has to find a free cell.
        let head = 12 | (7 << 16);
        let game = Game::new(rng([head]));
        assert!(!game.body().any(|p| p == game.food()));
    }

    #[test]
    fn renders_on_the_oled() {
        let game = Game::new(rng([0]));
        let mut d = Display::new();
        render(&game, 7, None, &mut d);
        // Head at (12, 7): a full 4x4 block.
        let (hx, hy) = (1 + 12 * 4, 4 + 7 * 4);
        assert!((0..4).all(|i| d.pixel(hx + i, hy + i)));
        // Body segment at (11, 7) leaves its right column dark.
        assert!(d.pixel(hx - 4, hy) && !d.pixel(hx - 1, hy));
        // Food at (0, 0) is a plus: centre lit, corner dark.
        assert!(d.pixel(2, 5) && !d.pixel(1, 4));
        // The box around the board.
        assert!(d.pixel(0, 3) && d.pixel(97, 60) && !d.pixel(98, 3));

        let mut banner = Display::new();
        render(&game, 7, Some("PAUSED"), &mut banner);
        assert!(banner.count() != d.count());
    }
}
