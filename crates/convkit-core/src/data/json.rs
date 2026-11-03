//! JSON, read as a stream of events, so a file of any size is formatted
//! or converted holding only what is open at each moment, and written from
//! events the same way.
//!
//! The reader keeps every number's digits as written and every object's
//! keys in order, says exactly where a file goes wrong and what the usual
//! cause is, and in lenient mode reads JSON5 and JSONC (comments, trailing
//! commas, single quotes, unquoted keys, JSON5's numbers and escapes),
//! noting what it found.

use std::collections::HashMap;
use std::io::BufRead;

use super::error::{DataError, DataErrorKind, Pos};
use super::out::{Out, Role};
use super::value::{Number, Scalar, Value};
use super::{Budget, Emit, Track, MAX_DEPTH};

/// What the reader hands out.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Event {
    BeginObject,
    EndObject,
    BeginArray,
    EndArray,
    Key(String),
    Scalar(Value),
    /// The text is over, after exactly one value.
    End,
}

/// What lenient reading accepted that JSON itself does not.
#[derive(Debug, Clone, Default)]
pub(crate) struct Json5 {
    pub comments: usize,
    pub trailing_commas: bool,
    pub single_quotes: bool,
    pub unquoted_keys: bool,
    pub numbers: bool,
    pub escapes: bool,
    /// Where the first of them is.
    pub first: Option<Pos>,
}

impl Json5 {
    fn saw(&mut self, at: Pos) {
        self.first.get_or_insert(at);
    }

    /// The JSON5 features found, for a note: "comments and trailing commas".
    pub fn features(&self) -> Vec<&'static str> {
        let mut found = Vec::new();
        if self.comments > 0 {
            found.push("comments");
        }
        if self.trailing_commas {
            found.push("trailing commas");
        }
        if self.single_quotes {
            found.push("single-quoted strings");
        }
        if self.unquoted_keys {
            found.push("unquoted keys");
        }
        if self.numbers {
            found.push("JSON5 numbers");
        }
        if self.escapes {
            found.push("JavaScript escapes");
        }
        found
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Start,
    Value,
    /// Just inside `[`, or after a comma in one (where the comma is).
    ArrayFirst,
    ArrayNext(Pos),
    ObjectFirst,
    ObjectNext(Pos),
    Colon,
    AfterValue,
    Done,
    Finished,
}

struct Frame {
    object: bool,
    open: Pos,
    /// An object's keys so far, and where each is, to catch one given
    /// twice.
    keys: HashMap<String, Pos>,
}

pub(crate) struct Reader<R: BufRead> {
    src: R,
    pos: Pos,
    /// Just past the last thing read that wasn't space or a comment: where
    /// an error about the text ending points.
    end: Pos,
    lenient: bool,
    state: State,
    stack: Vec<Frame>,
    pub json5: Json5,
}

/// The hint for a strict error lenient reading would have accepted.
const LENIENT: &str = "conv fmt --lenient reads JSON5 and JSONC, which allow it";

impl<R: BufRead> Reader<R> {
    pub fn new(src: R, lenient: bool) -> Reader<R> {
        let start = Pos {
            line: 1,
            column: 1,
            offset: 0,
        };
        Reader {
            src,
            pos: start,
            end: start,
            lenient,
            state: State::Start,
            stack: Vec::new(),
            json5: Json5::default(),
        }
    }

    fn peek(&mut self) -> Result<Option<u8>, DataError> {
        let buf = self.src.fill_buf().map_err(|e| read_error(&e))?;
        Ok(buf.first().copied())
    }

    fn bump(&mut self) {
        let first = match self.src.fill_buf() {
            Ok(buf) => buf.first().copied(),
            Err(_) => None,
        };
        if let Some(b) = first {
            step(&mut self.pos, b);
            self.src.consume(1);
        }
    }

    fn err(&self, at: Pos, message: impl Into<String>) -> DataError {
        DataError::syntax(at, message)
    }

    /// The next event. After `End`, always `End`.
    pub fn next(&mut self) -> Result<Event, DataError> {
        let event = self.step()?;
        self.end = self.pos;
        Ok(event)
    }

    fn step(&mut self) -> Result<Event, DataError> {
        loop {
            match self.state {
                State::Start => {
                    // A UTF-8 byte-order mark is not JSON, but Windows
                    // tools write one; it is read past.
                    if self.peek()? == Some(0xEF) {
                        let buf = self.src.fill_buf().map_err(|e| read_error(&e))?;
                        if buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
                            self.src.consume(3);
                            self.pos.offset += 3;
                        }
                    }
                    self.skip_space()?;
                    if self.peek()?.is_none() {
                        return Err(self
                            .err(self.pos, "the text is empty")
                            .hint("an empty file isn't JSON; the smallest JSON is {} or null"));
                    }
                    self.state = State::Value;
                }
                State::Value => return self.value(),
                State::ArrayFirst | State::ArrayNext(_) => {
                    self.skip_space()?;
                    if self.peek()? == Some(b']') {
                        if let State::ArrayNext(comma) = self.state {
                            self.trailing_comma(comma)?;
                        }
                        self.bump();
                        self.stack.pop();
                        self.after_value();
                        return Ok(Event::EndArray);
                    }
                    self.state = State::Value;
                }
                State::ObjectFirst | State::ObjectNext(_) => return self.key(),
                State::Colon => {
                    self.skip_space()?;
                    let at = self.pos;
                    match self.peek()? {
                        Some(b':') => {
                            self.bump();
                            self.state = State::Value;
                        }
                        Some(b'=') => {
                            return Err(self
                                .err(at, "an = between a key and its value")
                                .hint("JSON puts a : there"))
                        }
                        None => return Err(self.unclosed(at)),
                        Some(_) => {
                            return Err(self
                                .err(at, "a : is missing after the key")
                                .hint("a key and its value are written \"key\": value"))
                        }
                    }
                }
                State::AfterValue => {
                    self.skip_space()?;
                    let at = self.pos;
                    let object = self.stack.last().is_some_and(|f| f.object);
                    match self.peek()? {
                        Some(b',') => {
                            self.bump();
                            self.state = if object {
                                State::ObjectNext(at)
                            } else {
                                State::ArrayNext(at)
                            };
                        }
                        Some(b'}') if object => {
                            self.bump();
                            self.stack.pop();
                            self.after_value();
                            return Ok(Event::EndObject);
                        }
                        Some(b']') if !object => {
                            self.bump();
                            self.stack.pop();
                            self.after_value();
                            return Ok(Event::EndArray);
                        }
                        Some(c @ (b'}' | b']')) => return Err(self.mismatched(at, c)),
                        None => return Err(self.unclosed(at)),
                        Some(c) if starts_value(c) => {
                            return Err(self
                                .err(at, "a comma is missing before this")
                                .hint("items are separated by commas"))
                        }
                        Some(b':') if !object => {
                            return Err(self.err(at, "a : inside a list").hint(
                                "only an object's keys take a : after them; a list is [a, b]",
                            ))
                        }
                        Some(c) => return Err(self.unexpected(at, c)),
                    }
                }
                State::Done => {
                    self.skip_space()?;
                    let at = self.pos;
                    match self.peek()? {
                        None => {
                            self.state = State::Finished;
                            return Ok(Event::End);
                        }
                        Some(c) => {
                            let e = self.err(at, "more text after the JSON value ended");
                            return Err(if matches!(c, b'{' | b'[') {
                                e.hint(
                                    "this looks like JSON Lines, one value per line; a .json file holds one value, so wrap them in [ ] with commas between",
                                )
                            } else {
                                e.hint("a JSON file holds exactly one value")
                            });
                        }
                    }
                }
                State::Finished => return Ok(Event::End),
            }
        }
    }

    fn after_value(&mut self) {
        self.state = if self.stack.is_empty() {
            State::Done
        } else {
            State::AfterValue
        };
    }

    fn push(&mut self, object: bool, at: Pos) -> Result<(), DataError> {
        if self.stack.len() >= MAX_DEPTH {
            return Err(DataError {
                kind: DataErrorKind::TooBig,
                ..self.err(at, format!("nested more than {MAX_DEPTH} deep"))
            }
            .hint("conv stops there rather than run out of stack"));
        }
        self.bump();
        self.stack.push(Frame {
            object,
            open: at,
            keys: HashMap::new(),
        });
        self.state = if object {
            State::ObjectFirst
        } else {
            State::ArrayFirst
        };
        Ok(())
    }

    fn value(&mut self) -> Result<Event, DataError> {
        self.skip_space()?;
        let at = self.pos;
        match self.peek()? {
            None => Err(self.unclosed(at)),
            Some(b'{') => {
                self.push(true, at)?;
                Ok(Event::BeginObject)
            }
            Some(b'[') => {
                self.push(false, at)?;
                Ok(Event::BeginArray)
            }
            Some(b'"') => {
                let s = self.string(b'"')?;
                self.after_value();
                Ok(Event::Scalar(Value::String(s)))
            }
            Some(b'\'') => {
                self.single_quote(at, "a string in single quotes")?;
                let s = self.string(b'\'')?;
                self.after_value();
                Ok(Event::Scalar(Value::String(s)))
            }
            Some(b'-' | b'+' | b'.' | b'0'..=b'9') => {
                let n = self.number()?;
                self.after_value();
                Ok(Event::Scalar(Value::Number(n)))
            }
            Some(c) if word_char(c) => {
                let v = self.word_value(at)?;
                self.after_value();
                Ok(Event::Scalar(v))
            }
            Some(c @ (b']' | b'}')) => {
                if self.stack.last().is_some_and(|f| f.object == (c == b'}')) {
                    Err(self
                        .err(at, "a value is missing before this")
                        .hint("a key needs a value; null is JSON's empty one"))
                } else {
                    Err(self.mismatched(at, c))
                }
            }
            Some(b',') => Err(self
                .err(at, "a value is missing before this comma")
                .hint("two commas in a row leave an empty item; null is JSON's empty value")),
            Some(c) => Err(self.unexpected(at, c)),
        }
    }

    fn key(&mut self) -> Result<Event, DataError> {
        self.skip_space()?;
        let at = self.pos;
        let key = match self.peek()? {
            Some(b'}') => {
                if let State::ObjectNext(comma) = self.state {
                    self.trailing_comma(comma)?;
                }
                self.bump();
                self.stack.pop();
                self.after_value();
                return Ok(Event::EndObject);
            }
            Some(b'"') => self.string(b'"')?,
            Some(b'\'') => {
                self.single_quote(at, "a key in single quotes")?;
                self.string(b'\'')?
            }
            Some(c) if word_char(c) && !c.is_ascii_digit() => {
                let word = self.word()?;
                if !self.lenient {
                    return Err(self
                        .err(at, format!("the key {word} isn't in quotes"))
                        .hint(format!("JSON keys are strings: \"{word}\"; {LENIENT}")));
                }
                self.json5.unquoted_keys = true;
                self.json5.saw(at);
                word
            }
            None => return Err(self.unclosed(at)),
            Some(b',') => {
                return Err(self
                    .err(at, "a key is missing before this comma")
                    .hint("two commas in a row leave an empty item"))
            }
            Some(b']') => return Err(self.mismatched(at, b']')),
            Some(_) => {
                return Err(self
                    .err(at, "expected a key in double quotes")
                    .hint("an object's items are written \"key\": value"))
            }
        };
        let frame = self
            .stack
            .last_mut()
            .expect("a key is read inside an object");
        if let Some(first) = frame.keys.get(&key) {
            let first = *first;
            return Err(self
                .err(at, format!("the key {key:?} is in this object twice"))
                .hint(format!(
                    "the first is at {}; readers keep one or the other, so conv won't guess: remove or rename one",
                    first.at()
                )));
        }
        frame.keys.insert(key.clone(), at);
        self.state = State::Colon;
        Ok(Event::Key(key))
    }

    fn single_quote(&mut self, at: Pos, what: &str) -> Result<(), DataError> {
        if !self.lenient {
            return Err(self
                .err(at, what.to_string())
                .hint(format!("JSON strings use double quotes; {LENIENT}")));
        }
        self.json5.single_quotes = true;
        self.json5.saw(at);
        Ok(())
    }

    fn trailing_comma(&mut self, comma: Pos) -> Result<(), DataError> {
        if !self.lenient {
            return Err(self.err(comma, "a comma after the last item").hint(format!(
                "JSON allows no comma before }} or ]; remove it, or: {LENIENT}"
            )));
        }
        self.json5.trailing_commas = true;
        self.json5.saw(comma);
        Ok(())
    }

    /// The text ended with something still open. The error points just
    /// past the last thing in it, not at the end of the trailing space.
    fn unclosed(&self, _: Pos) -> DataError {
        let at = self.end;
        match self.stack.last() {
            Some(f) => {
                let (what, close) = if f.object {
                    ("object", '}')
                } else {
                    ("list", ']')
                };
                let more = match self.stack.len() {
                    1 => String::new(),
                    n => format!(" ({n} brackets are open)"),
                };
                self.err(
                    at,
                    format!(
                        "the text ends before the {what} opened at {} is closed{more}",
                        f.open.at()
                    ),
                )
                .hint(format!("add the missing {close}"))
            }
            None => self.err(at, "the text ends where a value should be"),
        }
    }

    fn mismatched(&self, at: Pos, c: u8) -> DataError {
        match self.stack.last() {
            Some(f) => {
                let (what, needs) = if f.object {
                    ("object", '}')
                } else {
                    ("list", ']')
                };
                self.err(
                    at,
                    format!(
                        "this {} doesn't match the {what} opened at {}",
                        char::from(c),
                        f.open.at()
                    ),
                )
                .hint(format!("that {what} closes with {needs}"))
            }
            None => self.err(at, format!("a {} that closes nothing", char::from(c))),
        }
    }

    fn unexpected(&self, at: Pos, c: u8) -> DataError {
        let shown = if c.is_ascii_graphic() {
            format!("{:?}", char::from(c))
        } else {
            format!("the byte 0x{c:02X}")
        };
        self.err(at, format!("unexpected {shown}"))
    }

    /// Spaces, tabs and line breaks, and in lenient mode comments.
    fn skip_space(&mut self) -> Result<(), DataError> {
        loop {
            // The common case in bulk: a run of whitespace in the buffer.
            let buf = self.src.fill_buf().map_err(|e| read_error(&e))?;
            let n = buf
                .iter()
                .position(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
                .unwrap_or(buf.len());
            if n > 0 {
                for &b in &buf[..n] {
                    step(&mut self.pos, b);
                }
                self.src.consume(n);
                continue;
            }
            let first = buf.first().copied();
            match first {
                Some(b'/') => self.comment()?,
                Some(0x0B | 0x0C) if self.lenient => self.bump(),
                _ => return Ok(()),
            }
        }
    }

    fn comment(&mut self) -> Result<(), DataError> {
        let at = self.pos;
        self.bump();
        let block = match self.peek()? {
            Some(b'/') => false,
            Some(b'*') => true,
            _ => return Err(self.unexpected(at, b'/')),
        };
        if !self.lenient {
            return Err(self
                .err(at, "a comment")
                .hint(format!("JSON has no comments; {LENIENT}")));
        }
        self.bump();
        self.json5.comments += 1;
        loop {
            match self.peek()? {
                None if block => {
                    return Err(self
                        .err(at, "a /* comment that is never closed")
                        .hint("close it with */"))
                }
                None => return Ok(()),
                Some(b'\n') if !block => return Ok(()),
                Some(b'*') if block => {
                    self.bump();
                    if self.peek()? == Some(b'/') {
                        self.bump();
                        return Ok(());
                    }
                }
                Some(_) => self.bump(),
            }
        }
    }

    fn string(&mut self, quote: u8) -> Result<String, DataError> {
        let open = self.pos;
        self.bump();
        let mut out: Vec<u8> = Vec::new();
        loop {
            let buf = self.src.fill_buf().map_err(|e| read_error(&e))?;
            if buf.is_empty() {
                let close = char::from(quote);
                return Err(self
                    .err(open, "this string is never closed")
                    .hint(format!("close it with {close}")));
            }
            let n = buf
                .iter()
                .position(|&b| b == quote || b == b'\\' || b < 0x20)
                .unwrap_or(buf.len());
            if n > 0 {
                out.extend_from_slice(&buf[..n]);
                let run = buf[..n].iter().filter(|&&b| b & 0xC0 != 0x80).count();
                self.src.consume(n);
                self.pos.offset += n;
                self.pos.column += run;
                continue;
            }
            let at = self.pos;
            let first = buf[0];
            match first {
                b if b == quote => {
                    self.bump();
                    break;
                }
                b'\\' => self.escape(&mut out)?,
                b'\n' | b'\r' => {
                    return Err(self
                        .err(at, "the line ends inside a string")
                        .hint(format!(
                            "the string opened at {} must close on its own line; write \\n for a line break",
                            open.at()
                        )))
                }
                b'\t' if self.lenient => {
                    out.push(b'\t');
                    self.bump();
                }
                b'\t' => {
                    return Err(self
                        .err(at, "a tab inside a string")
                        .hint("write it as \\t"))
                }
                b => {
                    return Err(self
                        .err(at, format!("the control character U+{b:04X} inside a string"))
                        .hint(format!("write it as \\u{b:04x}")))
                }
            }
        }
        String::from_utf8(out).map_err(|_| {
            self.err(open, "this string isn't UTF-8 text")
                .hint("save the file as UTF-8")
        })
    }

    fn escape(&mut self, out: &mut Vec<u8>) -> Result<(), DataError> {
        let at = self.pos;
        self.bump();
        let Some(c) = self.peek()? else {
            return Err(self.err(at, "the text ends inside a string"));
        };
        self.bump();
        let simple = match c {
            b'"' => Some(b'"'),
            b'\\' => Some(b'\\'),
            b'/' => Some(b'/'),
            b'b' => Some(0x08),
            b'f' => Some(0x0C),
            b'n' => Some(b'\n'),
            b'r' => Some(b'\r'),
            b't' => Some(b'\t'),
            _ => None,
        };
        if let Some(b) = simple {
            out.push(b);
            return Ok(());
        }
        if c == b'u' {
            let first = self.hex(4, at)?;
            let cp = if (0xD800..0xDC00).contains(&first) {
                let mut low = None;
                if self.peek()? == Some(b'\\') {
                    self.bump();
                    if self.peek()? == Some(b'u') {
                        self.bump();
                        low = Some(self.hex(4, at)?);
                    }
                }
                match low {
                    Some(l) if (0xDC00..0xE000).contains(&l) => {
                        0x10000 + ((first - 0xD800) << 10) + (l - 0xDC00)
                    }
                    _ => {
                        return Err(self
                            .err(at, format!("\\u{first:04X} is half of a character"))
                            .hint("it must be followed by the other half, \\uDC00 to \\uDFFF"))
                    }
                }
            } else if (0xDC00..0xE000).contains(&first) {
                return Err(self
                    .err(
                        at,
                        format!("\\u{first:04X} is the second half of a character, alone"),
                    )
                    .hint("it must follow its first half, \\uD800 to \\uDBFF"));
            } else {
                first
            };
            let ch = char::from_u32(cp).expect("surrogates are handled above");
            out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
            return Ok(());
        }
        if !self.lenient {
            let shown = if c.is_ascii_graphic() {
                format!("\\{}", char::from(c))
            } else {
                "a backslash here".to_string()
            };
            let hint = if c == b'\'' {
                format!("a ' needs no backslash in a JSON string; {LENIENT}")
            } else {
                "JSON's escapes are \\\" \\\\ \\/ \\b \\f \\n \\r \\t and \\uXXXX".to_string()
            };
            return Err(self
                .err(at, format!("{shown} isn't a JSON escape"))
                .hint(hint));
        }
        self.json5.escapes = true;
        self.json5.saw(at);
        match c {
            b'v' => out.push(0x0B),
            b'0' => out.push(0),
            b'x' => {
                let cp = self.hex(2, at)?;
                let ch = char::from_u32(cp).expect("two hex digits are a character");
                out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
            }
            // A line continuation: the backslash and the break are dropped.
            b'\n' => {}
            b'\r' => {
                if self.peek()? == Some(b'\n') {
                    self.bump();
                }
            }
            // Any other character stands for itself; a multi-byte one's
            // remaining bytes are read as ordinary text.
            c => out.push(c),
        }
        Ok(())
    }

    fn hex(&mut self, digits: usize, at: Pos) -> Result<u32, DataError> {
        let mut v = 0;
        for _ in 0..digits {
            let d = self.peek()?.and_then(|b| char::from(b).to_digit(16));
            let Some(d) = d else {
                return Err(self
                    .err(at, "an escape with too few hex digits")
                    .hint(format!("it needs exactly {digits}, like \\u00e9")));
            };
            v = v * 16 + d;
            self.bump();
        }
        Ok(v)
    }

    /// A run of number characters, checked as JSON's (or, lenient, JSON5's)
    /// number syntax.
    fn number(&mut self) -> Result<Number, DataError> {
        let at = self.pos;
        let mut raw = String::new();
        while let Some(b) = self.peek()? {
            if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-' | b'_') {
                raw.push(char::from(b));
                self.bump();
            } else {
                break;
            }
        }
        if super::value::is_json_number(&raw) {
            return Ok(Number::from_json(raw));
        }
        if self.lenient {
            if let Some(n) = Number::from_json5(&raw) {
                self.json5.numbers = true;
                self.json5.saw(at);
                return Ok(n);
            }
        }
        let unsigned = raw.trim_start_matches(['-', '+']);
        let lenient_ok = Number::from_json5(&raw).is_some();
        let (message, hint) = if unsigned.len() > 1
            && unsigned.starts_with('0')
            && unsigned.as_bytes()[1].is_ascii_digit()
        {
            (
                format!("{raw} starts with a 0"),
                format!("a JSON number can't, unless it is 0 itself; a zip code or ID with leading zeros needs quotes: \"{raw}\""),
            )
        } else if raw.starts_with('+') && lenient_ok {
            (
                format!("{raw} has a + sign"),
                format!("JSON numbers don't; remove it, or: {LENIENT}"),
            )
        } else if unsigned.starts_with('.') && lenient_ok {
            (
                format!("{raw} has no digit before the point"),
                format!("write 0{unsigned}, or: {LENIENT}"),
            )
        } else if (unsigned.ends_with('.') || unsigned.contains(".e") || unsigned.contains(".E"))
            && lenient_ok
        {
            (
                format!("{raw} has no digit after the point"),
                format!("write {raw}0, or: {LENIENT}"),
            )
        } else if unsigned.starts_with("0x") || unsigned.starts_with("0X") {
            (
                format!("{raw} is hexadecimal"),
                format!("JSON numbers are decimal; {LENIENT}"),
            )
        } else if unsigned == "Infinity" || unsigned == "NaN" {
            (
                format!("JSON has no {raw}"),
                format!("YAML (.inf, .nan) and TOML (inf, nan) can hold it; {LENIENT}"),
            )
        } else if raw.contains('_') {
            (
                format!("{raw} has an underscore"),
                "JSON numbers are written without separators".to_string(),
            )
        } else if raw == "-" {
            ("a - with no number after it".to_string(), String::new())
        } else {
            (format!("{raw} isn't a number"), String::new())
        };
        let e = self.err(at, message);
        Err(if hint.is_empty() { e } else { e.hint(hint) })
    }

    /// Letters, digits, `_` and `$`, and any non-ASCII character: an
    /// unquoted key or a bare word.
    fn word(&mut self) -> Result<String, DataError> {
        let mut raw = Vec::new();
        while let Some(b) = self.peek()? {
            if word_char(b) {
                raw.push(b);
                self.bump();
            } else {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&raw).into_owned())
    }

    fn word_value(&mut self, at: Pos) -> Result<Value, DataError> {
        let word = self.word()?;
        match word.as_str() {
            "true" => return Ok(Value::Bool(true)),
            "false" => return Ok(Value::Bool(false)),
            "null" => return Ok(Value::Null),
            "Infinity" | "NaN" if self.lenient => {
                self.json5.numbers = true;
                self.json5.saw(at);
                return Ok(Value::Number(
                    Number::from_json5(&word).expect("Infinity and NaN are JSON5 numbers"),
                ));
            }
            _ => {}
        }
        let (message, hint) = match word.as_str() {
            "True" | "TRUE" => (
                format!("{word} isn't JSON"),
                "JSON writes it true".to_string(),
            ),
            "False" | "FALSE" => (
                format!("{word} isn't JSON"),
                "JSON writes it false".to_string(),
            ),
            "None" | "Null" | "NULL" | "nil" => (
                format!("{word} isn't JSON"),
                "JSON writes it null".to_string(),
            ),
            "undefined" => (
                "JSON has no undefined".to_string(),
                "use null, or leave the key out".to_string(),
            ),
            "Infinity" | "NaN" => (
                format!("JSON has no {word}"),
                format!("YAML (.inf, .nan) and TOML (inf, nan) can hold it; {LENIENT}"),
            ),
            _ => (
                format!("the text {word} isn't in quotes"),
                format!("JSON strings need double quotes: \"{word}\""),
            ),
        };
        Err(self.err(at, message).hint(hint))
    }
}

fn read_error(e: &std::io::Error) -> DataError {
    DataError::io(e, "read the text")
}

/// Moves `pos` past byte `b`: a column per character, a line per `\n`.
fn step(pos: &mut Pos, b: u8) {
    pos.offset += 1;
    if b == b'\n' {
        pos.line += 1;
        pos.column = 1;
    } else if b & 0xC0 != 0x80 {
        pos.column += 1;
    }
}

fn word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

fn starts_value(c: u8) -> bool {
    matches!(c, b'"' | b'\'' | b'{' | b'[' | b'-' | b'0'..=b'9') || c.is_ascii_alphabetic()
}

/// One whole value from the reader, as a tree.
pub(crate) fn read_tree<R: BufRead>(
    r: &mut Reader<R>,
    budget: &mut Budget,
) -> Result<Value, DataError> {
    let first = r.next()?;
    build(r, first, budget)
}

/// The value `first` begins, read to its end.
pub(crate) fn build<R: BufRead>(
    r: &mut Reader<R>,
    first: Event,
    budget: &mut Budget,
) -> Result<Value, DataError> {
    match first {
        Event::Scalar(v) => {
            if !budget.value(&v) {
                return Err(budget.exceeded());
            }
            Ok(v)
        }
        Event::BeginArray => {
            let mut items = Vec::new();
            loop {
                match r.next()? {
                    Event::EndArray => break,
                    e => items.push(build(r, e, budget)?),
                }
            }
            if !budget.value(&Value::Null) {
                return Err(budget.exceeded());
            }
            Ok(Value::Array(items))
        }
        Event::BeginObject => {
            let mut entries = Vec::new();
            loop {
                match r.next()? {
                    Event::EndObject => break,
                    Event::Key(k) => {
                        if !budget.key(&k) {
                            return Err(budget.exceeded());
                        }
                        let first = r.next()?;
                        let v = build(r, first, budget)?;
                        entries.push((k, v));
                    }
                    e => unreachable!("an object holds keys, not {e:?}"),
                }
            }
            if !budget.value(&Value::Null) {
                return Err(budget.exceeded());
            }
            Ok(Value::Object(entries))
        }
        e => unreachable!("a value never begins with {e:?}"),
    }
}

/// Every event of one value, from the reader straight to a writer: a
/// file of any size, holding only what is open.
pub(crate) fn pipe<R: BufRead>(r: &mut Reader<R>, w: &mut dyn Emit) -> Result<(), DataError> {
    loop {
        match r.next()? {
            Event::BeginObject => w.begin(true)?,
            Event::BeginArray => w.begin(false)?,
            Event::EndObject | Event::EndArray => w.end()?,
            Event::Key(k) => w.key(&k)?,
            Event::Scalar(v) => w.scalar(super::scalar_of(&v))?,
            Event::End => return Ok(()),
        }
    }
}

/// JSON out: two-space (or `--indent`, or tab) indentation, or packed onto
/// one line, always ending in a line break.
pub(crate) struct Writer<'o> {
    out: &'o mut dyn Out,
    indent: Option<String>,
    /// Open containers: whether each is an object, and how many items it
    /// has so far.
    stack: Vec<(bool, usize)>,
    after_key: bool,
    track: Track,
}

impl<'o> Writer<'o> {
    /// `indent` is what one level indents by; `None` packs it all onto one
    /// line.
    pub fn new(out: &'o mut dyn Out, indent: Option<String>) -> Writer<'o> {
        Writer {
            out,
            indent,
            stack: Vec::new(),
            after_key: false,
            track: Track::default(),
        }
    }

    fn put(&mut self, role: Role, s: &str) -> Result<(), DataError> {
        self.out
            .put(role, s)
            .map_err(|e| DataError::io(&e, "write the result"))
    }

    fn newline(&mut self) -> Result<(), DataError> {
        if let Some(step) = &self.indent {
            let line = format!("\n{}", step.repeat(self.stack.len()));
            self.put(Role::Space, &line)?;
        }
        Ok(())
    }

    /// What goes before a value: nothing after a key; in a list, the comma
    /// and the line break.
    fn before_value(&mut self) -> Result<(), DataError> {
        if self.after_key {
            self.after_key = false;
            return Ok(());
        }
        if let Some((_, count)) = self.stack.last_mut() {
            let first = *count == 0;
            *count += 1;
            self.track.item();
            if !first {
                self.put(Role::Punct, ",")?;
            }
            self.newline()?;
        }
        Ok(())
    }
}

impl Emit for Writer<'_> {
    fn begin(&mut self, object: bool) -> Result<(), DataError> {
        self.before_value()?;
        self.put(Role::Punct, if object { "{" } else { "[" })?;
        self.stack.push((object, 0));
        self.track.begin();
        Ok(())
    }

    fn key(&mut self, key: &str) -> Result<(), DataError> {
        if let Some((_, count)) = self.stack.last_mut() {
            let first = *count == 0;
            *count += 1;
            if !first {
                self.put(Role::Punct, ",")?;
            }
        }
        self.newline()?;
        self.track.key(key);
        self.put(Role::Key, &quote(key))?;
        self.put(Role::Punct, ":")?;
        if self.indent.is_some() {
            self.put(Role::Space, " ")?;
        }
        self.after_key = true;
        Ok(())
    }

    fn end(&mut self) -> Result<(), DataError> {
        let (object, count) = self.stack.pop().expect("end follows a begin");
        self.track.end();
        if count > 0 {
            self.newline()?;
        }
        self.put(Role::Punct, if object { "}" } else { "]" })?;
        if self.stack.is_empty() {
            self.put(Role::Space, "\n")?;
        }
        Ok(())
    }

    fn scalar(&mut self, s: Scalar) -> Result<(), DataError> {
        self.before_value()?;
        match s {
            Scalar::Null => self.put(Role::Null, "null")?,
            Scalar::Bool(b) => self.put(Role::Bool, if b { "true" } else { "false" })?,
            Scalar::Number(Number::Int(t) | Number::Float(t)) => self.put(Role::Num, t)?,
            Scalar::Number(n) => {
                return Err(DataError::new(
                    DataErrorKind::Unrepresentable,
                    format!(
                        "JSON has no {}, and {} is {}",
                        if matches!(n, Number::NaN) {
                            "NaN"
                        } else {
                            "infinity"
                        },
                        self.track.render(),
                        n.text()
                    ),
                )
                .hint("YAML (.inf, .nan) and TOML (inf, nan) can hold it"))
            }
            Scalar::Str(t) => self.put(Role::Str, &quote(t))?,
            Scalar::DateTime(t) => self.put(Role::Date, &quote(t))?,
        }
        if self.stack.is_empty() {
            self.put(Role::Space, "\n")?;
        }
        Ok(())
    }
}

/// A JSON string literal: quotes, backslashes and control characters
/// escaped, everything else as it is.
pub(crate) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
