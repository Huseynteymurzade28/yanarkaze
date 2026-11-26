//! A small calculator for the `calc` command: integers (decimal, hex, binary), floats and
//! the C operators that firmware people reach for.
//!
//! Precedence, lowest first (as in C): `|`, `^`, `&`, `<< >>`, `+ -`, `* / %`, unary `- ~`.
//! Integers stay integers (64-bit, wrapping) as long as the result is exact; anything else
//! becomes a float.

use core::fmt;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
}

impl Value {
    fn as_f64(self) -> f64 {
        match self {
            Value::Int(i) => i as f64,
            Value::Float(f) => f,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Syntax(usize),
    DivByZero,
    /// A bitwise operator applied to a float.
    NotInteger,
    TooDeep,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Syntax(at) => write!(f, "syntax error at column {}", at + 1),
            Error::DivByZero => f.write_str("division by zero"),
            Error::NotInteger => f.write_str("bitwise operators need integers"),
            Error::TooDeep => f.write_str("too many nested parentheses"),
        }
    }
}

/// Deep enough for anything typed by hand, shallow enough for a 2 KiB stack.
const MAX_DEPTH: u8 = 16;

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
    depth: u8,
}

pub fn eval(expr: &str) -> Result<Value, Error> {
    let mut p = Parser { src: expr.as_bytes(), pos: 0, depth: 0 };
    let value = p.bit_or()?;
    p.skip_spaces();
    if p.pos != p.src.len() {
        return Err(Error::Syntax(p.pos));
    }
    Ok(value)
}

fn ints(a: Value, b: Value) -> Result<(i64, i64), Error> {
    match (a, b) {
        (Value::Int(a), Value::Int(b)) => Ok((a, b)),
        _ => Err(Error::NotInteger),
    }
}

impl Parser<'_> {
    fn skip_spaces(&mut self) {
        while self.src.get(self.pos) == Some(&b' ') {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_spaces();
        self.src.get(self.pos).copied()
    }

    /// Consumes `op` if it comes next.
    fn eat(&mut self, op: &str) -> bool {
        self.skip_spaces();
        if self.src[self.pos..].starts_with(op.as_bytes()) {
            self.pos += op.len();
            true
        } else {
            false
        }
    }

    fn bit_or(&mut self) -> Result<Value, Error> {
        let mut v = self.bit_xor()?;
        // `|` but not the start of `||` (unsupported, so it ends up as a syntax error).
        while self.peek() == Some(b'|') && self.src.get(self.pos + 1) != Some(&b'|') {
            self.pos += 1;
            let (a, b) = ints(v, self.bit_xor()?)?;
            v = Value::Int(a | b);
        }
        Ok(v)
    }

    fn bit_xor(&mut self) -> Result<Value, Error> {
        let mut v = self.bit_and()?;
        while self.eat("^") {
            let (a, b) = ints(v, self.bit_and()?)?;
            v = Value::Int(a ^ b);
        }
        Ok(v)
    }

    fn bit_and(&mut self) -> Result<Value, Error> {
        let mut v = self.shift()?;
        while self.peek() == Some(b'&') && self.src.get(self.pos + 1) != Some(&b'&') {
            self.pos += 1;
            let (a, b) = ints(v, self.shift()?)?;
            v = Value::Int(a & b);
        }
        Ok(v)
    }

    fn shift(&mut self) -> Result<Value, Error> {
        let mut v = self.sum()?;
        loop {
            let left = if self.eat("<<") {
                true
            } else if self.eat(">>") {
                false
            } else {
                return Ok(v);
            };
            let (a, b) = ints(v, self.sum()?)?;
            let n = (b & 63) as u32;
            v = Value::Int(if left { a.wrapping_shl(n) } else { a.wrapping_shr(n) });
        }
    }

    fn sum(&mut self) -> Result<Value, Error> {
        let mut v = self.product()?;
        loop {
            let plus = match self.peek() {
                Some(b'+') => true,
                Some(b'-') => false,
                _ => return Ok(v),
            };
            self.pos += 1;
            let rhs = self.product()?;
            v = match (v, rhs) {
                (Value::Int(a), Value::Int(b)) => Value::Int(if plus { a.wrapping_add(b) } else { a.wrapping_sub(b) }),
                (a, b) => Value::Float(if plus { a.as_f64() + b.as_f64() } else { a.as_f64() - b.as_f64() }),
            };
        }
    }

    fn product(&mut self) -> Result<Value, Error> {
        let mut v = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(op @ (b'*' | b'/' | b'%')) => op,
                _ => return Ok(v),
            };
            self.pos += 1;
            let rhs = self.unary()?;
            v = match (op, v, rhs) {
                (b'/' | b'%', _, Value::Int(0)) => return Err(Error::DivByZero),
                (b'*', Value::Int(a), Value::Int(b)) => Value::Int(a.wrapping_mul(b)),
                (b'%', Value::Int(a), Value::Int(b)) => Value::Int(a.wrapping_rem(b)),
                // Integer division only when it is exact: 6 / 3 = 2, but 7 / 2 = 3.5.
                (b'/', Value::Int(a), Value::Int(b)) if a.wrapping_rem(b) == 0 => Value::Int(a.wrapping_div(b)),
                (b'/', _, b) if b.as_f64() == 0.0 => return Err(Error::DivByZero),
                (b'*', a, b) => Value::Float(a.as_f64() * b.as_f64()),
                (b'/', a, b) => Value::Float(a.as_f64() / b.as_f64()),
                (_, _, _) => return Err(Error::NotInteger), // % on floats
            };
        }
    }

    fn unary(&mut self) -> Result<Value, Error> {
        match self.peek() {
            Some(b'-') => {
                self.pos += 1;
                Ok(match self.unary()? {
                    Value::Int(i) => Value::Int(i.wrapping_neg()),
                    Value::Float(f) => Value::Float(-f),
                })
            }
            Some(b'+') => {
                self.pos += 1;
                self.unary()
            }
            Some(b'~') => {
                self.pos += 1;
                match self.unary()? {
                    Value::Int(i) => Ok(Value::Int(!i)),
                    Value::Float(_) => Err(Error::NotInteger),
                }
            }
            _ => self.atom(),
        }
    }

    fn atom(&mut self) -> Result<Value, Error> {
        if self.eat("(") {
            if self.depth == MAX_DEPTH {
                return Err(Error::TooDeep);
            }
            self.depth += 1;
            let v = self.bit_or()?;
            self.depth -= 1;
            if !self.eat(")") {
                return Err(Error::Syntax(self.pos));
            }
            return Ok(v);
        }
        self.number()
    }

    fn number(&mut self) -> Result<Value, Error> {
        self.skip_spaces();
        let start = self.pos;
        let rest = &self.src[start..];
        let (radix, skip) = match rest {
            [b'0', b'x' | b'X', ..] => (16, 2),
            [b'0', b'b' | b'B', ..] => (2, 2),
            _ => (10, 0),
        };
        let mut end = start + skip;
        let mut float = false;
        while let Some(&c) = self.src.get(end) {
            let ok = match c {
                b'_' => true,
                b'.' if radix == 10 && !float => {
                    float = true;
                    true
                }
                _ => (c as char).is_digit(radix),
            };
            if !ok {
                break;
            }
            end += 1;
        }
        // Scientific notation: 150e6, 2.5E-3.
        if radix == 10 && end > start && matches!(self.src.get(end), Some(b'e' | b'E')) {
            let mut exp_end = end + 1;
            if matches!(self.src.get(exp_end), Some(b'+' | b'-')) {
                exp_end += 1;
            }
            let exp_digits = exp_end;
            while self.src.get(exp_end).is_some_and(u8::is_ascii_digit) {
                exp_end += 1;
            }
            if exp_end > exp_digits {
                end = exp_end;
                float = true;
            }
        }
        let digits = &self.src[start + skip..end];
        if digits.is_empty() || digits == b"." {
            return Err(Error::Syntax(start));
        }
        self.pos = end;

        if float {
            // Only ASCII digits and one dot got in, so this is valid UTF-8 and parses.
            let text = core::str::from_utf8(digits).map_err(|_| Error::Syntax(start))?;
            let mut clean: heapless::String<32> = heapless::String::new();
            for ch in text.chars().filter(|&c| c != '_') {
                clean.push(ch).map_err(|_| Error::Syntax(start))?;
            }
            return clean.parse().map(Value::Float).map_err(|_| Error::Syntax(start));
        }
        let mut value: u64 = 0;
        for &c in digits.iter().filter(|&&c| c != b'_') {
            let d = (c as char).to_digit(radix).unwrap_or(0) as u64;
            value = value.checked_mul(radix as u64).and_then(|v| v.checked_add(d)).ok_or(Error::Syntax(start))?;
        }
        // 0xFFFF_FFFF_FFFF_FFFF is accepted and reads as -1, like a register would.
        Ok(Value::Int(value as i64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(expr: &str) -> i64 {
        match eval(expr) {
            Ok(Value::Int(i)) => i,
            other => panic!("{expr}: {other:?}"),
        }
    }

    fn float(expr: &str) -> f64 {
        match eval(expr) {
            Ok(Value::Float(f)) => f,
            other => panic!("{expr}: {other:?}"),
        }
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(int("1 + 2 * 3"), 7);
        assert_eq!(int("(1 + 2) * 3"), 9);
        assert_eq!(int("10 - 4 - 3"), 3);
        assert_eq!(int("-2 * -3"), 6);
        assert_eq!(int("17 % 5"), 2);
        assert_eq!(int("6 / 3"), 2);
    }

    #[test]
    fn inexact_division_becomes_float() {
        assert_eq!(float("7 / 2"), 3.5);
        assert_eq!(float("1.5 * 4"), 6.0);
        assert!(float("0.1 + 0.2 - 0.3").abs() < 1e-12);
    }

    #[test]
    fn scientific_notation() {
        assert_eq!(float("150e6"), 150e6);
        assert_eq!(float("2.5E-3"), 2.5e-3);
        assert_eq!(float("1e+2 / 4"), 25.0);
        assert_eq!(eval("1e"), Err(Error::Syntax(1))); // no exponent digits
    }

    #[test]
    fn hex_binary_and_bitwise() {
        assert_eq!(int("0xff & ~0x0f"), 0xf0);
        assert_eq!(int("1 << 12 | 0b101"), 4101);
        assert_eq!(int("0x8000_0000 >> 31"), 1);
        assert_eq!(int("0xAA ^ 0xFF"), 0x55);
        assert_eq!(int("1 + 1 << 2"), 8); // shifts bind looser than +, as in C
        assert_eq!(int("0xFFFF_FFFF_FFFF_FFFF"), -1);
    }

    #[test]
    fn errors() {
        assert_eq!(eval("1 / 0"), Err(Error::DivByZero));
        assert_eq!(eval("1.0 / 0"), Err(Error::DivByZero));
        assert_eq!(eval("5 % 0"), Err(Error::DivByZero));
        assert_eq!(eval("1.5 & 1"), Err(Error::NotInteger));
        assert_eq!(eval("1 +"), Err(Error::Syntax(3)));
        assert_eq!(eval("(1 + 2"), Err(Error::Syntax(6)));
        assert_eq!(eval("2 3"), Err(Error::Syntax(2)));
        assert_eq!(eval("1 || 2"), Err(Error::Syntax(2)));
        assert_eq!(eval("0x"), Err(Error::Syntax(0)));
        assert_eq!(eval("99999999999999999999"), Err(Error::Syntax(0)));
        assert_eq!(eval(""), Err(Error::Syntax(0)));
    }

    #[test]
    fn nesting_is_limited() {
        assert_eq!(int("((((1))))"), 1);
        let deep: heapless::String<64> = (0..20).map(|_| '(').chain("1".chars()).chain((0..20).map(|_| ')')).collect();
        assert_eq!(eval(&deep), Err(Error::TooDeep));
    }
}
