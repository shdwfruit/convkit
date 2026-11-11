//! TOML, read with toml_edit (keeping each number's text), and written by
//! conv's own writer, which keeps every key where it was.
//!
//! TOML has no null and no top-level list, and its integers stop at 64
//! bits: a value TOML can't hold is refused, naming where it is, rather
//! than changed.

use toml_edit::{Item, Table};

use super::error::{DataError, DataErrorKind, Pos};
use super::out::{Out, Role};
use super::value::{Number, Value};
use super::Budget;

/// Reads `text` into a value, and counts its comments.
pub(crate) fn read(text: &str, budget: &mut Budget) -> Result<(Value, usize), DataError> {
    let doc = toml_edit::Document::parse(text).map_err(|e| parse_error(text, &e))?;
    let value = table(doc.as_table(), text, budget)?;
    Ok((value, count_comments(text)))
}

fn table(t: &Table, text: &str, budget: &mut Budget) -> Result<Value, DataError> {
    let mut entries = Vec::with_capacity(t.len());
    for (k, item) in t.iter() {
        if !budget.key(k) {
            return Err(budget.exceeded());
        }
        let v = match item {
            Item::None => continue,
            Item::Value(v) => value(v, text, budget)?,
            Item::Table(sub) => table(sub, text, budget)?,
            Item::ArrayOfTables(a) => Value::Array(
                a.iter()
                    .map(|t| table(t, text, budget))
                    .collect::<Result<_, _>>()?,
            ),
        };
        entries.push((k.to_string(), v));
    }
    Ok(Value::Object(entries))
}

fn value(v: &toml_edit::Value, text: &str, budget: &mut Budget) -> Result<Value, DataError> {
    use toml_edit::Value as V;
    // A number's own text, so `1e10` stays `1e10` and `0.1` stays `0.1`.
    let raw = |span: Option<std::ops::Range<usize>>| span.and_then(|r| text.get(r));
    let out = match v {
        V::String(s) => Value::String(s.value().clone()),
        V::Integer(i) => Value::Number(
            raw(i.span())
                .and_then(Number::from_toml)
                .unwrap_or_else(|| Number::Int(i.value().to_string())),
        ),
        V::Float(f) => {
            let n = raw(f.span()).and_then(Number::from_toml);
            Value::Number(n.unwrap_or_else(|| Number::Float(format!("{:?}", f.value()))))
        }
        V::Boolean(b) => Value::Bool(*b.value()),
        V::Datetime(d) => Value::DateTime(
            raw(d.span())
                .map(str::to_string)
                .unwrap_or_else(|| d.value().to_string()),
        ),
        V::Array(a) => Value::Array(
            a.iter()
                .map(|v| value(v, text, budget))
                .collect::<Result<_, _>>()?,
        ),
        V::InlineTable(t) => {
            let mut entries = Vec::with_capacity(t.len());
            for (k, v) in t.iter() {
                if !budget.key(k) {
                    return Err(budget.exceeded());
                }
                entries.push((k.to_string(), value(v, text, budget)?));
            }
            Value::Object(entries)
        }
    };
    if !budget.value(&out) {
        return Err(budget.exceeded());
    }
    Ok(out)
}

fn parse_error(text: &str, e: &toml_edit::TomlError) -> DataError {
    let offset = e.span().map_or(text.len(), |r| r.start).min(text.len());
    let line_start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let pos = Pos {
        line: text[..offset].matches('\n').count() + 1,
        column: text[line_start..offset].chars().count() + 1,
        offset,
    };
    // toml_edit's message is its first line; what it expected follows.
    let message = e.message().trim().replace('\n', "; ");
    let rest = &text[offset..];
    let hint = if message.contains("duplicate key") {
        Some("a key can be set once in a table; remove or rename one".to_string())
    } else if message.contains("string values must be quoted")
        || (message.contains("invalid") && rest.starts_with(|c: char| c.is_alphabetic()))
    {
        let word: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
        Some(format!("TOML strings are quoted: \"{word}\""))
    } else if message.contains("expected `=`") || message.contains("expected `.`, `=`") {
        Some("a key and its value are written key = value".to_string())
    } else if message.contains("invalid table header") && rest.starts_with('[') {
        Some("a table header is [name] on a line of its own".to_string())
    } else if rest.starts_with('#') || rest.starts_with("//") {
        Some("TOML comments start with # and run to the end of the line".to_string())
    } else {
        None
    };
    let e = DataError::syntax(pos, message);
    match hint {
        Some(h) => e.hint(h),
        None => e,
    }
}

/// Comments: a `#` outside every string. The text has parsed, so a simple
/// scan of its strings is enough.
fn count_comments(text: &str) -> usize {
    let b = text.as_bytes();
    let mut i = 0;
    let mut count = 0;
    while i < b.len() {
        match b[i] {
            b'#' => {
                count += 1;
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' if b[i..].starts_with(b"\"\"\"") => {
                i += 3;
                while i < b.len() && !b[i..].starts_with(b"\"\"\"") {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 3;
                while i < b.len() && b[i] == b'"' {
                    i += 1;
                }
            }
            b'\'' if b[i..].starts_with(b"'''") => {
                i += 3;
                while i < b.len() && !b[i..].starts_with(b"'''") {
                    i += 1;
                }
                i += 3;
                while i < b.len() && b[i] == b'\'' {
                    i += 1;
                }
            }
            q @ (b'"' | b'\'') => {
                i += 1;
                while i < b.len() && b[i] != q && b[i] != b'\n' {
                    i += if q == b'"' && b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    count
}

/// Writes `v` as a TOML document.
///
/// Keys stay in their order. A table's own keys are written first, then
/// its tables as `[headers]`; a table among the keys that has keys after
/// it is written with dotted keys (`a.b = 1`) instead, since nothing after
/// a header can belong to the table above it.
pub(crate) fn write(v: &Value, out: &mut dyn Out) -> Result<(), DataError> {
    let Value::Object(root) = v else {
        return Err(DataError::new(
            DataErrorKind::Unrepresentable,
            format!(
                "a TOML document is a table of keys, and this is {}",
                v.describe()
            ),
        )
        .hint(match v {
            Value::Array(_) => "TOML has no top-level list; JSON or YAML can hold one",
            _ => "JSON or YAML can hold it",
        }));
    };
    let mut w = Writer {
        out,
        path: Vec::new(),
        wrote: false,
    };
    w.table(root)
}

struct Writer<'o> {
    out: &'o mut dyn Out,
    /// Where the writer is, for messages: keys and list positions.
    path: Vec<Seg>,
    wrote: bool,
}

#[derive(Clone)]
enum Seg {
    Key(String),
    Index(usize),
}

/// A child that can be written as its own `[table]` or `[[table]]`.
fn headed(v: &Value) -> bool {
    match v {
        Value::Object(e) => !e.is_empty(),
        Value::Array(items) => {
            !items.is_empty() && items.iter().all(|i| matches!(i, Value::Object(_)))
        }
        _ => false,
    }
}

impl Writer<'_> {
    fn put(&mut self, role: Role, s: &str) -> Result<(), DataError> {
        self.wrote = true;
        self.out
            .put(role, s)
            .map_err(|e| DataError::io(&e, "write the result"))
    }

    fn where_(&self) -> String {
        render_path(&self.path)
    }

    /// A table's contents: its own keys, then its tables.
    fn table(&mut self, entries: &[(String, Value)]) -> Result<(), DataError> {
        let split = entries
            .iter()
            .rposition(|(_, v)| !headed(v))
            .map_or(0, |i| i + 1);
        for (k, v) in &entries[..split] {
            self.path.push(Seg::Key(k.clone()));
            self.dotted(&mut vec![k.as_str()], v)?;
            self.path.pop();
        }
        for (k, v) in &entries[split..] {
            self.path.push(Seg::Key(k.clone()));
            match v {
                Value::Object(sub) => {
                    // A table with no keys of its own needs no header: its
                    // tables' headers name it.
                    let own = sub.iter().any(|(_, v)| !headed(v));
                    if own {
                        self.header(false)?;
                    }
                    self.table(sub)?;
                }
                Value::Array(items) => {
                    for (i, item) in items.iter().enumerate() {
                        let Value::Object(sub) = item else {
                            unreachable!("headed lists hold tables")
                        };
                        self.path.push(Seg::Index(i));
                        self.header(true)?;
                        self.table(sub)?;
                        self.path.pop();
                    }
                }
                _ => unreachable!("only tables and lists of tables are headed"),
            }
            self.path.pop();
        }
        Ok(())
    }

    /// `[a.b]` or `[[a.b]]`, for the keys on the path.
    fn header(&mut self, array: bool) -> Result<(), DataError> {
        if self.wrote {
            self.put(Role::Space, "\n")?;
        }
        let keys: Vec<String> = self
            .path
            .iter()
            .filter_map(|s| match s {
                Seg::Key(k) => Some(key(k)),
                Seg::Index(_) => None,
            })
            .collect();
        let (open, close) = if array { ("[[", "]]") } else { ("[", "]") };
        self.put(Role::Header, &format!("{open}{}{close}", keys.join(".")))?;
        self.put(Role::Space, "\n")
    }

    /// `a.b.c = value` lines for a value. A table among them is spread
    /// into dotted keys when it has one key (`version.workspace = true`, as
    /// Cargo writes it) or is too long for a line, and is an inline table
    /// otherwise (`clap = { version = "4", features = ["derive"] }`).
    fn dotted<'v>(&mut self, keys: &mut Vec<&'v str>, v: &'v Value) -> Result<(), DataError> {
        if let Value::Object(entries) = v {
            let name_len: usize = keys.iter().map(|k| key(k).len() + 1).sum();
            let inline =
                entries.len() > 1 && inline_len(v, LINE.saturating_sub(name_len + 3)).is_some();
            if !entries.is_empty() && !inline {
                for (k, sub) in entries {
                    keys.push(k);
                    self.path.push(Seg::Key(k.clone()));
                    self.dotted(keys, sub)?;
                    self.path.pop();
                    keys.pop();
                }
                return Ok(());
            }
        }
        let name: Vec<String> = keys.iter().map(|k| key(k)).collect();
        self.put(Role::Key, &name.join("."))?;
        self.put(Role::Punct, " = ")?;
        self.inline(v, true)?;
        self.put(Role::Space, "\n")
    }

    /// A value on one line. `top` allows a multi-line string.
    fn inline(&mut self, v: &Value, top: bool) -> Result<(), DataError> {
        match v {
            Value::Null => Err(DataError::new(
                DataErrorKind::Unrepresentable,
                format!("TOML has no null, and {} is null", self.where_()),
            )
            .hint("leave the key out, or keep it in JSON or YAML, which have null")),
            Value::Bool(b) => self.put(Role::Bool, if *b { "true" } else { "false" }),
            Value::Number(n) => {
                if !n.fits_i64() {
                    return Err(DataError::new(
                        DataErrorKind::Unrepresentable,
                        format!(
                            "TOML's integers stop at 64 bits, and {} is {}",
                            self.where_(),
                            n.text()
                        ),
                    )
                    .hint("JSON and YAML keep it exactly; a TOML reader would have to round it"));
                }
                if overflows(n) {
                    return Err(DataError::new(
                        DataErrorKind::Unrepresentable,
                        format!(
                            "TOML's floats are 64-bit, and {} is {}, too large for one",
                            self.where_(),
                            n.text()
                        ),
                    )
                    .hint("JSON and YAML keep it exactly; a TOML reader would refuse it"));
                }
                self.put(Role::Num, n.text())
            }
            Value::String(s) if top && s.contains('\n') => self.put(Role::Str, &multi_line(s)),
            Value::String(s) => self.put(Role::Str, &basic(s)),
            Value::DateTime(d) => self.put(Role::Date, d),
            Value::Array(items) => {
                self.put(Role::Punct, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.put(Role::Punct, ", ")?;
                    }
                    self.path.push(Seg::Index(i));
                    self.inline(item, false)?;
                    self.path.pop();
                }
                self.put(Role::Punct, "]")
            }
            Value::Object(entries) => {
                if entries.is_empty() {
                    return self.put(Role::Punct, "{}");
                }
                self.put(Role::Punct, "{ ")?;
                for (i, (k, sub)) in entries.iter().enumerate() {
                    if i > 0 {
                        self.put(Role::Punct, ", ")?;
                    }
                    self.put(Role::Key, &key(k))?;
                    self.put(Role::Punct, " = ")?;
                    self.path.push(Seg::Key(k.clone()));
                    self.inline(sub, false)?;
                    self.path.pop();
                }
                self.put(Role::Punct, " }")
            }
        }
    }
}

/// How long a line an inline table may make.
const LINE: usize = 100;

/// The length of `v` written inline, if it is at most `max` and has
/// nothing TOML can't hold (whose error the dotted keys then name).
fn inline_len(v: &Value, max: usize) -> Option<usize> {
    let len = match v {
        Value::Null => return None,
        Value::Bool(b) => b.to_string().len(),
        Value::Number(n) if !n.fits_i64() || overflows(n) => return None,
        Value::Number(n) => n.text().len(),
        Value::String(s) => basic(s).len(),
        Value::DateTime(d) => d.len(),
        Value::Array(items) => {
            let mut len = 2;
            for (i, item) in items.iter().enumerate() {
                len += inline_len(item, max.checked_sub(len)?)? + if i > 0 { 2 } else { 0 };
            }
            len
        }
        Value::Object(entries) if entries.is_empty() => 2,
        Value::Object(entries) => {
            let mut len = 4;
            for (i, (k, sub)) in entries.iter().enumerate() {
                len += key(k).len() + 3 + inline_len(sub, max.checked_sub(len)?)?;
                if i > 0 {
                    len += 2;
                }
            }
            len
        }
    };
    (len <= max).then_some(len)
}

/// A float past what TOML's 64-bit floats hold, like `1e400`.
fn overflows(n: &Number) -> bool {
    matches!(n, Number::Float(t) if t.parse::<f64>().is_ok_and(f64::is_infinite))
}

/// A key bare when it can be (`A-Za-z0-9_-`), quoted otherwise.
fn key(k: &str) -> String {
    if !k.is_empty()
        && k.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        k.to_string()
    } else {
        basic(k)
    }
}

/// A basic string: `"..."` with escapes.
fn basic(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        push_escaped(&mut out, c, false);
    }
    out.push('"');
    out
}

/// A multi-line basic string, `"""` on lines of their own, so a script
/// stays readable.
fn multi_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    out.push_str("\"\"\"\n");
    let mut quotes = 0;
    for c in s.chars() {
        if c == '"' {
            quotes += 1;
            // Three in a row would close the string.
            if quotes == 3 {
                out.push_str("\\\"");
                quotes = 0;
                continue;
            }
        } else {
            quotes = 0;
        }
        push_escaped(&mut out, c, true);
    }
    // A quote just before the closing ones would join them.
    if out.ends_with('"') && !out.ends_with("\\\"") {
        out.pop();
        out.push_str("\\\"");
    }
    out.push_str("\"\"\"");
    out
}

fn push_escaped(out: &mut String, c: char, multi_line: bool) {
    match c {
        '"' if multi_line => out.push('"'),
        '"' => out.push_str("\\\""),
        '\\' => out.push_str("\\\\"),
        '\n' if multi_line => out.push('\n'),
        '\n' => out.push_str("\\n"),
        '\t' => out.push_str("\\t"),
        '\r' => out.push_str("\\r"),
        '\u{08}' => out.push_str("\\b"),
        '\u{0C}' => out.push_str("\\f"),
        c if (c as u32) < 0x20 || c == '\u{7F}' => out.push_str(&format!("\\u{:04X}", c as u32)),
        c => out.push(c),
    }
}

/// `servers[0].port`, or "the top level".
fn render_path(path: &[Seg]) -> String {
    if path.is_empty() {
        return "the top level".to_string();
    }
    let mut out = String::new();
    for seg in path {
        match seg {
            Seg::Index(i) => out.push_str(&format!("[{i}]")),
            Seg::Key(k) if is_ident(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            Seg::Key(k) => out.push_str(&format!("[{}]", super::json::quote(k))),
        }
    }
    out
}

pub(crate) fn is_ident(k: &str) -> bool {
    let mut chars = k.chars();
    chars
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_are_counted_outside_strings() {
        let text =
            "# a\nx = \"# not\" # b\ny = '''\n# not\n'''\nz = \"\"\"\n\\\"\"\" # not\n\"\"\"\n";
        assert_eq!(count_comments(text), 2);
    }

    #[test]
    fn multi_line_strings_never_close_early() {
        let s = multi_line("a\"\"\"b\"");
        let doc: toml_edit::Document<String> = format!("x = {s}\n").parse().unwrap();
        assert_eq!(doc["x"].as_str(), Some("a\"\"\"b\""));
    }

    #[test]
    fn keys_are_bare_only_when_they_can_be() {
        assert_eq!(key("a-b_1"), "a-b_1");
        assert_eq!(key("a.b"), "\"a.b\"");
        assert_eq!(key(""), "\"\"");
        assert_eq!(key("ü"), "\"ü\"");
    }
}
