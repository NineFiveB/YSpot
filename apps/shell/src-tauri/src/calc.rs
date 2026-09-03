//! Inline calculator (SPEC.md §7.7).
//!
//! A deterministic expression engine in the shell: no JS `eval`, no external
//! process, no allocation per token beyond one token vector. It runs on the
//! routing hot path for every keystroke, inside the §2.5 shell-routing share
//! of ≤ 3 ms, so the whole thing is a tokenizer, a precedence-climbing
//! parser and an `f64` evaluator over a query that is at most a line long.
//!
//! What it answers:
//! - arithmetic with precedence and parentheses, `+ - * / %` (modulo), `^`
//!   (exponent, right-associative), unary `-`/`+`/`~`;
//! - percentages the way a calculator means them: `20%` is 0.2 on its own,
//!   but `200 + 10%` is 220 — a percentage *of the left operand* — because
//!   that is what someone typing it into a launcher wants;
//! - bases and bitwise work: `0x`, `0b`, `0o` literals, `& | << >>` and the
//!   `xor` word, with the result echoed in hex when the input was hex;
//! - unit conversion, `12 mi in km`, `72 f in c`, `1.5 gb in mib`;
//! - date math, `days until 2026-12-25` and `days until dec 25`.
//!
//! `^` is exponentiation rather than xor: in a calculator box that is what
//! people type, and `xor` spells the rarer operation unambiguously.
//!
//! Currency (`100 eur in usd`) is NOT here. It is the one item of §7.7 that
//! needs a network fetch and a rate provider, which is a dependency and a
//! third-party choice rather than an implementation detail; recorded as
//! deferred in `docs/M1.md`.

use std::time::{SystemTime, UNIX_EPOCH};

/// A calculator answer: what to show, and what Enter copies (§7.7).
#[derive(Debug, Clone, PartialEq)]
pub struct CalcResult {
    pub display: String,
    pub copy: String,
}

/// Evaluate a root query, or `None` when it is not an expression at all.
///
/// Returning `None` is the common case — nearly every keystroke is a search,
/// not a sum — so every path that decides "not for me" does so before doing
/// real work.
pub fn evaluate(input: &str) -> Option<CalcResult> {
    let q = input.trim();
    // Cheapest possible rejection first: an expression needs a digit
    // somewhere, and a bare word never becomes one.
    if q.len() < 2 || q.len() > 256 || !q.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    if let Some(r) = date_math(q) {
        return Some(r);
    }
    if let Some(r) = conversion(q) {
        return Some(r);
    }
    arithmetic(q)
}

// ---------------------------------------------------------------------------
// Tokens

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    Amp,
    Pipe,
    Xor,
    Shl,
    Shr,
    Tilde,
    LParen,
    RParen,
}

fn tokenize(s: &str) -> Option<(Vec<Tok>, bool)> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(16);
    let mut i = 0usize;
    let mut based = false;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b',' | b'_' => i += 1,
            b'+' => {
                out.push(Tok::Plus);
                i += 1;
            }
            b'-' => {
                out.push(Tok::Minus);
                i += 1;
            }
            b'*' => {
                // `**` is exponentiation too, for the people who type it.
                if b.get(i + 1) == Some(&b'*') {
                    out.push(Tok::Caret);
                    i += 2;
                } else {
                    out.push(Tok::Star);
                    i += 1;
                }
            }
            b'/' => {
                out.push(Tok::Slash);
                i += 1;
            }
            b'%' => {
                out.push(Tok::Percent);
                i += 1;
            }
            b'^' => {
                out.push(Tok::Caret);
                i += 1;
            }
            b'&' => {
                out.push(Tok::Amp);
                i += 1;
            }
            b'|' => {
                out.push(Tok::Pipe);
                i += 1;
            }
            b'~' => {
                out.push(Tok::Tilde);
                i += 1;
            }
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b'<' if b.get(i + 1) == Some(&b'<') => {
                out.push(Tok::Shl);
                i += 2;
            }
            b'>' if b.get(i + 1) == Some(&b'>') => {
                out.push(Tok::Shr);
                i += 2;
            }
            b'0'..=b'9' | b'.' => {
                let (tok, next, was_based) = number(b, i)?;
                based |= was_based;
                out.push(tok);
                i = next;
            }
            _ if c.is_ascii_alphabetic() => {
                let start = i;
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                match s[start..i].to_ascii_lowercase().as_str() {
                    "xor" => out.push(Tok::Xor),
                    "and" => out.push(Tok::Amp),
                    "or" => out.push(Tok::Pipe),
                    "mod" => out.push(Tok::Percent),
                    // Any other word means this is prose, not a sum — which
                    // also rejects a stray base marker like `0 x 1`.
                    _ => return None,
                }
            }
            _ => return None,
        }
    }
    if out.is_empty() {
        return None;
    }
    Some((out, based))
}

/// A number literal at `i`: decimal, or `0x`/`0b`/`0o` in its base.
fn number(b: &[u8], i: usize) -> Option<(Tok, usize, bool)> {
    if b[i] == b'0' && i + 1 < b.len() {
        let (radix, skip) = match b[i + 1] {
            b'x' | b'X' => (16u32, 2),
            b'b' | b'B' => (2, 2),
            b'o' | b'O' => (8, 2),
            _ => (10, 0),
        };
        if radix != 10 {
            let start = i + skip;
            let mut j = start;
            while j < b.len() && (b[j] as char).is_digit(radix) {
                j += 1;
            }
            if j == start {
                return None;
            }
            let text = std::str::from_utf8(&b[start..j]).ok()?;
            let v = u64::from_str_radix(text, radix).ok()?;
            return Some((Tok::Num(v as f64), j, true));
        }
    }
    let mut j = i;
    let mut seen_dot = false;
    while j < b.len() {
        match b[j] {
            b'0'..=b'9' => j += 1,
            b'.' if !seen_dot => {
                seen_dot = true;
                j += 1;
            }
            // Exponent form, but only when a digit follows.
            b'e' | b'E'
                if j > i
                    && b.get(j + 1)
                        .is_some_and(|c| c.is_ascii_digit() || matches!(c, b'+' | b'-')) =>
            {
                j += 2;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                break;
            }
            _ => break,
        }
    }
    let text = std::str::from_utf8(&b[i..j]).ok()?;
    let v: f64 = text.parse().ok()?;
    Some((Tok::Num(v), j, false))
}

// ---------------------------------------------------------------------------
// Parser: precedence climbing over the token vector.

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
}

/// A parsed operand, and whether it was written as a percentage.
///
/// The flag is what makes `200 + 10%` mean 220 while `200 + 10` means 210:
/// the percent-of-left reading belongs to the OPERAND, not to the operator,
/// so it has to travel with the value rather than being inferred from `+`.
#[derive(Clone, Copy)]
struct Value {
    v: f64,
    percent: bool,
}

impl Value {
    fn plain(v: f64) -> Value {
        Value { v, percent: false }
    }
}

/// Binding power of an infix operator, or `None` if it is not one.
fn infix_bp(t: &Tok) -> Option<(u8, u8)> {
    Some(match t {
        Tok::Pipe => (1, 2),
        Tok::Xor => (3, 4),
        Tok::Amp => (5, 6),
        Tok::Shl | Tok::Shr => (7, 8),
        Tok::Plus | Tok::Minus => (9, 10),
        Tok::Star | Tok::Slash | Tok::Percent => (11, 12),
        // Right-associative: the right power binds looser, so `2^3^2` is
        // 2^(3^2).
        Tok::Caret => (16, 15),
        _ => return None,
    })
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn expr(&mut self, min_bp: u8) -> Option<Value> {
        let mut lhs = match self.next()? {
            Tok::Num(v) => Value::plain(v),
            Tok::Minus => Value::plain(-self.expr(13)?.v),
            Tok::Plus => Value::plain(self.expr(13)?.v),
            Tok::Tilde => Value::plain(!to_int(self.expr(13)?.v)? as f64),
            Tok::LParen => {
                let v = self.expr(0)?;
                match self.next() {
                    // A parenthesised percentage keeps its flag: `200 +
                    // (5+5)%` reads like `200 + 10%`.
                    Some(Tok::RParen) => v,
                    _ => return None,
                }
            }
            _ => return None,
        };

        loop {
            // Postfix `%`: a percentage, unless a term follows it, in which
            // case it is modulo and the infix rule below handles it.
            if matches!(self.peek(), Some(Tok::Percent)) && !self.starts_term(self.pos + 1) {
                if min_bp > 12 {
                    break;
                }
                self.pos += 1;
                lhs = Value {
                    v: lhs.v / 100.0,
                    percent: true,
                };
                continue;
            }
            let Some(op) = self.peek().cloned() else {
                break;
            };
            let Some((lbp, rbp)) = infix_bp(&op) else {
                break;
            };
            if lbp < min_bp {
                break;
            }
            self.pos += 1;
            let rhs = self.expr(rbp)?;
            lhs = Value::plain(apply(&op, lhs.v, rhs.v, rhs.percent)?);
        }
        Some(lhs)
    }

    /// Whether a term (and therefore a modulo right-hand side) starts at
    /// `at` — a number, an opening paren, or a prefix operator.
    fn starts_term(&self, at: usize) -> bool {
        matches!(
            self.toks.get(at),
            Some(Tok::Num(_) | Tok::LParen | Tok::Minus | Tok::Plus | Tok::Tilde)
        )
    }
}

/// `rhs_is_percent` carries the operand's own flag: `200 + 10%` adds 10% OF
/// 200, while `200 + 10` adds ten. Only `+` and `-` read it that way —
/// `200 * 10%` is a plain multiply by 0.1, which is what it looks like.
fn apply(op: &Tok, lhs: f64, rhs: f64, rhs_is_percent: bool) -> Option<f64> {
    Some(match op {
        Tok::Plus => lhs + if rhs_is_percent { lhs * rhs } else { rhs },
        Tok::Minus => lhs - if rhs_is_percent { lhs * rhs } else { rhs },
        Tok::Star => lhs * rhs,
        Tok::Slash => lhs / rhs,
        Tok::Percent => lhs % rhs,
        Tok::Caret => lhs.powf(rhs),
        Tok::Amp => (to_int(lhs)? & to_int(rhs)?) as f64,
        Tok::Pipe => (to_int(lhs)? | to_int(rhs)?) as f64,
        Tok::Xor => (to_int(lhs)? ^ to_int(rhs)?) as f64,
        Tok::Shl => to_int(lhs)?.checked_shl(to_int(rhs)?.try_into().ok()?)? as f64,
        Tok::Shr => to_int(lhs)?.checked_shr(to_int(rhs)?.try_into().ok()?)? as f64,
        _ => return None,
    })
}

/// Bitwise operands must be exact integers; a fractional one is a typo, not
/// a rounding opportunity.
fn to_int(v: f64) -> Option<i64> {
    if v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        Some(v as i64)
    } else {
        None
    }
}

fn arithmetic(q: &str) -> Option<CalcResult> {
    // A lone number is not a calculation; showing "5 = 5" is noise.
    let (toks, based) = tokenize(q)?;
    if toks.len() < 2 {
        return None;
    }
    let mut p = Parser {
        toks: &toks,
        pos: 0,
    };
    let value = p.expr(0)?.v;
    if p.pos != toks.len() || !value.is_finite() {
        return None;
    }
    let plain = format_number(value);
    // Echo the base the question was asked in (§7.7 "bit/hex").
    let display = match (based, to_int(value)) {
        (true, Some(i)) if i >= 0 => format!("{plain}  (0x{i:X}, 0b{i:b})"),
        _ => plain.clone(),
    };
    Some(CalcResult {
        display,
        copy: plain,
    })
}

/// Human number formatting: integers grouped with thin separators, decimals
/// trimmed to at most 10 significant fractional digits with trailing zeros
/// removed, so `0.1 + 0.2` reads `0.3` rather than `0.30000000000000004`.
fn format_number(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string();
    }
    let abs = v.abs();
    if abs >= 1e15 || abs < 1e-6 {
        return format!("{v:e}");
    }
    if v.fract() == 0.0 {
        return group(&format!("{}", v as i64));
    }
    let mut s = format!("{v:.10}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    // Round-tripping through the trimmed string keeps grouping honest for
    // values like 1234.5.
    match s.split_once('.') {
        Some((int, frac)) => format!("{}.{frac}", group(int)),
        None => group(&s),
    }
}

fn group(int: &str) -> String {
    let (sign, digits) = match int.strip_prefix('-') {
        Some(d) => ("-", d),
        None => ("", int),
    };
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{sign}{out}")
}

// ---------------------------------------------------------------------------
// Unit conversion

/// Everything measurable in one dimension, as a factor to the dimension's
/// base unit. Temperature is not a scale factor, so it is handled apart.
const UNITS: &[(&str, Dim, f64)] = &[
    // Length, base metre.
    ("mm", Dim::Length, 0.001),
    ("cm", Dim::Length, 0.01),
    ("m", Dim::Length, 1.0),
    ("km", Dim::Length, 1000.0),
    ("in", Dim::Length, 0.0254),
    ("inch", Dim::Length, 0.0254),
    ("inches", Dim::Length, 0.0254),
    ("ft", Dim::Length, 0.3048),
    ("feet", Dim::Length, 0.3048),
    ("foot", Dim::Length, 0.3048),
    ("yd", Dim::Length, 0.9144),
    ("mi", Dim::Length, 1609.344),
    ("mile", Dim::Length, 1609.344),
    ("miles", Dim::Length, 1609.344),
    ("nmi", Dim::Length, 1852.0),
    // Mass, base kilogram.
    ("mg", Dim::Mass, 1e-6),
    ("g", Dim::Mass, 0.001),
    ("kg", Dim::Mass, 1.0),
    ("t", Dim::Mass, 1000.0),
    ("oz", Dim::Mass, 0.028_349_523_125),
    ("lb", Dim::Mass, 0.453_592_37),
    ("lbs", Dim::Mass, 0.453_592_37),
    ("st", Dim::Mass, 6.350_293_18),
    // Data, base byte. Decimal and binary both, because both are meant.
    ("b", Dim::Data, 1.0),
    ("byte", Dim::Data, 1.0),
    ("bytes", Dim::Data, 1.0),
    ("kb", Dim::Data, 1e3),
    ("mb", Dim::Data, 1e6),
    ("gb", Dim::Data, 1e9),
    ("tb", Dim::Data, 1e12),
    ("kib", Dim::Data, 1024.0),
    ("mib", Dim::Data, 1024.0 * 1024.0),
    ("gib", Dim::Data, 1024.0 * 1024.0 * 1024.0),
    ("tib", Dim::Data, 1024.0 * 1024.0 * 1024.0 * 1024.0),
    // Time, base second.
    ("ms", Dim::Time, 0.001),
    ("s", Dim::Time, 1.0),
    ("sec", Dim::Time, 1.0),
    ("min", Dim::Time, 60.0),
    ("h", Dim::Time, 3600.0),
    ("hr", Dim::Time, 3600.0),
    ("hour", Dim::Time, 3600.0),
    ("hours", Dim::Time, 3600.0),
    ("d", Dim::Time, 86400.0),
    ("day", Dim::Time, 86400.0),
    ("days", Dim::Time, 86400.0),
    ("wk", Dim::Time, 604800.0),
    // Speed, base metre per second.
    ("mps", Dim::Speed, 1.0),
    ("kph", Dim::Speed, 1000.0 / 3600.0),
    ("kmh", Dim::Speed, 1000.0 / 3600.0),
    ("mph", Dim::Speed, 1609.344 / 3600.0),
    ("kn", Dim::Speed, 1852.0 / 3600.0),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dim {
    Length,
    Mass,
    Data,
    Time,
    Speed,
    Temperature,
}

fn unit(name: &str) -> Option<(Dim, f64)> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "c" | "celsius" => return Some((Dim::Temperature, 0.0)),
        "f" | "fahrenheit" => return Some((Dim::Temperature, 1.0)),
        "k" | "kelvin" => return Some((Dim::Temperature, 2.0)),
        _ => {}
    }
    UNITS
        .iter()
        .find(|(u, _, _)| *u == n)
        .map(|(_, d, f)| (*d, *f))
}

/// `<expr> <unit> in|to <unit>`.
fn conversion(q: &str) -> Option<CalcResult> {
    let lower = q.to_ascii_lowercase();
    // Split on the last " in " / " to " so `1 in in cm` still parses.
    let (lhs, rhs) = [" in ", " to "]
        .iter()
        .filter_map(|sep| lower.rfind(sep).map(|i| (i, sep.len())))
        .max_by_key(|(i, _)| *i)
        .map(|(i, len)| (&q[..i], q[i + len..].trim()))?;
    let (to_dim, to_factor) = unit(rhs)?;

    // The source unit is the trailing word of the left side; everything
    // before it is the value expression, so `2*3 kg in lb` works.
    let lhs = lhs.trim();
    let split = lhs.rfind(|c: char| !c.is_ascii_alphabetic())? + 1;
    let (value_src, from_name) = lhs.split_at(split);
    let (from_dim, from_factor) = unit(from_name.trim())?;
    if from_dim != to_dim {
        return None;
    }
    let value = match value_src.trim() {
        "" => return None,
        v => arithmetic_value(v)?,
    };

    let out = if to_dim == Dim::Temperature {
        let kelvin = match from_factor as u8 {
            0 => value + 273.15,
            1 => (value - 32.0) * 5.0 / 9.0 + 273.15,
            _ => value,
        };
        match to_factor as u8 {
            0 => kelvin - 273.15,
            1 => (kelvin - 273.15) * 9.0 / 5.0 + 32.0,
            _ => kelvin,
        }
    } else {
        value * from_factor / to_factor
    };
    if !out.is_finite() {
        return None;
    }
    let plain = format_number(out);
    Some(CalcResult {
        display: format!("{plain} {}", rhs.to_ascii_lowercase()),
        copy: plain,
    })
}

/// Evaluate an expression for its value alone (no formatting, no "a lone
/// number is not a calculation" rule — here a lone number is the point).
fn arithmetic_value(q: &str) -> Option<f64> {
    let (toks, _) = tokenize(q)?;
    let mut p = Parser {
        toks: &toks,
        pos: 0,
    };
    let v = p.expr(0)?.v;
    (p.pos == toks.len() && v.is_finite()).then_some(v)
}

// ---------------------------------------------------------------------------
// Date math

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// `days until <date>` / `days since <date>`, where the date is
/// `YYYY-MM-DD` or `<month> <day>` (the next such date, this year or next).
fn date_math(q: &str) -> Option<CalcResult> {
    let lower = q.to_ascii_lowercase();
    let (sign, rest) = [("days until ", 1i64), ("days to ", 1), ("days since ", -1)]
        .into_iter()
        .find_map(|(prefix, sign)| lower.strip_prefix(prefix).map(|r| (sign, r)))?;
    let today = today_civil()?;
    let target = parse_date(rest.trim(), today, sign)?;
    let days = (days_from_civil(target) - days_from_civil(today)) * sign;
    let plain = days.to_string();
    let (y, m, d) = target;
    Some(CalcResult {
        display: format!(
            "{plain} days ({}{y:04}-{m:02}-{d:02})",
            if days < 0 { "was " } else { "" }
        ),
        copy: plain,
    })
}

/// `YYYY-MM-DD`, or `<month> <day>` resolved to the nearest such date in the
/// direction the question asks.
fn parse_date(s: &str, today: (i64, u32, u32), sign: i64) -> Option<(i64, u32, u32)> {
    if let Some((y, rest)) = s.split_once('-') {
        let (m, d) = rest.split_once('-')?;
        let date = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
        return valid(date).then_some(date);
    }
    let mut parts = s.split_whitespace();
    let month_word = parts.next()?;
    let day: u32 = parts
        .next()?
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()?;
    if parts.next().is_some() {
        return None;
    }
    let m = MONTHS
        .iter()
        .position(|m| month_word.starts_with(m))
        .map(|i| i as u32 + 1)?;
    let (ty, tm, td) = today;
    let this_year = (ty, m, day);
    valid(this_year).then_some(())?;
    // "until" a date already past means next year; "since" one still ahead
    // means last year.
    let passed = (m, day) < (tm, td);
    Some(match (sign, passed) {
        (1, true) => (ty + 1, m, day),
        (-1, false) if (m, day) > (tm, td) => (ty - 1, m, day),
        _ => this_year,
    })
}

fn valid((y, m, d): (i64, u32, u32)) -> bool {
    if !(1..=12).contains(&m) || d == 0 || !(1..=9999).contains(&y) {
        return false;
    }
    d <= days_in_month(y, m)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's `days_from_civil`,
/// exact for the whole proleptic Gregorian range and free of any dependency).
fn days_from_civil((y, m, d): (i64, u32, u32)) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// The inverse, for turning "now" into a civil date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Today in local terms. The shell has no timezone database, so this is UTC
/// — off by at most a day near midnight, which for "days until Christmas" is
/// the right trade against carrying a timezone dependency.
fn today_civil() -> Option<(i64, u32, u32)> {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    Some(civil_from_days(secs.div_euclid(86_400)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(q: &str) -> String {
        evaluate(q)
            .unwrap_or_else(|| panic!("no result for {q:?}"))
            .copy
    }

    fn shown(q: &str) -> String {
        evaluate(q)
            .unwrap_or_else(|| panic!("no result for {q:?}"))
            .display
    }

    #[test]
    fn arithmetic_with_precedence_and_parens() {
        assert_eq!(v("1+1"), "2");
        assert_eq!(v("2 + 3 * 4"), "14");
        assert_eq!(v("(2 + 3) * 4"), "20");
        assert_eq!(v("10 / 4"), "2.5");
        assert_eq!(v("7 % 3"), "1");
        assert_eq!(v("2^10"), "1,024");
        assert_eq!(v("2**10"), "1,024");
        // Right-associative exponent.
        assert_eq!(v("2^3^2"), "512");
        assert_eq!(v("-3 + 1"), "-2");
        assert_eq!(v("1 - -1"), "2");
    }

    #[test]
    fn floating_point_noise_is_not_shown() {
        assert_eq!(v("0.1 + 0.2"), "0.3");
        assert_eq!(v("1/3"), "0.3333333333");
    }

    #[test]
    fn large_integers_are_grouped() {
        assert_eq!(v("1234567 * 1"), "1,234,567");
        assert_eq!(v("-1234567 * 1"), "-1,234,567");
        assert_eq!(v("1000 * 1.5"), "1,500");
    }

    #[test]
    fn percentages_read_the_way_a_calculator_means_them() {
        assert_eq!(v("20%"), "0.2");
        assert_eq!(v("200 + 10%"), "220");
        assert_eq!(v("200 - 10%"), "180");
        // Multiplication is a plain scale, not percent-of-left.
        assert_eq!(v("200 * 10%"), "20");
        // `%` between two terms is still modulo.
        assert_eq!(v("10 % 4"), "2");
        assert_eq!(v("10 mod 4"), "2");
        // A plain right operand is added as itself, not as a fraction of the
        // left — the bug this flag exists to prevent.
        assert_eq!(v("200 + 10"), "210");
        assert_eq!(v("0.1 + 0.2"), "0.3");
        assert_eq!(v("2 + 3 * 4"), "14");
        // A parenthesised percentage keeps the reading.
        assert_eq!(v("200 + (5 + 5)%"), "220");
    }

    #[test]
    fn bases_and_bitwise() {
        assert_eq!(v("0xff + 1"), "256");
        assert_eq!(v("0b1010 + 0"), "10");
        assert_eq!(v("0o17 + 0"), "15");
        assert_eq!(v("6 & 3"), "2");
        assert_eq!(v("6 | 3"), "7");
        assert_eq!(v("6 xor 3"), "5");
        assert_eq!(v("6 and 3"), "2");
        assert_eq!(v("6 or 3"), "7");
        // A word that is not an operator keeps the query a search.
        assert!(evaluate("6 nor 3").is_none());
        assert!(evaluate("0 x 1").is_none());
        assert_eq!(v("1 << 10"), "1,024");
        assert_eq!(v("1024 >> 3"), "128");
        assert_eq!(v("~0 + 1"), "0");
        // A hex question is answered in hex too.
        assert!(shown("0xff + 1").contains("0x100"));
        assert!(shown("0xff + 1").contains("0b1"));
        // A decimal question is not.
        assert!(!shown("255 + 1").contains("0x"));
        // Bitwise on a fraction is a typo, not a rounding opportunity.
        assert!(evaluate("1.5 & 1").is_none());
    }

    #[test]
    fn unit_conversion_covers_the_common_dimensions() {
        assert_eq!(v("12 mi in km"), "19.312128");
        assert_eq!(v("1 kg in lb"), "2.2046226218");
        assert_eq!(v("1.5 gb in mib"), "1,430.5114746094");
        assert_eq!(v("90 min in h"), "1.5");
        assert_eq!(v("100 kph in mph"), "62.1371192237");
        // `to` is a synonym, and an expression may stand in for the value.
        assert_eq!(v("2*3 kg to g"), "6,000");
        // The unit name is echoed so the row reads as an answer.
        assert_eq!(shown("12 mi in km"), "19.312128 km");
        // Mixing dimensions is not a conversion.
        assert!(evaluate("12 mi in kg").is_none());
    }

    #[test]
    fn temperature_is_offset_not_scaled() {
        assert_eq!(v("72 f in c"), "22.2222222222");
        assert_eq!(v("100 c in f"), "212");
        assert_eq!(v("0 c in k"), "273.15");
        assert_eq!(v("-40 c in f"), "-40");
    }

    #[test]
    fn date_math_counts_forward_and_back() {
        // Against a known "today", the arithmetic is exact.
        assert_eq!(
            days_from_civil((2026, 12, 25)) - days_from_civil((2026, 9, 3)),
            113
        );
        assert_eq!(
            days_from_civil((2024, 3, 1)) - days_from_civil((2024, 2, 28)),
            2
        ); // leap
        assert_eq!(
            days_from_civil((2023, 3, 1)) - days_from_civil((2023, 2, 28)),
            1
        );
        // Round-trip through the inverse for a spread of dates.
        for z in [-25_567i64, 0, 19_000, 20_000, 100_000] {
            assert_eq!(days_from_civil(civil_from_days(z)), z, "z={z}");
        }
        // The live forms answer with a date and a count.
        let r = evaluate("days until 2099-01-01").expect("iso date");
        assert!(r.display.contains("2099-01-01"), "{}", r.display);
        assert!(r.copy.parse::<i64>().unwrap() > 0);
        let r = evaluate("days since 2000-01-01").expect("past date");
        assert!(r.copy.parse::<i64>().unwrap() > 0);
        assert!(evaluate("days until dec 25").is_some());
        assert!(evaluate("days until 2026-02-30").is_none(), "invalid day");
        assert!(evaluate("days until nope 25").is_none());
    }

    #[test]
    fn prose_and_searches_are_not_calculations() {
        for q in [
            "notepad",
            "report 2024",
            "c:\\users",
            "5",           // a lone number
            "",            //
            "a",           //
            "1 +",         // incomplete
            "(1 + 2",      // unbalanced
            "1 2",         // two numbers, no operator
            "readme.md",   // a file name with a dot and digits nearby
            "version 1.2", // prose with a number
        ] {
            assert!(evaluate(q).is_none(), "{q:?} should not calculate");
        }
    }

    #[test]
    fn hostile_input_terminates_and_returns_nothing() {
        assert!(evaluate(&"(".repeat(300)).is_none());
        assert!(evaluate(&"1+".repeat(200)).is_none());
        assert!(evaluate("1/0").is_none(), "infinity is not an answer");
        assert!(evaluate("0/0").is_none());
        assert!(evaluate("1e400 + 1").is_none());
        assert!(evaluate("999999999 ^ 999999").is_none());
    }
}
