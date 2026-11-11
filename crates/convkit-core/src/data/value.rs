//! The one model every data format is read into and written from: JSON's
//! values, plus TOML's dates, with numbers kept as their digits and objects
//! as their keys in order.

/// A number as its decimal digits, never as a binary float: `0.1` stays
/// `0.1` and `12345678901234567890` stays exact, whatever reads or writes
/// it. The text is always valid JSON number syntax, so every writer can
/// copy it out; each reader turns its own spellings (`+1`, `1_000`, `0x1F`,
/// `.5`) into this form when it reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Number {
    /// `-?(0|[1-9][0-9]*)`, any length.
    Int(String),
    /// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`, with a fraction,
    /// an exponent, or both.
    Float(String),
    /// YAML's `.inf` and TOML's `inf`; JSON has neither.
    Inf {
        negative: bool,
    },
    NaN,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    /// A TOML date, time or date-time (or a YAML timestamp TOML could hold),
    /// as written. Only TOML has a type for it; everything else writes the
    /// text.
    DateTime(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

/// A leaf value as the writers take it.
#[derive(Debug, Clone, Copy)]
pub enum Scalar<'a> {
    Null,
    Bool(bool),
    Number(&'a Number),
    Str(&'a str),
    DateTime(&'a str),
}

impl Value {
    /// What kind of value this is, for messages: "an object", "a list".
    pub fn describe(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "a true/false value",
            Value::Number(_) => "a number",
            Value::String(_) => "a string",
            Value::DateTime(_) => "a date",
            Value::Array(_) => "a list",
            Value::Object(_) => "an object",
        }
    }

    /// Sorts every object's keys, all the way down, by their characters'
    /// code points (as `jq -S` and Python's `sort_keys` do).
    pub fn sort_keys(&mut self) {
        match self {
            Value::Array(items) => items.iter_mut().for_each(Value::sort_keys),
            Value::Object(entries) => {
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                entries.iter_mut().for_each(|(_, v)| v.sort_keys());
            }
            _ => {}
        }
    }
}

impl Number {
    pub fn text(&self) -> &str {
        match self {
            Number::Int(t) | Number::Float(t) => t,
            Number::Inf { negative: false } => "inf",
            Number::Inf { negative: true } => "-inf",
            Number::NaN => "nan",
        }
    }

    /// A JSON number token, already checked by the JSON reader.
    pub(crate) fn from_json(text: String) -> Number {
        if text.bytes().any(|b| matches!(b, b'.' | b'e' | b'E')) {
            Number::Float(text)
        } else {
            Number::Int(text)
        }
    }

    /// A plain YAML scalar the 1.2 core schema reads as a number:
    /// `[-+]?[0-9]+`, `0o[0-7]+`, `0x[0-9a-fA-F]+`, a decimal float, `.inf`
    /// or `.nan` in their three spellings.
    pub(crate) fn from_yaml(s: &str) -> Option<Number> {
        match s {
            ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => {
                return Some(Number::Inf { negative: false })
            }
            "-.inf" | "-.Inf" | "-.INF" => return Some(Number::Inf { negative: true }),
            ".nan" | ".NaN" | ".NAN" => return Some(Number::NaN),
            _ => {}
        }
        if let Some(hex) = s.strip_prefix("0x") {
            return radix(hex, 16).map(Number::Int);
        }
        if let Some(oct) = s.strip_prefix("0o") {
            return radix(oct, 8).map(Number::Int);
        }
        decimal(s)
    }

    /// A TOML integer or float, as written (`+1_000`, `0xDEAD_BEEF`,
    /// `6.626e-34`, `-inf`).
    pub(crate) fn from_toml(raw: &str) -> Option<Number> {
        let unsigned = raw.trim_start_matches(['+', '-']);
        let negative = raw.starts_with('-');
        match unsigned {
            "inf" => return Some(Number::Inf { negative }),
            "nan" => return Some(Number::NaN),
            _ => {}
        }
        let digits: String = raw.chars().filter(|&c| c != '_').collect();
        for (prefix, base) in [("0x", 16), ("0o", 8), ("0b", 2)] {
            if let Some(rest) = digits.strip_prefix(prefix) {
                return radix(rest, base).map(Number::Int);
            }
        }
        decimal(&digits)
    }

    /// A JSON5 number the lenient JSON reader accepted: JSON's, plus a
    /// leading `+`, `.5`, `5.`, hexadecimal, `Infinity` and `NaN`.
    pub(crate) fn from_json5(raw: &str) -> Option<Number> {
        let negative = raw.starts_with('-');
        let unsigned = raw.trim_start_matches(['+', '-']);
        match unsigned {
            "Infinity" => return Some(Number::Inf { negative }),
            "NaN" => return Some(Number::NaN),
            _ => {}
        }
        if let Some(hex) = unsigned
            .strip_prefix("0x")
            .or_else(|| unsigned.strip_prefix("0X"))
        {
            let n = radix(hex, 16)?;
            return Some(Number::Int(if negative && n != "0" {
                format!("-{n}")
            } else {
                n
            }));
        }
        // As in JSON: 02134 is an ID or a zip code, not 2134.
        let b = unsigned.as_bytes();
        if b.len() > 1 && b[0] == b'0' && b[1].is_ascii_digit() {
            return None;
        }
        decimal(raw)
    }

    /// A CSV cell that is plainly a number: JSON's own syntax, or with a
    /// decimal comma (`1,5`) when the file writes them.
    pub(crate) fn from_csv(cell: &str, decimal_comma: bool) -> Option<Number> {
        let json = |s: &str| is_json_number(s).then(|| Number::from_json(s.to_string()));
        if decimal_comma && cell.contains(',') && !cell.contains('.') {
            return json(&cell.replacen(',', ".", 1));
        }
        json(cell)
    }

    /// Whether this integer fits TOML's 64-bit integers.
    pub(crate) fn fits_i64(&self) -> bool {
        match self {
            Number::Int(t) => t.parse::<i64>().is_ok(),
            _ => true,
        }
    }
}

/// Whether `s` is exactly a JSON number token.
pub(crate) fn is_json_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if b.first() == Some(&b'-') {
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_len = i - int_start;
    if int_len == 0 || (int_len > 1 && b[int_start] == b'0') {
        return false;
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    i == b.len()
}

/// A decimal number in any of the spellings the readers accept, in JSON's
/// form: no `+`, no leading zeros, a digit on each side of the point
/// (`.5` -> `0.5`, `5.` -> `5.0`). The digits themselves never change, and
/// neither does the exponent as written.
fn decimal(s: &str) -> Option<Number> {
    let (negative, rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let (mantissa, exponent) = match rest.find(['e', 'E']) {
        Some(at) => (&rest[..at], Some(&rest[at..])),
        None => (rest, None),
    };
    let (int, frac) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    let digits = |d: &str| d.bytes().all(|b| b.is_ascii_digit());
    if !digits(int)
        || !frac.is_none_or(digits)
        || (int.is_empty() && frac.is_none_or(str::is_empty))
    {
        return None;
    }
    if let Some(e) = exponent {
        let body = e[1..].trim_start_matches(['+', '-']);
        if body.is_empty() || !digits(body) || e[1..].len() - body.len() > 1 {
            return None;
        }
    }
    let int = int.trim_start_matches('0');
    let int = if int.is_empty() { "0" } else { int };
    let mut text = String::with_capacity(s.len() + 2);
    if negative {
        text.push('-');
    }
    text.push_str(int);
    let float = frac.is_some() || exponent.is_some();
    if let Some(f) = frac {
        text.push('.');
        text.push_str(if f.is_empty() { "0" } else { f });
    }
    if let Some(e) = exponent {
        text.push_str(e);
    }
    Some(if float {
        Number::Float(text)
    } else {
        Number::Int(text)
    })
}

/// The most digits `radix` reads: its work grows with their square, and no
/// real number in another base is anywhere near this long.
const MAX_RADIX_DIGITS: usize = 1000;

/// Digits in `base` (no sign, no prefix), as a decimal integer of up to
/// `MAX_RADIX_DIGITS` digits.
fn radix(digits: &str, base: u32) -> Option<String> {
    if digits.is_empty() || digits.len() > MAX_RADIX_DIGITS {
        return None;
    }
    // Little-endian limbs of 10^9.
    let mut limbs: Vec<u64> = vec![0];
    for c in digits.chars() {
        let d = u64::from(c.to_digit(base)?);
        let mut carry = d;
        for limb in &mut limbs {
            let v = *limb * u64::from(base) + carry;
            *limb = v % 1_000_000_000;
            carry = v / 1_000_000_000;
        }
        if carry > 0 {
            limbs.push(carry);
        }
    }
    let mut out = limbs.last().copied().unwrap_or(0).to_string();
    for limb in limbs.iter().rev().skip(1) {
        out.push_str(&format!("{limb:09}"));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(n: Option<Number>) -> String {
        n.map(|n| n.text().to_string()).unwrap_or_default()
    }

    #[test]
    fn json_numbers_are_recognised_exactly() {
        for ok in [
            "0",
            "-0",
            "1",
            "0.1",
            "1e5",
            "1E+5",
            "-1.5e-3",
            "123456789012345678901234567890",
        ] {
            assert!(is_json_number(ok), "{ok}");
        }
        for bad in [
            "", "-", "01", "1.", ".5", "+1", "1e", "1e+", "0x1", "1_000", " 1", "1 ",
        ] {
            assert!(!is_json_number(bad), "{bad}");
        }
    }

    #[test]
    fn yaml_spellings_become_json_digits() {
        assert_eq!(text(Number::from_yaml("+12")), "12");
        assert_eq!(text(Number::from_yaml("007")), "7");
        assert_eq!(text(Number::from_yaml("0x1F")), "31");
        assert_eq!(text(Number::from_yaml("0o17")), "15");
        assert_eq!(text(Number::from_yaml(".5")), "0.5");
        assert_eq!(text(Number::from_yaml("5.")), "5.0");
        assert_eq!(text(Number::from_yaml("1e5")), "1e5");
        assert_eq!(text(Number::from_yaml("-.inf")), "-inf");
        assert_eq!(Number::from_yaml("1_000"), None);
        assert_eq!(Number::from_yaml("1.2.3"), None);
        assert_eq!(Number::from_yaml("."), None);
    }

    #[test]
    fn toml_spellings_become_json_digits() {
        assert_eq!(text(Number::from_toml("+1_000")), "1000");
        assert_eq!(text(Number::from_toml("0xDEAD_BEEF")), "3735928559");
        assert_eq!(text(Number::from_toml("0b1101")), "13");
        assert_eq!(text(Number::from_toml("6.626e-34")), "6.626e-34");
        assert_eq!(text(Number::from_toml("-inf")), "-inf");
        assert_eq!(text(Number::from_toml("+nan")), "nan");
    }

    #[test]
    fn long_hex_converts_without_overflow() {
        assert_eq!(
            text(Number::from_yaml("0xFFFFFFFFFFFFFFFFFFFFFFFF")),
            "79228162514264337593543950335"
        );
    }

    #[test]
    fn hex_too_long_to_convert_quickly_is_not_a_number() {
        let long = format!("0x{}", "f".repeat(200_000));
        assert_eq!(Number::from_yaml(&long), None);
        assert_eq!(Number::from_json5(&long), None);
    }

    #[test]
    fn json5_refuses_a_leading_zero() {
        assert_eq!(Number::from_json5("02134"), None);
        assert_eq!(Number::from_json5("-00.5"), None);
        assert_eq!(text(Number::from_json5("0.5")), "0.5");
        assert_eq!(text(Number::from_json5("+0")), "0");
    }

    #[test]
    fn i64_range_is_checked_on_the_digits() {
        assert!(Number::Int("9223372036854775807".into()).fits_i64());
        assert!(Number::Int("-9223372036854775808".into()).fits_i64());
        assert!(!Number::Int("9223372036854775808".into()).fits_i64());
    }
}
