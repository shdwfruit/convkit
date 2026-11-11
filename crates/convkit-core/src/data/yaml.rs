//! YAML, read with saphyr-parser's events into values, and written in
//! block style from events.
//!
//! Reading is safe: no tag ever makes anything but a string, number, true
//! or false, null, list or mapping. Plain scalars are read by the YAML 1.2
//! core schema. Aliases are expanded, with a cap so a "billion laughs" file
//! cannot exhaust memory, and merge keys (`<<`) are expanded as YAML 1.1
//! readers do. A file of several documents is read as all of them.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use saphyr_parser::{Event, Marker, Parser, ScalarStyle, Span, Tag};

use super::error::{DataError, DataErrorKind, Pos};
use super::out::{Out, Role};
use super::value::{Number, Scalar, Value};
use super::{Budget, Emit, ALIAS_LIMIT, MAX_DEPTH, VALUE_COST};

/// What reading found besides the documents, for the notes.
#[derive(Debug, Default)]
pub(crate) struct Found {
    pub comments: usize,
    pub aliases: bool,
    pub merges: bool,
    /// Tags dropped in lenient reading, as written (`!Ref`).
    pub dropped_tags: Vec<String>,
    pub binary: bool,
    /// Plain `yes`, `no`, `on` or `off` values, which YAML 1.1 reads as
    /// true or false.
    pub yaml11_bools: bool,
    /// Plain numbers with a leading 0 (`0644`), which YAML 1.1 reads as
    /// octal.
    pub yaml11_octal: bool,
}

enum Node {
    Seq {
        items: Vec<Value>,
        anchor: usize,
    },
    Map {
        entries: Vec<(String, Value)>,
        keys: HashMap<String, Marker>,
        /// The key whose value comes next, where it is, and whether it is a
        /// plain `<<`.
        key: Option<(String, Marker, bool)>,
        /// Merges, by where in `entries` they go.
        merges: Vec<(usize, Value, Marker)>,
        anchor: usize,
    },
}

struct Builder<'t> {
    text: &'t str,
    lenient: bool,
    stack: Vec<Node>,
    /// Each anchor's value, and what it holds.
    anchors: HashMap<usize, (Value, Held)>,
    expanded: usize,
    docs: Vec<Value>,
    root: Option<Value>,
    found: Found,
}

/// Reads every document in `text`.
pub(crate) fn read(
    text: &str,
    lenient: bool,
    budget: &mut Budget,
) -> Result<(Vec<Value>, Found), DataError> {
    let mut b = Builder {
        text,
        lenient,
        stack: Vec::new(),
        anchors: HashMap::new(),
        expanded: 0,
        docs: Vec::new(),
        root: None,
        found: Found::default(),
    };
    // Where each quoted or block scalar is, so a `#` inside one is not
    // counted as a comment.
    let mut scalars: Vec<(usize, usize, ScalarStyle)> = Vec::new();
    let mut parser = Parser::new_from_str(text);
    while let Some(next) = parser.next_event() {
        let (event, span) = next.map_err(|e| scan_error(text, &e))?;
        match event {
            Event::StreamStart | Event::StreamEnd | Event::Nothing => {}
            Event::DocumentStart(_) => b.root = None,
            Event::DocumentEnd => {
                let root = b.root.take().unwrap_or(Value::Null);
                b.docs.push(root);
            }
            Event::Scalar(value, style, anchor, tag) => {
                if style != ScalarStyle::Plain {
                    scalars.push((span.start.index(), span.end.index(), style));
                }
                b.scalar(value, style, anchor, tag, span, budget)?;
            }
            Event::SequenceStart(anchor, tag) => {
                b.check_tag(tag.as_deref(), &["seq", "omap", "pairs"], span)?;
                b.open(span)?;
                b.stack.push(Node::Seq {
                    items: Vec::new(),
                    anchor,
                });
            }
            Event::MappingStart(anchor, tag) => {
                b.check_tag(tag.as_deref(), &["map", "set"], span)?;
                b.open(span)?;
                b.stack.push(Node::Map {
                    entries: Vec::new(),
                    keys: HashMap::new(),
                    key: None,
                    merges: Vec::new(),
                    anchor,
                });
            }
            Event::SequenceEnd => {
                let Some(Node::Seq { items, anchor }) = b.stack.pop() else {
                    unreachable!("a sequence ends after it starts");
                };
                if !budget.value(&Value::Null) {
                    return Err(budget.exceeded());
                }
                b.done(Value::Array(items), anchor, span, budget)?;
            }
            Event::MappingEnd => {
                let Some(Node::Map {
                    entries,
                    merges,
                    anchor,
                    ..
                }) = b.stack.pop()
                else {
                    unreachable!("a mapping ends after it starts");
                };
                let merged = b.merge(entries, merges)?;
                if !budget.value(&Value::Null) {
                    return Err(budget.exceeded());
                }
                b.done(Value::Object(merged), anchor, span, budget)?;
            }
            Event::Alias(id) => {
                let Some(&(_, held)) = b.anchors.get(&id) else {
                    return Err(b
                        .error(
                            span.start,
                            "this alias names an anchor that isn't defined above it",
                        )
                        .hint("an alias *name needs an &name earlier in the file"));
                };
                if b.stack.len() + held.depth > MAX_DEPTH {
                    return Err(b.too_deep(span));
                }
                b.expanded += held.nodes;
                if b.expanded > ALIAS_LIMIT {
                    return Err(DataError {
                        kind: DataErrorKind::TooBig,
                        ..b.error(
                            span.start,
                            format!(
                                "the aliases here expand to more than {} values",
                                group(ALIAS_LIMIT)
                            ),
                        )
                    }
                    .hint(
                        "a file whose aliases multiply like this (a \"billion laughs\") exhausts memory, so conv stops reading it",
                    ));
                }
                if !budget.charge(held.cost()) {
                    return Err(budget.exceeded());
                }
                b.found.aliases = true;
                let value = b.anchors[&id].0.clone();
                b.place(value, span)?;
            }
        }
    }
    b.found.comments = count_comments(text, &scalars);
    Ok((b.docs, b.found))
}

/// `1,000,000`.
fn group(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl Builder<'_> {
    fn pos(&self, m: Marker) -> Pos {
        marker_pos(self.text, m)
    }

    fn error(&self, m: Marker, message: impl Into<String>) -> DataError {
        DataError::syntax(self.pos(m), message)
    }

    fn open(&mut self, span: Span) -> Result<(), DataError> {
        if let Some(Node::Map { key: None, .. }) = self.stack.last() {
            return Err(self
                .error(span.start, "a key that is a list or a mapping")
                .hint("JSON, TOML and CSV keys are text, so conv can't carry it over"));
        }
        if self.stack.len() >= MAX_DEPTH {
            return Err(self.too_deep(span));
        }
        Ok(())
    }

    fn too_deep(&self, span: Span) -> DataError {
        DataError {
            kind: DataErrorKind::TooBig,
            ..self.error(span.start, format!("nested more than {MAX_DEPTH} deep"))
        }
    }

    /// A tag on a list or mapping: its own kind, or one conv drops or
    /// refuses.
    fn check_tag(&mut self, tag: Option<&Tag>, own: &[&str], span: Span) -> Result<(), DataError> {
        match tag {
            None => Ok(()),
            Some(t) if t.is_yaml_core_schema() && own.contains(&t.suffix.as_str()) => Ok(()),
            Some(t) if t.handle.is_empty() && t.suffix == "!" => Ok(()),
            Some(t) => self.foreign_tag(t, span),
        }
    }

    /// A tag only the program that wrote the file knows: an error, or in
    /// lenient reading dropped (the value is kept) and noted.
    fn foreign_tag(&mut self, t: &Tag, span: Span) -> Result<(), DataError> {
        let shown = shown_tag(t);
        if !self.lenient {
            return Err(self
                .error(span.start, format!("the tag {shown}"))
                .hint("a tag means something only to the program that reads this file, and conv can't keep it; conv fmt --lenient drops tags and keeps the values"));
        }
        if !self.found.dropped_tags.contains(&shown) {
            self.found.dropped_tags.push(shown);
        }
        Ok(())
    }

    fn scalar(
        &mut self,
        text: Cow<str>,
        style: ScalarStyle,
        anchor: usize,
        tag: Option<Cow<Tag>>,
        span: Span,
        budget: &mut Budget,
    ) -> Result<(), DataError> {
        let plain = style == ScalarStyle::Plain;
        let value = match tag.as_deref() {
            None if plain => {
                if matches!(
                    text.as_ref(),
                    "yes"
                        | "Yes"
                        | "YES"
                        | "no"
                        | "No"
                        | "NO"
                        | "on"
                        | "On"
                        | "ON"
                        | "off"
                        | "Off"
                        | "OFF"
                ) && self.wants_value()
                {
                    self.found.yaml11_bools = true;
                }
                let t = text.as_bytes();
                if t.len() > 1
                    && t[0] == b'0'
                    && t.iter().all(u8::is_ascii_digit)
                    && self.wants_value()
                {
                    self.found.yaml11_octal = true;
                }
                resolve_plain(&text)
            }
            None => Value::String(text.to_string()),
            Some(t) if t.handle.is_empty() && t.suffix == "!" => Value::String(text.to_string()),
            Some(t) if t.is_yaml_core_schema() => match t.suffix.as_str() {
                "str" => Value::String(text.to_string()),
                "null" => Value::Null,
                "bool" => match text.as_ref() {
                    "true" | "True" | "TRUE" => Value::Bool(true),
                    "false" | "False" | "FALSE" => Value::Bool(false),
                    _ => return Err(self.bad_tagged(span, "!!bool", &text)),
                },
                "int" => match Number::from_yaml(&text) {
                    Some(n @ Number::Int(_)) => Value::Number(n),
                    _ => return Err(self.bad_tagged(span, "!!int", &text)),
                },
                "float" => match Number::from_yaml(&text) {
                    Some(Number::Int(t)) => Value::Number(Number::Float(format!("{t}.0"))),
                    Some(n) => Value::Number(n),
                    None => return Err(self.bad_tagged(span, "!!float", &text)),
                },
                "timestamp" => match datetime(&text) {
                    true => Value::DateTime(text.to_string()),
                    false => Value::String(text.to_string()),
                },
                "binary" => {
                    self.found.binary = true;
                    Value::String(text.to_string())
                }
                _ => {
                    let t = t.clone();
                    self.foreign_tag(&t, span)?;
                    resolve_or_string(&text, plain)
                }
            },
            Some(t) => {
                let t = t.clone();
                self.foreign_tag(&t, span)?;
                resolve_or_string(&text, plain)
            }
        };
        // A key is its text as written, whatever it would mean as a value.
        if !self.wants_value() {
            let merge = plain && text == "<<";
            if !merge {
                let first = match self.stack.last() {
                    Some(Node::Map { keys, .. }) => keys.get(text.as_ref()).copied(),
                    _ => None,
                };
                if let Some(first) = first {
                    return Err(self.duplicate(&text, span.start, first));
                }
            }
            if !budget.key(&text) {
                return Err(budget.exceeded());
            }
            if anchor != 0 {
                let key = Value::String(text.to_string());
                self.keep(anchor, key, budget)?;
            }
            if let Some(Node::Map { key, keys, .. }) = self.stack.last_mut() {
                if !merge {
                    keys.insert(text.to_string(), span.start);
                }
                *key = Some((text.into_owned(), span.start, merge));
            }
            return Ok(());
        }
        if !budget.value(&value) {
            return Err(budget.exceeded());
        }
        self.done(value, anchor, span, budget)
    }

    /// Whether the next node is a value rather than a mapping's key.
    fn wants_value(&self) -> bool {
        !matches!(self.stack.last(), Some(Node::Map { key: None, .. }))
    }

    fn bad_tagged(&self, span: Span, tag: &str, text: &str) -> DataError {
        self.error(span.start, format!("{tag} on {text:?}, which isn't one"))
    }

    fn duplicate(&self, key: &str, at: Marker, first: Marker) -> DataError {
        self.error(at, format!("the key {key:?} is in this mapping twice"))
            .hint(format!(
                "the first is at {}; readers keep one or the other, so conv won't guess: remove or rename one",
                self.pos(first).at()
            ))
    }

    /// A finished node: kept for its anchor, and placed in what holds it.
    fn done(
        &mut self,
        value: Value,
        anchor: usize,
        span: Span,
        budget: &mut Budget,
    ) -> Result<(), DataError> {
        if anchor != 0 {
            self.keep(anchor, value.clone(), budget)?;
        }
        self.place(value, span)
    }

    /// Keeps a copy of an anchor's value, which costs memory like any other.
    fn keep(&mut self, anchor: usize, value: Value, budget: &mut Budget) -> Result<(), DataError> {
        let held = count(&value);
        if !budget.charge(held.cost()) {
            return Err(budget.exceeded());
        }
        self.anchors.insert(anchor, (value, held));
        Ok(())
    }

    fn place(&mut self, value: Value, span: Span) -> Result<(), DataError> {
        match self.stack.last_mut() {
            None => {
                self.root = Some(value);
                Ok(())
            }
            Some(Node::Seq { items, .. }) => {
                items.push(value);
                Ok(())
            }
            Some(Node::Map { key: None, .. }) => {
                // An alias used as a key: its text, if it is a scalar.
                let text = match &value {
                    Value::String(s) | Value::DateTime(s) => s.clone(),
                    Value::Number(n) => n.text().to_string(),
                    Value::Bool(b) => b.to_string(),
                    Value::Null => "null".to_string(),
                    Value::Array(_) | Value::Object(_) => {
                        return Err(self
                            .error(span.start, "a key that is a list or a mapping")
                            .hint("JSON, TOML and CSV keys are text, so conv can't carry it over"))
                    }
                };
                let dup = match self.stack.last() {
                    Some(Node::Map { keys, .. }) => keys.get(&text).copied(),
                    _ => None,
                };
                if let Some(first) = dup {
                    return Err(self.duplicate(&text, span.start, first));
                }
                if let Some(Node::Map { key, keys, .. }) = self.stack.last_mut() {
                    keys.insert(text.clone(), span.start);
                    *key = Some((text, span.start, false));
                }
                Ok(())
            }
            Some(Node::Map {
                entries,
                key: key @ Some(_),
                merges,
                ..
            }) => {
                let (k, at, merge) = key.take().expect("matched Some");
                if merge {
                    merges.push((entries.len(), value, at));
                } else {
                    entries.push((k, value));
                }
                Ok(())
            }
        }
    }

    /// A mapping with its merge keys expanded: each `<<` adds the keys of
    /// the mapping (or mappings, earlier ones first) it names that the
    /// mapping does not set itself, where the `<<` was.
    fn merge(
        &mut self,
        entries: Vec<(String, Value)>,
        merges: Vec<(usize, Value, Marker)>,
    ) -> Result<Vec<(String, Value)>, DataError> {
        if merges.is_empty() {
            return Ok(entries);
        }
        self.found.merges = true;
        let own: HashSet<String> = entries.iter().map(|(k, _)| k.clone()).collect();
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::with_capacity(entries.len());
        let mut entries = entries.into_iter().enumerate().peekable();
        for (at, source, marker) in merges {
            while let Some((_, e)) = entries.next_if(|(i, _)| *i < at) {
                out.push(e);
            }
            let sources = match source {
                Value::Object(m) => vec![m],
                Value::Array(items) => {
                    let mut maps = Vec::new();
                    for item in items {
                        match item {
                            Value::Object(m) => maps.push(m),
                            _ => return Err(self.bad_merge(marker)),
                        }
                    }
                    maps
                }
                _ => return Err(self.bad_merge(marker)),
            };
            for map in sources {
                for (k, v) in map {
                    if !own.contains(&k) && seen.insert(k.clone()) {
                        out.push((k, v));
                    }
                }
            }
        }
        out.extend(entries.map(|(_, e)| e));
        Ok(out)
    }

    fn bad_merge(&self, at: Marker) -> DataError {
        self.error(at, "a merge key (<<) whose value isn't a mapping")
            .hint("<< takes a mapping, usually an alias like *defaults, or a list of them")
    }
}

/// How a tag is written in the file: `!Ref`, `!!python/object`.
fn shown_tag(t: &Tag) -> String {
    if t.is_yaml_core_schema() {
        format!("!!{}", t.suffix)
    } else if t.handle == "!" {
        format!("!{}", t.suffix)
    } else {
        format!("{}{}", t.handle, t.suffix)
    }
}

fn resolve_or_string(text: &str, plain: bool) -> Value {
    if plain {
        resolve_plain(text)
    } else {
        Value::String(text.to_string())
    }
}

/// A plain scalar by the YAML 1.2 core schema: null, true or false, a
/// number, or a string. A date or time TOML could hold is kept as one.
pub(crate) fn resolve_plain(s: &str) -> Value {
    match s {
        "" | "~" | "null" | "Null" | "NULL" => return Value::Null,
        "true" | "True" | "TRUE" => return Value::Bool(true),
        "false" | "False" | "FALSE" => return Value::Bool(false),
        _ => {}
    }
    if let Some(n) = Number::from_yaml(s) {
        return Value::Number(n);
    }
    if datetime(s) {
        return Value::DateTime(s.to_string());
    }
    Value::String(s.to_string())
}

/// Whether `s` is a date, time or date-time as TOML writes them.
pub(crate) fn datetime(s: &str) -> bool {
    let b = s.as_bytes();
    let looks = b.len() >= 8
        && b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && (b[2] == b':' || (b.len() >= 10 && b[4] == b'-'));
    looks && s.parse::<toml_datetime::Datetime>().is_ok()
}

/// What a value holds, so each copy of it can be charged.
#[derive(Debug, Clone, Copy)]
struct Held {
    /// Values, itself included.
    nodes: usize,
    /// How deep its lists and mappings nest: 0 for a scalar.
    depth: usize,
    /// Bytes of text, keys included.
    text: usize,
}

impl Held {
    fn cost(self) -> usize {
        self.nodes * VALUE_COST + self.text
    }
}

fn count(v: &Value) -> Held {
    let add = |a: Held, b: Held| Held {
        nodes: a.nodes + b.nodes,
        depth: a.depth.max(b.depth + 1),
        text: a.text + b.text,
    };
    let scalar = |text| Held {
        nodes: 1,
        depth: 0,
        text,
    };
    let open = Held {
        nodes: 1,
        depth: 1,
        text: 0,
    };
    match v {
        Value::Array(items) => items.iter().map(count).fold(open, add),
        Value::Object(entries) => entries
            .iter()
            .map(|(k, v)| {
                let held = count(v);
                Held {
                    text: held.text + k.len() + std::mem::size_of::<String>(),
                    ..held
                }
            })
            .fold(open, add),
        Value::String(s) | Value::DateTime(s) => scalar(s.len()),
        Value::Number(n) => scalar(n.text().len()),
        Value::Bool(_) | Value::Null => scalar(0),
    }
}

fn marker_pos(text: &str, m: Marker) -> Pos {
    let offset = text
        .char_indices()
        .nth(m.index())
        .map_or(text.len(), |(i, _)| i);
    Pos {
        line: m.line(),
        column: m.col() + 1,
        offset,
    }
}

fn scan_error(text: &str, e: &saphyr_parser::ScanError) -> DataError {
    let mut pos = marker_pos(text, *e.marker());
    let info = e.info();
    let here = text[pos.offset..].chars().next();
    let line_start = text[..pos.offset].rfind('\n').map_or(0, |i| i + 1);
    let indent_has_tab = text[line_start..]
        .chars()
        .take_while(|c| c.is_whitespace() && *c != '\n')
        .any(|c| c == '\t');
    let mut message = info.to_string();
    let hint = if here == Some('\t') || indent_has_tab {
        message = "a tab where YAML expects spaces".to_string();
        // The caret goes on the tab itself.
        if let Some(tab) = text[line_start..].find('\t').filter(|_| here != Some('\t')) {
            pos.offset = line_start + tab;
            pos.column = text[line_start..pos.offset].chars().count() + 1;
        }
        Some("YAML indents with spaces only; replace the tab")
    } else if matches!(here, Some('@' | '`')) {
        Some("@ and ` can't start a plain value; put the value in quotes")
    } else if info.contains("mapping values are not allowed") {
        Some("a value with \": \" in it needs quotes, or this line is indented too far")
    } else if info.contains("did not find expected key")
        || info.contains("did not find expected '-'")
        || info.contains("expected <block end>")
    {
        Some("check this line's indentation against the lines above it")
    } else if info.contains("did not find expected ',' or ']'") {
        Some("a [ list on one line needs commas between items and a closing ]")
    } else if info.contains("did not find expected ',' or '}'") {
        Some("a { mapping on one line needs commas between items and a closing }")
    } else if info.contains("unexpected end of stream") || info.contains("found unexpected end") {
        Some("a quote or bracket is never closed")
    } else if info.contains("while scanning an anchor") || info.contains("while scanning an alias")
    {
        Some("an & or * must be followed by a name")
    } else {
        None
    };
    let e = DataError::syntax(pos, message);
    match hint {
        Some(h) => e.hint(h),
        None => e,
    }
}

/// Comments: a `#` at the start of a line or after a space or tab, outside
/// every quoted and block scalar. A block scalar's span is exact; a quoted
/// one's runs on over a comment after it, so its end is found here, at its
/// closing quote.
fn count_comments(text: &str, scalars: &[(usize, usize, ScalarStyle)]) -> usize {
    let mut spans = scalars.iter().peekable();
    let mut count = 0;
    let mut prev = '\n';
    // Inside a quoted scalar: its quote, and whether the last character
    // was a backslash (double) or a quote (single, where '' is a quote).
    let mut quoted: Option<(char, bool)> = None;
    let mut block_end = 0;
    let mut chars = text.chars().enumerate().peekable();
    while let Some((i, c)) = chars.next() {
        if let Some((q, escaped)) = quoted {
            quoted = match (q, c) {
                ('"', _) if escaped => Some(('"', false)),
                ('"', '\\') => Some(('"', true)),
                ('"', '"') => None,
                ('\'', '\'') if chars.peek().is_some_and(|(_, n)| *n == '\'') => {
                    chars.next();
                    Some(('\'', false))
                }
                ('\'', '\'') => None,
                _ => Some((q, false)),
            };
            prev = c;
            continue;
        }
        while let Some(&&(start, end, style)) = spans.peek() {
            if start > i {
                break;
            }
            spans.next();
            match style {
                ScalarStyle::SingleQuoted | ScalarStyle::DoubleQuoted if start == i => {
                    quoted = Some((c, false));
                }
                ScalarStyle::Literal | ScalarStyle::Folded => block_end = block_end.max(end),
                _ => {}
            }
        }
        if quoted.is_some() {
            prev = c;
            continue;
        }
        if c == '#' && i >= block_end && matches!(prev, '\n' | ' ' | '\t' | '\r') {
            count += 1;
        }
        prev = c;
    }
    count
}

/// YAML out, in block style: `key: value`, `- item`, nested blocks
/// indented by `step` spaces, and a literal block (`|`) for text of
/// several lines.
pub(crate) struct Writer<'o> {
    out: &'o mut dyn Out,
    step: usize,
    stack: Vec<Frame>,
    /// Where the next node goes.
    at: Place,
    documents: usize,
}

#[derive(Clone, Copy)]
struct Frame {
    object: bool,
    /// The column its entries start at.
    indent: usize,
    count: usize,
    /// How the frame was opened, which says where an empty one's `{}` or
    /// `[]` goes and whether its first entry needs a new line.
    opened: Place,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    /// A document's top: column 0, at the start of a line.
    Top,
    /// After `key:`, in a mapping at this indent.
    Value(usize),
    /// After `- `, in a list at this indent.
    Item(usize),
}

impl<'o> Writer<'o> {
    pub fn new(out: &'o mut dyn Out, step: usize) -> Writer<'o> {
        Writer {
            out,
            step,
            stack: Vec::new(),
            at: Place::Top,
            documents: 0,
        }
    }

    fn put(&mut self, role: Role, s: &str) -> Result<(), DataError> {
        self.out
            .put(role, s)
            .map_err(|e| DataError::io(&e, "write the result"))
    }

    /// Starts an entry of the innermost frame: a line break and indent,
    /// except for a first entry that follows `- ` or opens the document.
    fn entry(&mut self) -> Result<(), DataError> {
        let frame = self.stack.last_mut().expect("an entry is in a frame");
        let first = frame.count == 0;
        frame.count += 1;
        let indent = frame.indent;
        let inline = first && matches!(frame.opened, Place::Top | Place::Item(_));
        if !inline {
            self.put(Role::Space, &format!("\n{}", " ".repeat(indent)))?;
        }
        Ok(())
    }

    /// Before any node: the `- ` that makes it a list item.
    fn before_node(&mut self) -> Result<(), DataError> {
        if let Some(frame) = self.stack.last() {
            if !frame.object {
                let indent = frame.indent;
                self.entry()?;
                self.put(Role::Punct, "- ")?;
                self.at = Place::Item(indent);
            }
        }
        Ok(())
    }

    /// The indent of what a node placed at `at` nests.
    fn child_indent(&self, at: Place) -> usize {
        match at {
            Place::Top => 0,
            Place::Value(i) => i + self.step,
            Place::Item(i) => i + 2,
        }
    }

    fn after_node(&mut self) -> Result<(), DataError> {
        if self.stack.is_empty() {
            self.put(Role::Space, "\n")?;
            self.at = Place::Top;
        }
        Ok(())
    }
}

impl Emit for Writer<'_> {
    fn document(&mut self) -> Result<(), DataError> {
        if self.documents > 0 {
            self.put(Role::Punct, "---\n")?;
        }
        self.documents += 1;
        self.at = Place::Top;
        Ok(())
    }

    fn begin(&mut self, object: bool) -> Result<(), DataError> {
        self.before_node()?;
        let at = self.at;
        let indent = self.child_indent(at);
        self.stack.push(Frame {
            object,
            indent,
            count: 0,
            opened: at,
        });
        Ok(())
    }

    fn key(&mut self, key: &str) -> Result<(), DataError> {
        self.entry()?;
        let indent = self.stack.last().map_or(0, |f| f.indent);
        // An implicit key is at most 1024 characters; a longer one is
        // written as an explicit `? key`. Quoting can double its length.
        let k = key_text(key);
        if k.chars().count() > 1000 {
            self.put(Role::Punct, "? ")?;
            self.put(Role::Key, &double_quoted(key))?;
            self.put(Role::Space, &format!("\n{}", " ".repeat(indent)))?;
            self.put(Role::Punct, ":")?;
        } else {
            self.put(Role::Key, &k)?;
            self.put(Role::Punct, ":")?;
        }
        self.at = Place::Value(indent);
        Ok(())
    }

    fn end(&mut self) -> Result<(), DataError> {
        let frame = self.stack.pop().expect("end follows a begin");
        if frame.count == 0 {
            let empty = if frame.object { "{}" } else { "[]" };
            if let Place::Value(_) = frame.opened {
                self.put(Role::Space, " ")?;
            }
            self.put(Role::Punct, empty)?;
        }
        self.after_node()
    }

    fn scalar(&mut self, s: Scalar) -> Result<(), DataError> {
        self.before_node()?;
        let at = self.at;
        if let Place::Value(_) = at {
            self.put(Role::Space, " ")?;
        }
        match s {
            Scalar::Null => self.put(Role::Null, "null")?,
            Scalar::Bool(b) => self.put(Role::Bool, if b { "true" } else { "false" })?,
            Scalar::Number(n) => {
                let text = match n {
                    Number::Int(t) | Number::Float(t) => t.as_str(),
                    Number::Inf { negative: false } => ".inf",
                    Number::Inf { negative: true } => "-.inf",
                    Number::NaN => ".nan",
                };
                self.put(Role::Num, text)?;
            }
            Scalar::DateTime(t) => {
                // A time of day alone is quoted: YAML 1.1 would read
                // 07:32:00 as a number in base 60.
                if t.as_bytes().get(2) == Some(&b':') {
                    self.put(Role::Date, &single_quoted(t))?;
                } else {
                    self.put(Role::Date, t)?;
                }
            }
            Scalar::Str(t) => match style(t) {
                Style::Plain => self.put(Role::Str, t)?,
                Style::Single => self.put(Role::Str, &single_quoted(t))?,
                Style::Double => self.put(Role::Str, &double_quoted(t))?,
                Style::Literal => {
                    let indent = match at {
                        Place::Top => self.step,
                        other => self.child_indent(other),
                    };
                    let chomp = if !t.ends_with('\n') {
                        "-"
                    } else if t.ends_with("\n\n") {
                        "+"
                    } else {
                        ""
                    };
                    self.put(Role::Punct, &format!("|{chomp}"))?;
                    let body = t.strip_suffix('\n').unwrap_or(t);
                    let pad = " ".repeat(indent);
                    for line in body.split('\n') {
                        if line.is_empty() {
                            self.put(Role::Space, "\n")?;
                        } else {
                            self.put(Role::Space, &format!("\n{pad}"))?;
                            self.put(Role::Str, line)?;
                        }
                    }
                }
            },
        }
        self.after_node()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Style {
    Plain,
    Single,
    Double,
    Literal,
}

/// How a string is written so it reads back as that string, in YAML 1.2
/// and in the YAML 1.1 most tools still read.
fn style(s: &str) -> Style {
    style_for(s, false)
}

/// `style`, for a mapping's key or a value. A key is quoted for what YAML
/// 1.2 would read otherwise; a value for YAML 1.1 too, whose yes, no, on
/// and off are true and false (`on:` keeps GitHub Actions' look).
fn style_for(s: &str, key: bool) -> Style {
    let all_printable = s.chars().all(|c| printable(c) && c != '\t');
    if s.contains('\n') {
        let block_ok = s.chars().all(|c| printable(c) || c == '\n' || c == '\t')
            && !s.contains('\r')
            && s.trim().chars().next().is_some()
            && s.split('\n')
                .find(|l| !l.is_empty())
                .is_some_and(|l| !l.starts_with([' ', '\t']))
            && !s.split('\n').any(|l| l.ends_with([' ', '\t']));
        return if block_ok {
            Style::Literal
        } else {
            Style::Double
        };
    }
    if !all_printable {
        return Style::Double;
    }
    if plain_ok(s, key) {
        Style::Plain
    } else {
        Style::Single
    }
}

fn key_text(key: &str) -> String {
    match style_for(key, true) {
        Style::Plain => key.to_string(),
        Style::Single => single_quoted(key),
        Style::Double | Style::Literal => double_quoted(key),
    }
}

/// YAML's printable characters, less the ones some readers take as line
/// breaks (U+0085, U+2028, U+2029) and the byte-order mark.
fn printable(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{7E}' | '\u{A0}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
        && !matches!(c, '\u{2028}' | '\u{2029}' | '\u{FEFF}')
}

/// Whether a one-line string can go unquoted: nothing YAML would read as
/// another type, an indicator, or a comment.
fn plain_ok(s: &str, key: bool) -> bool {
    let Some(first) = s.chars().next() else {
        return false;
    };
    if s.starts_with([' ', '\t']) || s.ends_with([' ', '\t', ':']) {
        return false;
    }
    let second = s.chars().nth(1);
    let indicator = match first {
        '-' | '?' | ':' => second.is_none_or(|c| c == ' '),
        ',' | '[' | ']' | '{' | '}' | '#' | '&' | '*' | '!' | '|' | '>' | '\'' | '"' | '%'
        | '@' | '`' => true,
        _ => false,
    };
    if indicator
        || s.contains(": ")
        || s.contains(" #")
        || s.starts_with("---")
        || s.starts_with("...")
    {
        return false;
    }
    !looks_typed(s, key)
}

/// Whether YAML would read `s` as something other than a string: by the
/// 1.2 core schema (null, true or false, a number, and the dates conv reads
/// as dates), or by 1.1 (`1_000`, `0777`, `1:30`, `3.`, a date, `<<`, and
/// for a value yes, no, on, off, y and n). `0.6.2` and `18.x` stay plain.
fn looks_typed(s: &str, key: bool) -> bool {
    if !matches!(resolve_plain(s), Value::String(_)) || matches!(s, "<<" | "=") {
        return true;
    }
    const WORDS: [&str; 16] = [
        "y", "Y", "yes", "Yes", "YES", "n", "N", "no", "No", "NO", "on", "On", "ON", "off", "Off",
        "OFF",
    ];
    if !key && WORDS.contains(&s) {
        return true;
    }
    let t = s.strip_prefix(['+', '-']).unwrap_or(s);
    let digits =
        |x: &str, ok: fn(u8) -> bool| !x.is_empty() && x.bytes().all(|b| ok(b) || b == b'_');
    let dec = |b: u8| b.is_ascii_digit();
    if t.starts_with(|c: char| c.is_ascii_digit()) && digits(t, dec) {
        return true;
    }
    for (prefix, ok) in [
        ("0b", (|b: u8| matches!(b, b'0' | b'1')) as fn(u8) -> bool),
        ("0x", |b: u8| b.is_ascii_hexdigit()),
        ("0o", |b: u8| matches!(b, b'0'..=b'7')),
    ] {
        if t.strip_prefix(prefix).is_some_and(|r| digits(r, ok)) {
            return true;
        }
    }
    // Base 60: 1:30, 190:20:30.5.
    if let Some((head, rest)) = t.split_once(':') {
        let sixty = rest.split(':').enumerate().all(|(i, part)| {
            let part = if i == rest.split(':').count() - 1 {
                part.split_once('.').map_or(part, |(p, frac)| {
                    if digits(frac, dec) || frac.is_empty() {
                        p
                    } else {
                        "x"
                    }
                })
            } else {
                part
            };
            (1..=2).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_digit())
        });
        if digits(head, dec) && head.starts_with(|c: char| c.is_ascii_digit()) && sixty {
            return true;
        }
    }
    // A float as YAML 1.1 readers write it: 1., 1.5, .5, 1.5e+3.
    let (mantissa, exponent) = match t.find(['e', 'E']) {
        Some(at) => (&t[..at], Some(&t[at + 1..])),
        None => (t, None),
    };
    if let Some((int, frac)) = mantissa.split_once('.') {
        let int_ok =
            int.is_empty() || (int.starts_with(|c: char| c.is_ascii_digit()) && digits(int, dec));
        let frac_ok = frac.is_empty() || digits(frac, dec);
        let exp_ok = exponent.is_none_or(|e| {
            e.strip_prefix(['+', '-'])
                .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        });
        if int_ok && frac_ok && exp_ok && !(int.is_empty() && frac.is_empty()) {
            return true;
        }
    }
    // A date, as YAML 1.1 reads one: 2001-12-14, with or without a time.
    let b = s.as_bytes();
    b.len() >= 8
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5].is_ascii_digit()
        && (b[6] == b'-' || (b[6].is_ascii_digit() && b.get(7) == Some(&b'-')))
}

fn single_quoted(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn double_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if !printable(c) => {
                let n = c as u32;
                if n <= 0xFF {
                    out.push_str(&format!("\\x{n:02X}"));
                } else if n <= 0xFFFF {
                    out.push_str(&format!("\\u{n:04X}"));
                } else {
                    out.push_str(&format!("\\U{n:08X}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_that_would_read_as_something_else_are_quoted() {
        for typed in [
            "yes",
            "No",
            "ON",
            "null",
            "~",
            "true",
            "1",
            "-1",
            "1.5",
            ".5",
            "0x1F",
            "1e5",
            "2024-01-01",
            "1:30",
            "<<",
            "",
            " lead",
            "trail ",
            "a: b",
            "a #b",
            "- x",
            "#tag",
            "*star",
            "&amp",
            "!bang",
            "@at",
            "`tick",
            "'q",
            "\"q",
            "{x",
            "[x",
            "---",
            "...",
            "key:",
        ] {
            assert_ne!(style(typed), Style::Plain, "{typed:?}");
        }
        for plain in [
            "hello",
            "v1.2",
            "a:b",
            "a#b",
            "-x",
            "C#",
            "ünïcode",
            "with space",
            "user@host",
        ] {
            assert_eq!(style(plain), Style::Plain, "{plain:?}");
        }
    }

    #[test]
    fn multi_line_text_is_a_literal_block_when_it_can_be() {
        assert_eq!(style("a\nb\n"), Style::Literal);
        assert_eq!(style("a\nb"), Style::Literal);
        assert_eq!(
            style(" a\nb"),
            Style::Double,
            "a leading space needs an indicator"
        );
        assert_eq!(
            style("a \nb"),
            Style::Double,
            "a trailing space would be lost by editors"
        );
        assert_eq!(style("a\r\nb"), Style::Double);
        assert_eq!(style("\n"), Style::Double);
    }

    #[test]
    fn comments_inside_scalars_are_not_counted() {
        let text = "# one\na: \"x # no\" # two\nb: |\n  # no\nc: a#b\n";
        let (_, found) = read(text, false, &mut Budget::new()).unwrap();
        assert_eq!(found.comments, 2);
    }
}
