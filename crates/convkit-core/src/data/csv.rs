//! CSV to and from JSON, YAML and TOML's lists of objects.
//!
//! Reading: the first row names the columns; each row after it becomes an
//! object with those keys, in that order. A column whose every cell is a
//! number becomes numbers, one whose every cell is true or false becomes
//! true and false, and any other column stays text, so `02134` and an ID
//! too long for a double keep their digits. An empty cell is null in a
//! column of numbers or of true/false, and "" in a column of text. The
//! file is read twice, once to decide the columns and once to write them,
//! so it is never held in memory.
//!
//! Writing: a list of objects, one row each. A nested value becomes a
//! column named by its path with dots, `address.city` or `tags.0`; the
//! columns are every path any row has, in the order they first appear.
//! Converting back keeps those names as they are; it does not rebuild the
//! nesting. A list of lists is written as rows as they are, with no
//! header added.

use std::collections::{HashMap, HashSet};
use std::io::Read;

use super::error::{DataError, DataErrorKind};
use super::out::{Out, Role};
use super::value::{Number, Scalar, Value};
use super::{Budget, Emit, Source};
use crate::table::{self, Encoding};

/// How a CSV is read: its delimiter, its encoding and its decimal
/// separator, from its first 64 KB.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Shape {
    delimiter: u8,
    encoding: Encoding,
    decimal_comma: bool,
}

const SNIFF: usize = 64 * 1024;

pub(crate) fn shape(src: &Source) -> Result<Shape, DataError> {
    let mut head = Vec::with_capacity(SNIFF);
    src.open()?
        .take(SNIFF as u64)
        .read_to_end(&mut head)
        .map_err(|e| DataError::io(&e, "read the CSV"))?;
    // Whole records only, so a cut one doesn't count against a delimiter.
    if head.len() == SNIFF {
        if let Some(end) = head.iter().rposition(|&b| b == b'\n') {
            head.truncate(end + 1);
        }
    }
    let sniffed = table::sniff_csv(&head).unwrap_or_default();
    Ok(Shape {
        delimiter: sniffed.delimiter,
        encoding: sniffed.encoding,
        decimal_comma: sniffed.decimal_comma,
    })
}

/// The CSV's bytes as UTF-8, whatever it was saved as.
fn decoded<'a>(src: &'a Source, shape: Shape) -> Result<Box<dyn Read + 'a>, DataError> {
    let r = src.open()?;
    Ok(match shape.encoding {
        Encoding::Utf8 => r,
        Encoding::Windows1252 => Box::new(Transcode::new(r, |b, out| {
            out.extend_from_slice(table::windows_1252(b).encode_utf8(&mut [0; 4]).as_bytes())
        })),
        Encoding::Utf16 => Box::new(Utf16::new(r)),
    })
}

fn reader<'a>(src: &'a Source, shape: Shape) -> Result<csv::Reader<Box<dyn Read + 'a>>, DataError> {
    Ok(csv::ReaderBuilder::new()
        .delimiter(shape.delimiter)
        .has_headers(false)
        .flexible(true)
        .from_reader(decoded(src, shape)?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Number,
    Bool,
    Text,
}

struct Column {
    name: String,
    numbers: bool,
    bools: bool,
    any: bool,
    empty: bool,
    /// A cell whose digits a number would lose: `02134`, a 16-digit ID.
    digits: bool,
}

impl Column {
    fn kind(&self) -> Kind {
        match (self.any, self.numbers, self.bools) {
            (true, true, _) => Kind::Number,
            (true, false, true) => Kind::Bool,
            _ => Kind::Text,
        }
    }
}

fn csv_error(e: &csv::Error) -> DataError {
    match e.position() {
        Some(p) => DataError {
            line: Some(p.line() as usize),
            ..DataError::new(DataErrorKind::Syntax, format!("{e}"))
        },
        None => DataError::new(DataErrorKind::Syntax, format!("{e}")),
    }
}

fn field(record: &csv::ByteRecord, i: usize, line: u64) -> Result<&str, DataError> {
    let bytes = record.get(i).unwrap_or_default();
    std::str::from_utf8(bytes).map_err(|_| DataError {
        line: Some(line as usize),
        ..DataError::new(DataErrorKind::Syntax, "a cell that isn't UTF-8 text")
    })
}

/// Reads the CSV into events: a list of objects.
pub(crate) fn read(
    src: &Source,
    w: &mut dyn Emit,
    notes: &mut Vec<String>,
) -> Result<(), DataError> {
    let shape = shape(src)?;
    // Pass 1: the columns, and what each holds.
    let mut columns: Vec<Column> = Vec::new();
    let mut header = true;
    let mut record = csv::ByteRecord::new();
    let mut r = reader(src, shape)?;
    while r.read_byte_record(&mut record).map_err(|e| csv_error(&e))? {
        let line = record.position().map_or(0, |p| p.line());
        if header {
            header = false;
            columns = names(&record, line, notes)?;
            continue;
        }
        for i in 0..record.len() {
            let cell = field(&record, i, line)?;
            let Some(col) = columns.get_mut(i) else {
                if cell.is_empty() {
                    continue;
                }
                return Err(DataError {
                    line: Some(line as usize),
                    ..DataError::new(
                        DataErrorKind::Syntax,
                        format!(
                            "this row has {} cells, and the header names {} columns",
                            record.len(),
                            columns.len()
                        ),
                    )
                }
                .hint("a value past the last column has no key to go under; add a column name to the header"));
            };
            if cell.is_empty() {
                col.empty = true;
                continue;
            }
            col.any = true;
            let kept = table::keeps_digits(cell);
            col.digits |= kept;
            col.numbers &= !kept && Number::from_csv(cell, shape.decimal_comma).is_some();
            col.bools &= cell.eq_ignore_ascii_case("true") || cell.eq_ignore_ascii_case("false");
        }
    }
    let kinds: Vec<Kind> = columns.iter().map(Column::kind).collect();
    if columns.iter().any(|c| c.digits && c.kind() == Kind::Text) {
        notes.push(
            "Columns with leading zeros or long digit strings, such as zip codes and IDs, are kept as text."
                .to_string(),
        );
    }
    if columns.iter().any(|c| c.empty && c.kind() != Kind::Text) {
        notes.push(
            "Empty cells in a column of numbers or of true/false values became null.".to_string(),
        );
    }
    if shape.decimal_comma && kinds.contains(&Kind::Number) {
        notes.push(
            "In a semicolon-separated CSV, decimal commas are read as decimals: 1,5 is one and a half."
                .to_string(),
        );
    }
    if shape.encoding == Encoding::Windows1252 {
        notes.push(
            "A CSV that is not UTF-8 is read as Windows-1252, the Western European encoding Excel writes."
                .to_string(),
        );
    }

    // Pass 2: the rows.
    w.begin(false)?;
    let mut r = reader(src, shape)?;
    let mut header = true;
    while r.read_byte_record(&mut record).map_err(|e| csv_error(&e))? {
        if header {
            header = false;
            continue;
        }
        let line = record.position().map_or(0, |p| p.line());
        w.begin(true)?;
        for (i, col) in columns.iter().enumerate() {
            w.key(&col.name)?;
            let cell = if i < record.len() {
                field(&record, i, line)?
            } else {
                ""
            };
            match (kinds[i], cell) {
                (Kind::Text, c) => w.scalar(Scalar::Str(c))?,
                (_, "") => w.scalar(Scalar::Null)?,
                (Kind::Number, c) => {
                    let n = Number::from_csv(c, shape.decimal_comma)
                        .expect("pass 1 found every cell a number");
                    w.scalar(Scalar::Number(&n))?;
                }
                (Kind::Bool, c) => w.scalar(Scalar::Bool(c.eq_ignore_ascii_case("true")))?,
            }
        }
        w.end()?;
    }
    w.end()
}

/// The header's names. A blank one is named for its column, and a repeated
/// one gets a number, both noted.
fn names(
    record: &csv::ByteRecord,
    line: u64,
    notes: &mut Vec<String>,
) -> Result<Vec<Column>, DataError> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut columns = Vec::new();
    let mut blank = Vec::new();
    let mut repeated = Vec::new();
    for i in 0..record.len() {
        let raw = field(record, i, line)?;
        let mut name = if raw.is_empty() {
            blank.push(i + 1);
            format!("column{}", i + 1)
        } else {
            raw.to_string()
        };
        if seen.contains(&name) {
            let mut n = 2;
            while seen.contains(&format!("{name}_{n}")) {
                n += 1;
            }
            let renamed = format!("{name}_{n}");
            repeated.push(format!("{name:?} as {renamed:?}"));
            name = renamed;
        }
        seen.insert(name.clone());
        columns.push(Column {
            name,
            numbers: true,
            bools: true,
            any: false,
            empty: false,
            digits: false,
        });
    }
    if !blank.is_empty() {
        let list: Vec<String> = blank.iter().map(|i| format!("column{i}")).collect();
        notes.push(format!(
            "The header has no name for {}, so {} named as {}.",
            plural(blank.len(), "a column", "columns"),
            if blank.len() == 1 {
                "it is"
            } else {
                "they are"
            },
            list.join(", ")
        ));
    }
    if !repeated.is_empty() {
        notes.push(format!(
            "The header repeats a name, so the repeat is written {}.",
            repeated.join(", ")
        ));
    }
    Ok(columns)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        one.to_string()
    } else {
        format!("{n} {many}")
    }
}

/// Decodes a byte-wise single-byte encoding into UTF-8 as it reads.
struct Transcode<'a> {
    inner: Box<dyn Read + 'a>,
    map: fn(u8, &mut Vec<u8>),
    pending: Vec<u8>,
    at: usize,
}

impl<'a> Transcode<'a> {
    fn new(inner: Box<dyn Read + 'a>, map: fn(u8, &mut Vec<u8>)) -> Self {
        Transcode {
            inner,
            map,
            pending: Vec::new(),
            at: 0,
        }
    }
}

impl Read for Transcode<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.at == self.pending.len() {
            let mut raw = [0u8; 8192];
            let n = self.inner.read(&mut raw)?;
            if n == 0 {
                return Ok(0);
            }
            self.pending.clear();
            self.at = 0;
            for &b in &raw[..n] {
                (self.map)(b, &mut self.pending);
            }
        }
        let n = buf.len().min(self.pending.len() - self.at);
        buf[..n].copy_from_slice(&self.pending[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// Decodes UTF-16 with a byte-order mark into UTF-8 as it reads.
struct Utf16<'a> {
    inner: Box<dyn Read + 'a>,
    big: Option<bool>,
    pending: Vec<u8>,
    at: usize,
    carry: Vec<u8>,
}

impl<'a> Utf16<'a> {
    fn new(inner: Box<dyn Read + 'a>) -> Self {
        Utf16 {
            inner,
            big: None,
            pending: Vec::new(),
            at: 0,
            carry: Vec::new(),
        }
    }
}

impl Read for Utf16<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.at == self.pending.len() {
            let mut raw = [0u8; 8192];
            let n = self.inner.read(&mut raw)?;
            if n == 0 {
                return Ok(0);
            }
            self.carry.extend_from_slice(&raw[..n]);
            if self.big.is_none() && self.carry.len() >= 2 {
                self.big = Some(self.carry.starts_with(&[0xFE, 0xFF]));
                self.carry.drain(..2);
            }
            let Some(big) = self.big else { continue };
            let whole = self.carry.len() / 2 * 2;
            let units: Vec<u16> = self.carry[..whole]
                .chunks_exact(2)
                .map(|c| {
                    if big {
                        u16::from_be_bytes([c[0], c[1]])
                    } else {
                        u16::from_le_bytes([c[0], c[1]])
                    }
                })
                .collect();
            // A surrogate pair split across reads waits for its other half.
            let keep = usize::from(units.last().is_some_and(|u| (0xD800..0xDC00).contains(u)));
            let used = units.len() - keep;
            self.pending = String::from_utf16_lossy(&units[..used]).into_bytes();
            self.at = 0;
            self.carry.drain(..used * 2);
        }
        let n = buf.len().min(self.pending.len() - self.at);
        buf[..n].copy_from_slice(&self.pending[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// What writing a CSV found, for the notes.
#[derive(Debug, Default)]
pub(crate) struct Written {
    nested: bool,
    empty_containers: bool,
    formulas: bool,
}

impl Written {
    pub fn notes(&self, notes: &mut Vec<String>) {
        if self.nested {
            notes.push(
                "Nested values became columns named by their path, like address.city or tags.0."
                    .to_string(),
            );
        }
        if self.empty_containers {
            notes.push("Empty lists and objects became empty cells.".to_string());
        }
        if self.formulas {
            notes.push(
                "Some cells start with =, + or @, which a spreadsheet may run as a formula when it opens the file."
                    .to_string(),
            );
        }
    }
}

/// The shape of the rows: objects with a header, or lists as they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rows {
    Objects,
    Lists,
}

/// The columns of a row, by path, and its cells.
pub(crate) fn flatten(
    row: &Value,
    index: usize,
    rows: Rows,
    found: &mut Written,
) -> Result<Vec<(String, String)>, DataError> {
    let mut cells = Vec::new();
    match (rows, row) {
        (Rows::Objects, Value::Object(entries)) => {
            for (k, v) in entries {
                leaf(k.clone(), v, &mut cells, found);
            }
        }
        (Rows::Lists, Value::Array(items)) => {
            for (i, v) in items.iter().enumerate() {
                if matches!(v, Value::Array(_) | Value::Object(_)) {
                    return Err(DataError::new(
                        DataErrorKind::Unrepresentable,
                        format!(
                            "row {} of the list, item {}, is {}, and a cell holds one value",
                            index + 1,
                            i + 1,
                            v.describe()
                        ),
                    ));
                }
                leaf(i.to_string(), v, &mut cells, found);
            }
        }
        (_, other) => {
            let want = if rows == Rows::Objects {
                "objects"
            } else {
                "lists"
            };
            // A list of rows that changes shape says so; one of plain
            // values says what a row has to be.
            let mixed = index > 0 && matches!(other, Value::Array(_) | Value::Object(_));
            let message = if mixed {
                format!(
                    "item {} of the list is {}, and the rows before it are {want}",
                    index + 1,
                    other.describe()
                )
            } else {
                format!(
                    "item {} of the list is {}, and a CSV row is an object or a list",
                    index + 1,
                    other.describe()
                )
            };
            return Err(DataError::new(DataErrorKind::Unrepresentable, message)
                .hint("a CSV is written from a list of objects, or a list of lists"));
        }
    }
    let mut seen = HashSet::new();
    for (name, _) in &cells {
        if !seen.insert(name.as_str()) {
            return Err(DataError::new(
                DataErrorKind::Unrepresentable,
                format!(
                    "item {} of the list has two values for the column {name:?}",
                    index + 1
                ),
            )
            .hint("a key with a dot in it and a nested key spell the same column; rename one"));
        }
    }
    Ok(cells)
}

fn leaf(path: String, v: &Value, cells: &mut Vec<(String, String)>, found: &mut Written) {
    let text = match v {
        Value::Object(entries) if !entries.is_empty() => {
            found.nested = true;
            for (k, sub) in entries {
                leaf(format!("{path}.{k}"), sub, cells, found);
            }
            return;
        }
        Value::Array(items) if !items.is_empty() => {
            found.nested = true;
            for (i, sub) in items.iter().enumerate() {
                leaf(format!("{path}.{i}"), sub, cells, found);
            }
            return;
        }
        Value::Object(_) | Value::Array(_) => {
            found.empty_containers = true;
            String::new()
        }
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.text().to_string(),
        Value::String(s) => {
            found.formulas |= s.starts_with(['=', '+', '@']);
            s.clone()
        }
        Value::DateTime(d) => d.clone(),
    };
    cells.push((path, text));
}

/// What a list of rows starts as, from its first item.
pub(crate) fn rows_of(first: Option<&Value>) -> Rows {
    match first {
        Some(Value::Array(_)) => Rows::Lists,
        _ => Rows::Objects,
    }
}

/// The CSV for an in-memory list of rows.
pub(crate) fn write_tree(
    v: &Value,
    out: &mut dyn Out,
    notes: &mut Vec<String>,
) -> Result<(), DataError> {
    let Value::Array(items) = v else {
        return Err(not_a_list(v));
    };
    let rows = rows_of(items.first());
    let mut found = Written::default();
    let mut cols = Columns::default();
    if rows == Rows::Objects {
        for (i, item) in items.iter().enumerate() {
            cols.add(&flatten(item, i, rows, &mut found)?);
        }
        cols.header(out)?;
    }
    for (i, item) in items.iter().enumerate() {
        let cells = flatten(item, i, rows, &mut found)?;
        cols.row(out, rows, cells)?;
    }
    found.notes(notes);
    if items.is_empty() {
        notes.push("The list is empty, so the CSV is too.".to_string());
    }
    Ok(())
}

/// The refusal for a value that isn't a list of rows.
pub(crate) fn not_a_list(v: &Value) -> DataError {
    DataError::new(
        DataErrorKind::Unrepresentable,
        format!("a CSV is a list of rows, and this is {}", v.describe()),
    )
    .hint("conv writes a CSV from a list of objects ([{...}, {...}]), one row each")
}

/// The columns found so far, in the order they first appeared.
#[derive(Default)]
pub(crate) struct Columns {
    names: Vec<String>,
    index: HashMap<String, usize>,
}

impl Columns {
    pub fn add(&mut self, cells: &[(String, String)]) {
        for (name, _) in cells {
            if !self.index.contains_key(name) {
                self.index.insert(name.clone(), self.names.len());
                self.names.push(name.clone());
            }
        }
    }

    pub fn header(&self, out: &mut dyn Out) -> Result<(), DataError> {
        if self.names.is_empty() {
            return Ok(());
        }
        let line = record(self.names.iter().map(String::as_str));
        put(out, Role::Key, &line)
    }

    pub fn row(
        &self,
        out: &mut dyn Out,
        rows: Rows,
        cells: Vec<(String, String)>,
    ) -> Result<(), DataError> {
        let line = match rows {
            Rows::Lists => record(cells.iter().map(|(_, c)| c.as_str())),
            Rows::Objects => {
                let mut row = vec![""; self.names.len()];
                for (name, cell) in &cells {
                    row[self.index[name]] = cell;
                }
                record(row.into_iter())
            }
        };
        put(out, Role::Str, &line)
    }
}

fn put(out: &mut dyn Out, role: Role, s: &str) -> Result<(), DataError> {
    out.put(role, s)
        .map_err(|e| DataError::io(&e, "write the result"))
}

/// One CSV line: a field with a comma, a quote or a line break in quotes,
/// its quotes doubled; a lone empty field as `""` so the line isn't blank.
fn record<'a>(fields: impl Iterator<Item = &'a str>) -> String {
    let fields: Vec<&str> = fields.collect();
    if let [only] = fields.as_slice() {
        if only.is_empty() {
            return "\"\"\n".to_string();
        }
    }
    let mut line = String::new();
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            line.push(',');
        }
        if f.contains([',', '"', '\n', '\r']) {
            line.push('"');
            line.push_str(&f.replace('"', "\"\""));
            line.push('"');
        } else {
            line.push_str(f);
        }
    }
    line.push('\n');
    line
}

/// Reads a JSON list one item at a time and writes it as a CSV, in two
/// passes: one for the columns, one for the rows.
pub(crate) fn from_json(
    src: &Source,
    lenient: bool,
    out: &mut dyn Out,
    notes: &mut Vec<String>,
) -> Result<super::json::Json5, DataError> {
    use super::json::{build, Event, Reader};
    let mut found = Written::default();
    let mut cols = Columns::default();
    let mut rows = Rows::Objects;
    let mut json5 = Default::default();
    for pass in 0..2 {
        let mut r = Reader::new(std::io::BufReader::new(src.open()?), lenient);
        match r.next()? {
            Event::BeginArray => {}
            Event::BeginObject => {
                return Err(not_a_list(&Value::Object(Vec::new())));
            }
            Event::Scalar(v) => return Err(not_a_list(&v)),
            e => unreachable!("a value never begins with {e:?}"),
        }
        let mut i = 0;
        loop {
            let e = r.next()?;
            if e == Event::EndArray {
                break;
            }
            let mut budget = Budget::new();
            let item = build(&mut r, e, &mut budget)?;
            if i == 0 {
                rows = rows_of(Some(&item));
            }
            let cells = flatten(&item, i, rows, &mut found)?;
            if pass == 0 {
                cols.add(&cells);
            } else {
                cols.row(out, rows, cells)?;
            }
            i += 1;
        }
        // Read to the end, so text after the list is still an error.
        r.next()?;
        if pass == 0 {
            if rows == Rows::Objects {
                cols.header(out)?;
            }
            if i == 0 {
                notes.push("The list is empty, so the CSV is too.".to_string());
            }
            json5 = r.json5.clone();
        }
    }
    found.notes(notes);
    Ok(json5)
}
