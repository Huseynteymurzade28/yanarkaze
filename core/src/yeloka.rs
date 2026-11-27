//! yeloka: a small language for scripts with logic, run with `yeloka <file>`.
//!
//! ```text
//! let n = 1
//! while n <= 5 {
//!     if n % 2 == 0 { print n, "is even" } else { print n, "is odd" }
//!     n = n + 1
//! }
//! ```
//!
//! So far: integers, variables, `+ - * / %`, comparisons, `and` / `or` / `not`, `print`,
//! `if` / `else` and `while`. It runs straight from the source text (no syntax tree, so no
//! heap): a loop jumps back to where its condition starts, a block that is not taken is
//! skipped by matching braces.

use core::fmt::Write as _;

use heapless::String;

const MAX_VARS: usize = 32;
/// Statements run before the program is stopped, so `while 1 { }` ends.
const MAX_STEPS: u32 = 200_000;
pub const OUTPUT: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Syntax,
    UnknownVariable,
    DivideByZero,
    TooManyVariables,
    TooLong,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    /// 1-based line of the source.
    pub line: usize,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let what = match self.kind {
            ErrorKind::Syntax => "syntax error",
            ErrorKind::UnknownVariable => "unknown variable (give it a value with let first)",
            ErrorKind::DivideByZero => "division by zero",
            ErrorKind::TooManyVariables => "too many variables (32 at most)",
            ErrorKind::TooLong => "the program ran too long (stopped after 200000 steps)",
        };
        write!(f, "line {}: {what}", self.line)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tok<'a> {
    Num(i32),
    Ident(&'a str),
    Str(&'a str),
    /// One or two characters: `+`, `==`, `{`, ...
    Op(&'a str),
    /// A newline or `;`: the end of a statement.
    End,
    Eof,
}

struct Interp<'a> {
    src: &'a str,
    pos: usize,
    vars: heapless::Vec<(&'a str, i32), MAX_VARS>,
    out: String<OUTPUT>,
    steps: u32,
}

type R<T> = Result<T, ErrorKind>;

impl<'a> Interp<'a> {
    fn line(&self) -> usize {
        self.src[..self.pos.min(self.src.len())].matches('\n').count() + 1
    }

    /// The next token, without consuming it.
    fn peek(&self) -> (Tok<'a>, usize) {
        let b = self.src.as_bytes();
        let mut i = self.pos;
        // Spaces, tabs and comments (`#` to the end of the line).
        loop {
            while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r') {
                i += 1;
            }
            if i < b.len() && b[i] == b'#' {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            break;
        }
        if i >= b.len() {
            return (Tok::Eof, i);
        }
        let start = i;
        match b[i] {
            b'\n' | b';' => (Tok::End, i + 1),
            b'0'..=b'9' => {
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                (Tok::Num(self.src[start..i].parse().unwrap_or(i32::MAX)), i)
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                (Tok::Ident(&self.src[start..i]), i)
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                    i += 1;
                }
                (Tok::Str(&self.src[start + 1..i]), (i + 1).min(b.len()))
            }
            _ => {
                let two = self.src.get(i..i + 2).unwrap_or("");
                if matches!(two, "==" | "!=" | "<=" | ">=") {
                    (Tok::Op(two), i + 2)
                } else {
                    let len = self.src[i..].chars().next().map_or(1, char::len_utf8);
                    (Tok::Op(&self.src[i..i + len]), i + len)
                }
            }
        }
    }

    fn next(&mut self) -> Tok<'a> {
        let (t, end) = self.peek();
        self.pos = end;
        t
    }

    fn expect(&mut self, op: &str) -> R<()> {
        match self.next() {
            Tok::Op(o) if o == op => Ok(()),
            _ => Err(ErrorKind::Syntax),
        }
    }

    fn skip_ends(&mut self) {
        while self.peek().0 == Tok::End {
            self.next();
        }
    }

    // --- expressions, lowest precedence first

    fn expr(&mut self) -> R<i32> {
        let mut v = self.and()?;
        while self.peek().0 == Tok::Ident("or") {
            self.next();
            let r = self.and()?;
            v = (v != 0 || r != 0) as i32;
        }
        Ok(v)
    }

    fn and(&mut self) -> R<i32> {
        let mut v = self.not()?;
        while self.peek().0 == Tok::Ident("and") {
            self.next();
            let r = self.not()?;
            v = (v != 0 && r != 0) as i32;
        }
        Ok(v)
    }

    fn not(&mut self) -> R<i32> {
        if self.peek().0 == Tok::Ident("not") {
            self.next();
            return Ok((self.not()? == 0) as i32);
        }
        self.compare()
    }

    fn compare(&mut self) -> R<i32> {
        let l = self.sum()?;
        let op = match self.peek().0 {
            Tok::Op(o @ ("==" | "!=" | "<" | "<=" | ">" | ">=")) => o,
            _ => return Ok(l),
        };
        self.next();
        let r = self.sum()?;
        Ok(match op {
            "==" => l == r,
            "!=" => l != r,
            "<" => l < r,
            "<=" => l <= r,
            ">" => l > r,
            _ => l >= r,
        } as i32)
    }

    fn sum(&mut self) -> R<i32> {
        let mut v = self.product()?;
        loop {
            match self.peek().0 {
                Tok::Op("+") => {
                    self.next();
                    v = v.wrapping_add(self.product()?);
                }
                Tok::Op("-") => {
                    self.next();
                    v = v.wrapping_sub(self.product()?);
                }
                _ => return Ok(v),
            }
        }
    }

    fn product(&mut self) -> R<i32> {
        let mut v = self.unary()?;
        loop {
            let op = match self.peek().0 {
                Tok::Op(o @ ("*" | "/" | "%")) => o,
                _ => return Ok(v),
            };
            self.next();
            let r = self.unary()?;
            v = match op {
                "*" => v.wrapping_mul(r),
                _ if r == 0 => return Err(ErrorKind::DivideByZero),
                "/" => v.wrapping_div(r),
                _ => v.wrapping_rem(r),
            };
        }
    }

    fn unary(&mut self) -> R<i32> {
        if self.peek().0 == Tok::Op("-") {
            self.next();
            return Ok(self.unary()?.wrapping_neg());
        }
        match self.next() {
            Tok::Num(n) => Ok(n),
            Tok::Ident(name) => {
                self.vars.iter().find(|(n, _)| *n == name).map(|&(_, v)| v).ok_or(ErrorKind::UnknownVariable)
            }
            Tok::Op("(") => {
                let v = self.expr()?;
                self.expect(")")?;
                Ok(v)
            }
            _ => Err(ErrorKind::Syntax),
        }
    }

    // --- statements

    fn set(&mut self, name: &'a str, v: i32) -> R<()> {
        if let Some(slot) = self.vars.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = v;
            return Ok(());
        }
        self.vars.push((name, v)).map_err(|_| ErrorKind::TooManyVariables)
    }

    /// Skips a `{ ... }` block (the `{` is next).
    fn skip_block(&mut self) -> R<()> {
        self.expect("{")?;
        let mut depth = 1;
        while depth > 0 {
            match self.next() {
                Tok::Op("{") => depth += 1,
                Tok::Op("}") => depth -= 1,
                Tok::Eof => return Err(ErrorKind::Syntax),
                _ => {}
            }
        }
        Ok(())
    }

    /// Runs a `{ ... }` block (the `{` is next).
    fn block(&mut self) -> R<()> {
        self.expect("{")?;
        loop {
            self.skip_ends();
            if self.peek().0 == Tok::Op("}") {
                self.next();
                return Ok(());
            }
            self.statement()?;
        }
    }

    fn statement(&mut self) -> R<()> {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return Err(ErrorKind::TooLong);
        }
        match self.next() {
            Tok::Ident("let") => {
                let Tok::Ident(name) = self.next() else { return Err(ErrorKind::Syntax) };
                self.expect("=")?;
                let v = self.expr()?;
                self.set(name, v)?;
            }
            Tok::Ident("print") => {
                let mut first = true;
                loop {
                    if !first {
                        let _ = self.out.push(' ');
                    }
                    first = false;
                    if let Tok::Str(s) = self.peek().0 {
                        self.next();
                        let _ = self.out.push_str(s);
                    } else if !matches!(self.peek().0, Tok::End | Tok::Eof | Tok::Op("}")) {
                        let v = self.expr()?;
                        let _ = write!(self.out, "{v}");
                    }
                    if self.peek().0 != Tok::Op(",") {
                        break;
                    }
                    self.next();
                }
                let _ = self.out.push('\n');
            }
            Tok::Ident("if") => {
                let cond = self.expr()?;
                if cond != 0 {
                    self.block()?
                } else {
                    self.skip_block()?
                }
                if self.peek().0 == Tok::Ident("else") {
                    self.next();
                    let is_if = self.peek().0 == Tok::Ident("if");
                    match (cond != 0, is_if) {
                        (true, true) => {
                            // `else if ...`: skip the whole chain.
                            self.next();
                            self.expr()?;
                            self.skip_block()?;
                            while self.peek().0 == Tok::Ident("else") {
                                self.next();
                                if self.peek().0 == Tok::Ident("if") {
                                    self.next();
                                    self.expr()?;
                                }
                                self.skip_block()?;
                            }
                        }
                        (true, false) => self.skip_block()?,
                        (false, true) => self.statement()?,
                        (false, false) => self.block()?,
                    }
                }
            }
            Tok::Ident("while") => {
                let start = self.pos;
                loop {
                    self.pos = start;
                    if self.expr()? == 0 {
                        self.skip_block()?;
                        break;
                    }
                    self.block()?;
                    self.steps += 1;
                    if self.steps > MAX_STEPS {
                        return Err(ErrorKind::TooLong);
                    }
                }
            }
            Tok::Ident(name) => {
                self.expect("=")?;
                let v = self.expr()?;
                if !self.vars.iter().any(|(n, _)| *n == name) {
                    return Err(ErrorKind::UnknownVariable);
                }
                self.set(name, v)?;
            }
            Tok::End => {}
            _ => return Err(ErrorKind::Syntax),
        }
        // A statement ends at a newline, `;`, `}` or the end.
        match self.peek().0 {
            Tok::End => {
                self.next();
                Ok(())
            }
            Tok::Op("}") | Tok::Eof => Ok(()),
            _ => Err(ErrorKind::Syntax),
        }
    }
}

/// Runs a program; what it printed, or where it went wrong (with what it printed so far).
pub fn run(src: &str) -> (String<OUTPUT>, Result<(), Error>) {
    let mut it = Interp { src, pos: 0, vars: heapless::Vec::new(), out: String::new(), steps: 0 };
    let result = loop {
        it.skip_ends();
        if it.peek().0 == Tok::Eof {
            break Ok(());
        }
        if let Err(kind) = it.statement() {
            break Err(Error { kind, line: it.line() });
        }
    };
    (it.out, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(src: &str) -> std::string::String {
        extern crate std;
        let (out, result) = run(src);
        assert_eq!(result, Ok(()), "{src}");
        out.as_str().into()
    }

    extern crate std;

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(out("print 1 + 2 * 3, (1 + 2) * 3, 7 / 2, 7 % 3, -2 * -3"), "7 9 3 1 6\n");
        assert_eq!(out("print 1 < 2 and 2 < 3, 1 == 2 or not 0, 3 >= 3"), "1 1 1\n");
    }

    #[test]
    fn variables_loops_and_branches() {
        let src = "let n = 1\nwhile n <= 4 {\n  if n % 2 == 0 { print n, \"even\" } else { print n, \"odd\" }\n  n = n + 1\n}\n";
        assert_eq!(out(src), "1 odd\n2 even\n3 odd\n4 even\n");
        assert_eq!(out("let x = 5; if x > 9 { print 1 } else if x > 3 { print 2 } else { print 3 }"), "2\n");
        assert_eq!(out("let x = 10; if x > 9 { print 1 } else if x > 3 { print 2 } else { print 3 }"), "1\n");
        assert_eq!(
            out("# a comment\nlet s = 0; let i = 1\nwhile i <= 100 { s = s + i; i = i + 1 }\nprint \"sum\", s"),
            "sum 5050\n"
        );
    }

    #[test]
    fn errors_say_where() {
        assert_eq!(run("let a = 1\nprint b").1, Err(Error { kind: ErrorKind::UnknownVariable, line: 2 }));
        assert_eq!(run("print 1 / 0").1.unwrap_err().kind, ErrorKind::DivideByZero);
        assert_eq!(run("let = 3").1.unwrap_err().kind, ErrorKind::Syntax);
        assert_eq!(run("while 1 { }").1.unwrap_err().kind, ErrorKind::TooLong);
        assert_eq!(run("x = 1").1.unwrap_err().kind, ErrorKind::UnknownVariable);
    }
}
