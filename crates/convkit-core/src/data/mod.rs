//! JSON, YAML, TOML and CSV, converted and formatted in process, with no
//! backend.
//!
//! Every format is read into one model (`Value`) or, where the file can be
//! large and the pair allows it, streamed as events straight from reader
//! to writer: JSON to JSON or YAML, a JSON list to CSV, and CSV to JSON or
//! YAML hold only what is open at each moment, whatever the file's size.
//! Everything else is read whole, up to `MEMORY_LIMIT`.
//!
//! Numbers keep their digits and keys their order. Nothing is changed to
//! fit a target that can't hold it: a null in TOML, NaN in JSON or a list
//! nested in a CSV cell is refused, naming where it is. What a conversion
//! drops (comments, YAML tags) or decides (a CSV column's type) is said in
//! a note.

mod csv;
mod error;
mod json;
mod out;
mod toml;
mod value;
mod yaml;

use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::Serialize;

pub use error::{DataError, DataErrorKind, Pos, Snippet};
pub use out::{Out, Plain, Role};
pub use value::{Number, Scalar, Value};

use crate::Format;

/// Deeper than this is refused rather than risk the stack.
pub(crate) const MAX_DEPTH: usize = 512;
/// How much of a file's values may be held at once, roughly, in bytes.
pub const MEMORY_LIMIT: usize = 512 * 1024 * 1024;
/// How many values YAML aliases may add, against "billion laughs" files.
pub(crate) const ALIAS_LIMIT: usize = 1_000_000;
/// What one value costs in memory before its text.
pub(crate) const VALUE_COST: usize = std::mem::size_of::<Value>() + 16;

/// The four data formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Json,
    Yaml,
    Toml,
    Csv,
}

impl Lang {
    pub fn of(f: Format) -> Option<Lang> {
        match f {
            Format::Json => Some(Lang::Json),
            Format::Yaml => Some(Lang::Yaml),
            Format::Toml => Some(Lang::Toml),
            Format::Csv => Some(Lang::Csv),
            _ => None,
        }
    }

    pub fn format(self) -> Format {
        match self {
            Lang::Json => Format::Json,
            Lang::Yaml => Format::Yaml,
            Lang::Toml => Format::Toml,
            Lang::Csv => Format::Csv,
        }
    }

    /// `JSON`, `YAML`, `TOML`, `CSV`.
    pub fn name(self) -> &'static str {
        match self {
            Lang::Json => "JSON",
            Lang::Yaml => "YAML",
            Lang::Toml => "TOML",
            Lang::Csv => "CSV",
        }
    }

    /// `json`, `yaml` (or `yml`), `toml`, `csv`.
    pub fn parse(s: &str) -> Option<Lang> {
        Format::from_ext(s).and_then(Lang::of)
    }
}

/// The pairs conv converts: every one between the four, but TOML and CSV.
pub fn converts(from: Lang, to: Lang) -> bool {
    from != to && refusal(from, to).is_none()
}

/// Why a pair of data formats can't be converted, if it can't.
fn refusal(from: Lang, to: Lang) -> Option<&'static str> {
    match (from, to) {
        (Lang::Csv, Lang::Toml) => Some(
            "a CSV is a list of rows, and a TOML document is a table: TOML has no top-level list",
        ),
        (Lang::Toml, Lang::Csv) => Some(
            "a TOML document is one table, and a CSV is a list of rows: TOML has no top-level list",
        ),
        (Lang::Csv, Lang::Csv) => Some("conv doesn't reformat a CSV"),
        _ => None,
    }
}

/// What to say for a data pair the registry doesn't list.
pub fn why_not(from: Format, to: Format) -> Option<String> {
    let (f, t) = (Lang::of(from)?, Lang::of(to)?);
    if f == t {
        return Some(match f {
            Lang::Csv => "it's already a CSV".to_string(),
            _ => format!(
                "it's already {}; to pretty-print, minify or check it, use conv fmt",
                f.name()
            ),
        });
    }
    refusal(f, t).map(str::to_string)
}

/// How to indent JSON or YAML.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indent {
    Spaces(u8),
    Tab,
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Read JSON5 and JSONC, and drop YAML tags conv can't keep, each with
    /// a note, rather than refuse them.
    pub lenient: bool,
    /// `None`: two spaces.
    pub indent: Option<Indent>,
    /// JSON on one line.
    pub minify: bool,
    pub sort_keys: bool,
    /// How notes name the input: `data.json`, or `the input`.
    pub name: String,
}

impl Options {
    pub fn new(name: impl Into<String>) -> Options {
        Options {
            lenient: false,
            indent: None,
            minify: false,
            sort_keys: false,
            name: name.into(),
        }
    }
}

/// Why a formatting option can't apply when writing `to`, if it can't.
pub fn option_problem(to: Lang, opts: &Options) -> Option<String> {
    if opts.minify && to != Lang::Json {
        return Some(format!(
            "--minify packs JSON onto one line, and {} needs its line breaks",
            to.name()
        ));
    }
    match (opts.indent, to) {
        (Some(Indent::Tab), Lang::Yaml) => {
            return Some("YAML can't indent with tabs; use --indent N".to_string())
        }
        (Some(_), Lang::Toml) => {
            return Some("TOML output isn't indented: its tables are [headers]".to_string())
        }
        (Some(_), Lang::Csv) => return Some("a CSV isn't indented".to_string()),
        _ => {}
    }
    if opts.sort_keys && to == Lang::Csv {
        return Some(
            "--sort-keys orders an object's keys; a CSV's columns follow its rows".to_string(),
        );
    }
    None
}

/// Where the text comes from: a file, which can be read again, or text
/// already in memory.
#[derive(Debug, Clone, Copy)]
pub enum Source<'a> {
    File(&'a Path),
    Text(&'a [u8]),
}

impl<'a> Source<'a> {
    pub(crate) fn open(&self) -> Result<Box<dyn Read + 'a>, DataError> {
        Ok(match *self {
            Source::File(p) => Box::new(
                std::fs::File::open(p)
                    .map_err(|e| DataError::io(&e, &format!("open {}", p.display())))?,
            ),
            Source::Text(t) => Box::new(t),
        })
    }

    /// The whole text, for the formats read whole.
    fn text(&self) -> Result<String, DataError> {
        let bytes = match *self {
            Source::File(p) => {
                let len = std::fs::metadata(p).map(|m| m.len() as usize).unwrap_or(0);
                if len > MEMORY_LIMIT / 2 {
                    return Err(Budget::new().exceeded());
                }
                std::fs::read(p).map_err(|e| DataError::io(&e, &format!("read {}", p.display())))?
            }
            Source::Text(t) => t.to_vec(),
        };
        let bytes = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
            Some(rest) => rest.to_vec(),
            None => bytes,
        };
        String::from_utf8(bytes).map_err(|e| {
            let good = &e.as_bytes()[..e.utf8_error().valid_up_to()];
            let line = good.iter().filter(|&&b| b == b'\n').count() + 1;
            DataError {
                line: Some(line),
                ..DataError::new(DataErrorKind::Syntax, "this isn't UTF-8 text")
            }
            .hint("save the file as UTF-8")
        })
    }

    fn starts_utf16(&self) -> Result<bool, DataError> {
        let mut head = [0u8; 2];
        let n = self
            .open()?
            .read(&mut head)
            .map_err(|e| DataError::io(&e, "read the text"))?;
        Ok(n == 2 && (head == [0xFF, 0xFE] || head == [0xFE, 0xFF]))
    }
}

/// What a transform found, besides its output.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub notes: Vec<String>,
    /// Comments in the input. No writer keeps them.
    pub comments: usize,
}

/// How much is held in memory, against `MEMORY_LIMIT`.
pub(crate) struct Budget {
    used: usize,
    limit: usize,
}

#[cfg(test)]
thread_local! {
    /// A smaller limit, so a test can show what is held and what streams
    /// without a file of half a gigabyte.
    pub(crate) static TEST_LIMIT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

impl Budget {
    pub fn new() -> Budget {
        #[cfg(test)]
        let limit = TEST_LIMIT.with(|l| l.get()).unwrap_or(MEMORY_LIMIT);
        #[cfg(not(test))]
        let limit = MEMORY_LIMIT;
        Budget { used: 0, limit }
    }

    /// Adds `bytes`; false once over the limit.
    pub fn charge(&mut self, bytes: usize) -> bool {
        self.used = self.used.saturating_add(bytes);
        self.used <= self.limit
    }

    pub fn value(&mut self, v: &Value) -> bool {
        let text = match v {
            Value::String(s) | Value::DateTime(s) => s.len(),
            Value::Number(Number::Int(t) | Number::Float(t)) => t.len(),
            _ => 0,
        };
        self.charge(VALUE_COST + text)
    }

    pub fn key(&mut self, k: &str) -> bool {
        self.charge(std::mem::size_of::<String>() + k.len())
    }

    /// The refusal once over the limit. It names no place: where reading
    /// stopped says nothing about the file.
    pub fn exceeded(&self) -> DataError {
        DataError::new(
            DataErrorKind::TooBig,
            format!(
                "this is too big to hold in memory: conv stops at {} MB of values",
                MEMORY_LIMIT >> 20
            ),
        )
        .hint("JSON to JSON, YAML or CSV, and CSV to JSON or YAML, stream at any size; YAML, TOML and --sort-keys read the whole file")
    }
}

/// A writer, fed one event at a time.
pub(crate) trait Emit {
    /// A new document begins (YAML writes `---` before all but the first).
    fn document(&mut self) -> Result<(), DataError> {
        Ok(())
    }
    fn begin(&mut self, object: bool) -> Result<(), DataError>;
    fn key(&mut self, key: &str) -> Result<(), DataError>;
    fn end(&mut self) -> Result<(), DataError>;
    fn scalar(&mut self, s: Scalar) -> Result<(), DataError>;
}

/// Takes events and writes nothing: a check.
struct Ignore;

impl Emit for Ignore {
    fn begin(&mut self, _: bool) -> Result<(), DataError> {
        Ok(())
    }
    fn key(&mut self, _: &str) -> Result<(), DataError> {
        Ok(())
    }
    fn end(&mut self) -> Result<(), DataError> {
        Ok(())
    }
    fn scalar(&mut self, _: Scalar) -> Result<(), DataError> {
        Ok(())
    }
}

/// Builds a value from events.
struct Tree<'b> {
    stack: Vec<(Value, Option<String>)>,
    key: Option<String>,
    root: Option<Value>,
    budget: &'b mut Budget,
}

impl Tree<'_> {
    fn place(&mut self, v: Value) -> Result<(), DataError> {
        if !self.budget.value(&v) {
            return Err(self.budget.exceeded());
        }
        match self.stack.last_mut() {
            None => self.root = Some(v),
            Some((Value::Array(items), _)) => items.push(v),
            Some((Value::Object(entries), _)) => {
                let k = self
                    .key
                    .take()
                    .expect("a value in an object follows its key");
                entries.push((k, v));
            }
            Some(_) => unreachable!("only lists and objects are open"),
        }
        Ok(())
    }
}

impl Emit for Tree<'_> {
    fn begin(&mut self, object: bool) -> Result<(), DataError> {
        let v = if object {
            Value::Object(Vec::new())
        } else {
            Value::Array(Vec::new())
        };
        let key = self.key.take();
        self.stack.push((v, key));
        Ok(())
    }
    fn key(&mut self, key: &str) -> Result<(), DataError> {
        if !self.budget.key(key) {
            return Err(self.budget.exceeded());
        }
        self.key = Some(key.to_string());
        Ok(())
    }
    fn end(&mut self) -> Result<(), DataError> {
        let (v, key) = self.stack.pop().expect("end follows a begin");
        self.key = key;
        self.place(v)
    }
    fn scalar(&mut self, s: Scalar) -> Result<(), DataError> {
        let v = match s {
            Scalar::Null => Value::Null,
            Scalar::Bool(b) => Value::Bool(b),
            Scalar::Number(n) => Value::Number(n.clone()),
            Scalar::Str(t) => Value::String(t.to_string()),
            Scalar::DateTime(t) => Value::DateTime(t.to_string()),
        };
        self.place(v)
    }
}

/// The path to what a writer is writing, for its messages.
#[derive(Debug, Default)]
pub(crate) struct Track {
    segs: Vec<Seg>,
}

#[derive(Debug)]
enum Seg {
    Start,
    Key(String),
    Index(usize),
}

impl Track {
    pub fn begin(&mut self) {
        self.segs.push(Seg::Start);
    }
    pub fn end(&mut self) {
        self.segs.pop();
    }
    pub fn key(&mut self, k: &str) {
        if let Some(last) = self.segs.last_mut() {
            *last = Seg::Key(k.to_string());
        }
    }
    pub fn item(&mut self) {
        if let Some(last) = self.segs.last_mut() {
            *last = match last {
                Seg::Index(i) => Seg::Index(*i + 1),
                _ => Seg::Index(0),
            };
        }
    }
    /// `servers[2].port`, or "the top level".
    pub fn render(&self) -> String {
        let mut out = String::new();
        for seg in &self.segs {
            match seg {
                Seg::Start => {}
                Seg::Index(i) => out.push_str(&format!("[{i}]")),
                Seg::Key(k) if toml::is_ident(k) => {
                    if !out.is_empty() {
                        out.push('.');
                    }
                    out.push_str(k);
                }
                Seg::Key(k) => out.push_str(&format!("[{}]", json::quote(k))),
            }
        }
        if out.is_empty() {
            "the top level".to_string()
        } else {
            out
        }
    }
}

pub(crate) fn scalar_of(v: &Value) -> Scalar<'_> {
    match v {
        Value::Null => Scalar::Null,
        Value::Bool(b) => Scalar::Bool(*b),
        Value::Number(n) => Scalar::Number(n),
        Value::String(s) => Scalar::Str(s),
        Value::DateTime(d) => Scalar::DateTime(d),
        Value::Array(_) | Value::Object(_) => unreachable!("only leaves are scalars"),
    }
}

/// Feeds a value to a writer.
pub(crate) fn walk(v: &Value, w: &mut dyn Emit) -> Result<(), DataError> {
    match v {
        Value::Array(items) => {
            w.begin(false)?;
            for item in items {
                walk(item, w)?;
            }
            w.end()
        }
        Value::Object(entries) => {
            w.begin(true)?;
            for (k, v) in entries {
                w.key(k)?;
                walk(v, w)?;
            }
            w.end()
        }
        leaf => w.scalar(scalar_of(leaf)),
    }
}

fn writer<'o>(to: Lang, opts: &Options, out: &'o mut dyn Out) -> Box<dyn Emit + 'o> {
    let spaces = match opts.indent {
        Some(Indent::Spaces(n)) => usize::from(n),
        _ => 2,
    };
    match to {
        Lang::Json => {
            let indent = match (opts.minify, opts.indent) {
                (true, _) => None,
                (false, Some(Indent::Tab)) => Some("\t".to_string()),
                (false, _) => Some(" ".repeat(spaces)),
            };
            Box::new(json::Writer::new(out, indent))
        }
        Lang::Yaml => Box::new(yaml::Writer::new(out, spaces)),
        Lang::Toml | Lang::Csv => unreachable!("TOML and CSV are written from a whole value"),
    }
}

/// Reads `src` as `from` and writes it to `out` as `to`.
pub fn transform(
    src: &Source,
    from: Lang,
    to: Lang,
    opts: &Options,
    out: &mut dyn Out,
) -> Result<Report, DataError> {
    if let Some(why) = refusal(from, to) {
        return Err(DataError::new(DataErrorKind::Unrepresentable, why));
    }
    if let Some(why) = option_problem(to, opts) {
        return Err(DataError::new(DataErrorKind::Unrepresentable, why));
    }
    with_decoded(src, from, &opts.name, |src| {
        let mut report = Report::default();
        run(src, from, to, opts, out, &mut report)?;
        comments_note(&mut report, opts, from);
        Ok(report)
    })
}

/// Reads `src` as `from` all the way through, writing nothing: whether it
/// is valid, and what it holds that a conversion would drop.
pub fn check(src: &Source, from: Lang, opts: &Options) -> Result<Report, DataError> {
    with_decoded(src, from, &opts.name, |src| {
        let mut report = Report::default();
        match from {
            Lang::Json => {
                let mut r =
                    json::Reader::new(BufReader::with_capacity(1 << 16, src.open()?), opts.lenient);
                json::pipe(&mut r, &mut Ignore)?;
                json5_notes(&r.json5, opts, &mut report);
            }
            Lang::Csv => csv::read(src, &mut Ignore, &mut report.notes)?,
            Lang::Yaml | Lang::Toml => {
                read_docs(src, from, from, opts, &mut report)?;
            }
        }
        Ok(report)
    })
}

/// Runs `f` on `src`, or on its text as UTF-8 when it is UTF-16 (which
/// Windows tools still write), with the line under any error filled in.
fn with_decoded(
    src: &Source,
    from: Lang,
    name: &str,
    f: impl FnOnce(&Source) -> Result<Report, DataError>,
) -> Result<Report, DataError> {
    if from != Lang::Csv && src.starts_utf16()? {
        let mut raw = Vec::new();
        src.open()?
            .take(MEMORY_LIMIT as u64)
            .read_to_end(&mut raw)
            .map_err(|e| DataError::io(&e, "read the text"))?;
        let big = raw.starts_with(&[0xFE, 0xFF]);
        let units: Vec<u16> = raw[2..]
            .chunks_exact(2)
            .map(|c| {
                if big {
                    u16::from_be_bytes([c[0], c[1]])
                } else {
                    u16::from_le_bytes([c[0], c[1]])
                }
            })
            .collect();
        let text = String::from_utf16_lossy(&units).into_bytes();
        let src = Source::Text(&text);
        let mut report = f(&src).map_err(|e| e.with_snippet_from(&text))?;
        report.notes.push(format!(
            "{} is UTF-16 text; the result is UTF-8.",
            uppercase_first(name)
        ));
        return Ok(report);
    }
    f(src).map_err(|e| with_snippet(e, src))
}

fn uppercase_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// The line under an error, read back from the source.
fn with_snippet(e: DataError, src: &Source) -> DataError {
    let (Some(offset), Some(line)) = (e.offset, e.line) else {
        return e;
    };
    if e.snippet.is_some() {
        return e;
    }
    match *src {
        Source::Text(t) => e.with_snippet_from(t),
        Source::File(p) => {
            let start = offset.saturating_sub(4096);
            let mut window = Vec::new();
            let read = std::fs::File::open(p).and_then(|mut f| {
                f.seek(SeekFrom::Start(start as u64))?;
                f.take(8192).read_to_end(&mut window)
            });
            if read.is_err() {
                return e;
            }
            let mut e = e;
            e.snippet = Some(Box::new(error::snippet(
                &window,
                (offset - start).min(window.len()),
                line,
            )));
            e
        }
    }
}

fn run(
    src: &Source,
    from: Lang,
    to: Lang,
    opts: &Options,
    out: &mut dyn Out,
    report: &mut Report,
) -> Result<(), DataError> {
    let stream = !opts.sort_keys;
    match (from, to) {
        (Lang::Json, Lang::Json | Lang::Yaml) if stream => {
            let mut r =
                json::Reader::new(BufReader::with_capacity(1 << 16, src.open()?), opts.lenient);
            let mut w = writer(to, opts, out);
            json::pipe(&mut r, w.as_mut())?;
            json5_notes(&r.json5, opts, report);
        }
        (Lang::Json, Lang::Csv) => {
            let json5 = csv::from_json(src, opts.lenient, out, &mut report.notes)?;
            json5_notes(&json5, opts, report);
        }
        (Lang::Csv, Lang::Json | Lang::Yaml) if stream => {
            let mut w = writer(to, opts, out);
            csv::read(src, w.as_mut(), &mut report.notes)?;
        }
        _ => {
            let docs = read_docs(src, from, to, opts, report)?;
            write_docs(docs, to, opts, out, report)?;
        }
    }
    Ok(())
}

/// Every document in `src`, read whole.
fn read_docs(
    src: &Source,
    from: Lang,
    to: Lang,
    opts: &Options,
    report: &mut Report,
) -> Result<Vec<Value>, DataError> {
    let mut budget = Budget::new();
    match from {
        Lang::Json => {
            let mut r =
                json::Reader::new(BufReader::with_capacity(1 << 16, src.open()?), opts.lenient);
            let v = json::read_tree(&mut r, &mut budget)?;
            // What follows the value is checked too.
            r.next()?;
            json5_notes(&r.json5, opts, report);
            Ok(vec![v])
        }
        Lang::Csv => {
            let mut tree = Tree {
                stack: Vec::new(),
                key: None,
                root: None,
                budget: &mut budget,
            };
            csv::read(src, &mut tree, &mut report.notes)?;
            Ok(vec![tree.root.unwrap_or(Value::Array(Vec::new()))])
        }
        Lang::Toml => {
            let text = src.text()?;
            let (v, comments) =
                toml::read(&text, &mut budget).map_err(|e| e.with_snippet_from(text.as_bytes()))?;
            report.comments = comments;
            if matches!(to, Lang::Json | Lang::Csv) && has_datetime(&v) {
                report.notes.push(format!(
                    "TOML dates and times are written as text; {} has no date type.",
                    to.name()
                ));
            }
            Ok(vec![v])
        }
        Lang::Yaml => {
            let text = src.text()?;
            let (mut docs, found) = yaml::read(&text, opts.lenient, &mut budget)
                .map_err(|e| e.with_snippet_from(text.as_bytes()))?;
            if docs.is_empty() {
                docs.push(Value::Null);
            }
            report.comments = found.comments;
            yaml_notes(&found, to, report);
            Ok(docs)
        }
    }
}

fn has_datetime(v: &Value) -> bool {
    match v {
        Value::DateTime(_) => true,
        Value::Array(items) => items.iter().any(has_datetime),
        Value::Object(entries) => entries.iter().any(|(_, v)| has_datetime(v)),
        _ => false,
    }
}

fn write_docs(
    mut docs: Vec<Value>,
    to: Lang,
    opts: &Options,
    out: &mut dyn Out,
    report: &mut Report,
) -> Result<(), DataError> {
    if opts.sort_keys {
        docs.iter_mut().for_each(Value::sort_keys);
    }
    let several = docs.len();
    let one = |docs: Vec<Value>, report: &mut Report| -> Value {
        if docs.len() == 1 {
            return docs.into_iter().next().expect("one document");
        }
        report.notes.push(format!(
            "The {several} YAML documents in {} became a list of {several}.",
            opts.name
        ));
        Value::Array(docs)
    };
    match to {
        Lang::Yaml => {
            let mut w = writer(to, opts, out);
            for doc in &docs {
                w.document()?;
                walk(doc, w.as_mut())?;
            }
        }
        Lang::Json => {
            let v = one(docs, report);
            let mut w = writer(to, opts, out);
            walk(&v, w.as_mut())?;
        }
        Lang::Toml => {
            if several > 1 {
                return Err(DataError::new(
                    DataErrorKind::Unrepresentable,
                    format!(
                        "a TOML file holds one document, and {} has {several}",
                        opts.name
                    ),
                )
                .hint("convert it to JSON or YAML, or split it into one file a document"));
            }
            toml::write(&docs[0], out)?;
        }
        Lang::Csv => {
            let v = one(docs, report);
            csv::write_tree(&v, out, &mut report.notes)?;
        }
    }
    Ok(())
}

fn json5_notes(found: &json::Json5, opts: &Options, report: &mut Report) {
    report.comments = found.comments;
    let features: Vec<&str> = found
        .features()
        .into_iter()
        .filter(|f| *f != "comments")
        .collect();
    if features.is_empty() {
        return;
    }
    let at = found
        .first
        .map_or(String::new(), |p| format!(" (the first at {})", p.at()));
    report.notes.push(format!(
        "{} is JSON5, not JSON: it has {}{at}. conv read it anyway.",
        opts.name,
        list(&features)
    ));
}

fn yaml_notes(found: &yaml::Found, to: Lang, report: &mut Report) {
    if found.merges {
        report
            .notes
            .push("Merge keys (<<) were expanded into the mappings that use them.".to_string());
    }
    if found.aliases && to == Lang::Yaml {
        report.notes.push(
            "Aliases (*name) are written out in full; the result has no anchors.".to_string(),
        );
    }
    if !found.dropped_tags.is_empty() {
        let tags: Vec<&str> = found.dropped_tags.iter().map(String::as_str).collect();
        report.notes.push(format!(
            "The {} {} dropped and the values kept; {} has no tags.",
            if tags.len() == 1 { "tag" } else { "tags" },
            if tags.len() == 1 {
                format!("{} was", tags[0])
            } else {
                format!("{} were", list(&tags))
            },
            to.name()
        ));
    }
    if found.binary {
        report
            .notes
            .push("!!binary values are kept as their base64 text.".to_string());
    }
    if found.yaml11_bools {
        report.notes.push(if to == Lang::Yaml {
            "yes, no, on and off are written in quotes, as text: YAML 1.2 reads them as text, but YAML 1.1 readers such as PyYAML and Ansible took them as true and false.".to_string()
        } else {
            "yes, no, on and off are read as text, as YAML 1.2 reads them; YAML 1.1 readers such as PyYAML and Ansible take them as true and false.".to_string()
        });
    }
}

fn comments_note(report: &mut Report, opts: &Options, from: Lang) {
    let n = report.comments;
    if n == 0 {
        return;
    }
    let none = if from == Lang::Json {
        "; JSON has none"
    } else {
        ""
    };
    report.notes.push(if n == 1 {
        format!("The comment in {} is not kept{none}.", opts.name)
    } else {
        format!("The {n} comments in {} are not kept{none}.", opts.name)
    });
}

/// `a`, `a and b`, `a, b and c`.
fn list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Converts a file for `exec`: `input` read as `from`, `output` written as
/// `to`, with the defaults (two-space JSON and YAML, keys in their order).
/// Reads leniently: JSON5 and JSONC, YAML tags dropped, each noted.
/// Returns the notes.
pub fn convert_file(
    input: &Path,
    from: Format,
    output: &Path,
    to: Format,
) -> crate::Result<Vec<String>> {
    let (Some(f), Some(t)) = (Lang::of(from), Lang::of(to)) else {
        return Err(crate::ConvError::unsupported_pair(from, to));
    };
    let name = input.file_name().map_or_else(
        || input.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let opts = Options {
        lenient: true,
        ..Options::new(name.clone())
    };
    let file = std::fs::File::create(output).map_err(|e| {
        crate::ConvError::new(
            crate::ErrorCode::ConversionFailed,
            format!("cannot write {}: {e}", output.display()),
        )
    })?;
    let mut out = Plain(std::io::BufWriter::with_capacity(1 << 16, file));
    let report = transform(&Source::File(input), f, t, &opts, &mut out)
        .map_err(|e| e.into_conv_error(&name))?;
    out.0.flush().map_err(|e| {
        crate::ConvError::new(
            crate::ErrorCode::ConversionFailed,
            format!("cannot write {}: {e}", output.display()),
        )
    })?;
    Ok(report.notes)
}

/// Which of JSON, YAML and TOML a text with no name most likely is: JSON
/// when it opens with `{`, `[` (but not a TOML `[table]`) or a `//`
/// comment, TOML when its first line is a `[table]` or `key = value`, and
/// YAML otherwise.
pub fn sniff(head: &[u8]) -> Lang {
    let text = String::from_utf8_lossy(head);
    let text = text.trim_start_matches('\u{FEFF}').trim_start();
    if text.starts_with('{') || text.starts_with("//") || text.starts_with("/*") {
        return Lang::Json;
    }
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'));
    match first {
        Some(l) if l.starts_with('[') => {
            if toml_header(l) {
                Lang::Toml
            } else {
                Lang::Json
            }
        }
        Some(l) if toml_key_value(l) => Lang::Toml,
        Some(_) if text.starts_with('[') => Lang::Json,
        _ => Lang::Yaml,
    }
}

fn toml_header(line: &str) -> bool {
    let line = line.split(" #").next().unwrap_or(line).trim();
    let inner = line
        .strip_prefix("[[")
        .and_then(|l| l.strip_suffix("]]"))
        .or_else(|| line.strip_prefix('[').and_then(|l| l.strip_suffix(']')));
    inner.is_some_and(|i| {
        !i.trim().is_empty()
            && i.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' '))
            && i.chars().any(|c| c.is_ascii_alphabetic() || c == '_')
    })
}

fn toml_key_value(line: &str) -> bool {
    let Some((key, _)) = line.split_once('=') else {
        return false;
    };
    let key = key.trim();
    !key.is_empty()
        && (key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' '))
            || (key.len() >= 2 && key.starts_with('"') && key.ends_with('"')))
}

#[cfg(test)]
mod tests;
