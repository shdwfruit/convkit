//! What a PDF says about itself: its version, page count, page sizes,
//! whether it is encrypted, and who made it. A plain file read, no backend.
//!
//! Only the objects that answer those questions are read: the
//! cross-reference table (or stream) is followed from `startxref`, through
//! every `/Prev`, and the catalog, page tree and info dictionary are looked
//! up in it, so a 500 MB scan costs a few small reads. Objects packed into
//! object streams (PDF 1.5 and later) are inflated as needed. A file whose
//! cross-reference is damaged is read whole and scanned for its objects
//! instead, the way viewers recover one.
//!
//! An encrypted file's strings and object streams are ciphertext. Nothing
//! is decrypted: a page tree kept in plain objects is still counted, and
//! the producer is left out rather than shown as noise.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::Serialize;

/// What `read` found.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PdfInfo {
    /// From the header, `%PDF-1.7`.
    pub version: Option<String>,
    /// The page tree's own count.
    pub pages: Option<u64>,
    /// The first page's size as shown (its `/Rotate` applied), in points.
    pub page_size: Option<PageSize>,
    /// Whether any page has a different size from the first.
    pub mixed_sizes: bool,
    pub encrypted: bool,
    pub producer: Option<String>,
    pub creator: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PageSize {
    pub width_pt: f64,
    pub height_pt: f64,
}

impl PageSize {
    /// The paper this is, when it is one of the common ones, within 2 pt
    /// either way up.
    pub fn paper(&self) -> Option<&'static str> {
        const PAPERS: &[(&str, f64, f64)] = &[
            ("A3", 841.89, 1190.55),
            ("A4", 595.28, 841.89),
            ("A5", 419.53, 595.28),
            ("Letter", 612.0, 792.0),
            ("Legal", 612.0, 1008.0),
            ("Tabloid", 792.0, 1224.0),
        ];
        let (w, h) = (self.width_pt, self.height_pt);
        let near = |a: f64, b: f64| (a - b).abs() <= 2.0;
        PAPERS
            .iter()
            .find(|&&(_, pw, ph)| (near(w, pw) && near(h, ph)) || (near(w, ph) && near(h, pw)))
            .map(|&(name, ..)| name)
    }
}

/// Reads `path`. `None` when it is not a PDF at all (no `%PDF-` header in
/// its first kilobyte) or cannot be opened.
pub fn read(path: &Path) -> Option<PdfInfo> {
    if !path.is_file() {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let mut head = vec![0u8; 1024];
    let n = read_up_to(&mut file, &mut head).ok()?;
    head.truncate(n);
    let at = find(&head, b"%PDF-")?;
    let version: String = head[at + 5..]
        .iter()
        .take_while(|b| b.is_ascii_digit() || **b == b'.')
        .map(|&b| b as char)
        .collect();
    let mut pdf = Pdf::open(file)?;
    let mut info = PdfInfo {
        version: (!version.is_empty()).then_some(version),
        encrypted: pdf.trailer.get(b"Encrypt").is_some(),
        ..PdfInfo::default()
    };
    pdf.describe(&mut info);
    Some(info)
}

/// Reads until `buf` is full or the file ends.
fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Objects

#[derive(Debug, Clone, PartialEq)]
enum Obj {
    Null,
    Bool(bool),
    Num(f64),
    Str(Vec<u8>),
    Name(Vec<u8>),
    Array(Vec<Obj>),
    Dict(Dict),
    Ref(u32, u16),
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Dict(Vec<(Vec<u8>, Obj)>);

impl Dict {
    fn get(&self, key: &[u8]) -> Option<&Obj> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

impl Obj {
    fn as_int(&self) -> Option<i64> {
        match self {
            Obj::Num(n) if n.fract() == 0.0 => Some(*n as i64),
            _ => None,
        }
    }
}

/// A dictionary followed by `stream`: where its data starts, in the bytes
/// the object was parsed from.
struct Parsed {
    obj: Obj,
    stream_at: Option<usize>,
}

/// A tokenizer and parser over one buffer.
struct Lexer<'a> {
    b: &'a [u8],
    pos: usize,
}

fn is_white(c: u8) -> bool {
    matches!(c, b'\0' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

fn is_delim(c: u8) -> bool {
    matches!(
        c,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

/// Arrays and dictionaries nest no deeper than this, so a hostile file
/// cannot exhaust the stack.
const MAX_DEPTH: usize = 64;

impl<'a> Lexer<'a> {
    fn new(b: &'a [u8], pos: usize) -> Self {
        Lexer { b, pos }
    }

    fn skip_white(&mut self) {
        while let Some(&c) = self.b.get(self.pos) {
            if is_white(c) {
                self.pos += 1;
            } else if c == b'%' {
                while let Some(&c) = self.b.get(self.pos) {
                    if c == b'\n' || c == b'\r' {
                        break;
                    }
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    /// A bare word: a keyword or a number.
    fn word(&mut self) -> &'a [u8] {
        self.skip_white();
        let start = self.pos;
        while let Some(&c) = self.b.get(self.pos) {
            if is_white(c) || is_delim(c) {
                break;
            }
            self.pos += 1;
        }
        &self.b[start..self.pos]
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_white();
        self.b.get(self.pos).copied()
    }

    fn keyword(&mut self, kw: &[u8]) -> bool {
        let save = self.pos;
        if self.word() == kw {
            true
        } else {
            self.pos = save;
            false
        }
    }

    fn uint(&mut self) -> Option<u64> {
        let save = self.pos;
        let w = self.word();
        match std::str::from_utf8(w).ok().and_then(|s| s.parse().ok()) {
            Some(n) if !w.is_empty() && w.iter().all(u8::is_ascii_digit) => Some(n),
            _ => {
                self.pos = save;
                None
            }
        }
    }

    fn object(&mut self, depth: usize) -> Option<Obj> {
        if depth > MAX_DEPTH {
            return None;
        }
        match self.peek()? {
            b'/' => {
                self.pos += 1;
                let start = self.pos;
                while let Some(&c) = self.b.get(self.pos) {
                    if is_white(c) || is_delim(c) {
                        break;
                    }
                    self.pos += 1;
                }
                Some(Obj::Name(unescape_name(&self.b[start..self.pos])))
            }
            b'(' => self.literal().map(Obj::Str),
            b'<' if self.b.get(self.pos + 1) == Some(&b'<') => {
                self.pos += 2;
                let mut d = Vec::new();
                loop {
                    match self.peek()? {
                        b'>' if self.b.get(self.pos + 1) == Some(&b'>') => {
                            self.pos += 2;
                            return Some(Obj::Dict(Dict(d)));
                        }
                        b'/' => {
                            let Obj::Name(k) = self.object(depth + 1)? else {
                                return None;
                            };
                            let v = self.object(depth + 1)?;
                            d.push((k, v));
                        }
                        _ => return None,
                    }
                }
            }
            b'<' => {
                self.pos += 1;
                let mut digits = Vec::new();
                loop {
                    let c = *self.b.get(self.pos)?;
                    self.pos += 1;
                    match c {
                        b'>' => break,
                        c if c.is_ascii_hexdigit() => digits.push(c),
                        c if is_white(c) => {}
                        _ => return None,
                    }
                }
                if digits.len() % 2 == 1 {
                    digits.push(b'0');
                }
                let bytes = digits
                    .chunks(2)
                    .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
                    .collect();
                Some(Obj::Str(bytes))
            }
            b'[' => {
                self.pos += 1;
                let mut a = Vec::new();
                loop {
                    if self.peek()? == b']' {
                        self.pos += 1;
                        return Some(Obj::Array(a));
                    }
                    a.push(self.object(depth + 1)?);
                }
            }
            _ => {
                let save = self.pos;
                let w = self.word();
                match w {
                    b"true" => Some(Obj::Bool(true)),
                    b"false" => Some(Obj::Bool(false)),
                    b"null" => Some(Obj::Null),
                    _ => {
                        let n: f64 = std::str::from_utf8(w).ok()?.parse().ok()?;
                        // `N G R` is a reference; anything else, a number.
                        if w.iter().all(u8::is_ascii_digit) {
                            let after = self.pos;
                            if let Some(g) = self.uint() {
                                if self.keyword(b"R") {
                                    return Some(Obj::Ref(
                                        u32::try_from(n as u64).ok()?,
                                        u16::try_from(g).ok()?,
                                    ));
                                }
                            }
                            self.pos = after;
                        }
                        if w.is_empty() {
                            self.pos = save;
                            return None;
                        }
                        Some(Obj::Num(n))
                    }
                }
            }
        }
    }

    /// A `( ... )` string, with its escapes undone.
    fn literal(&mut self) -> Option<Vec<u8>> {
        self.pos += 1;
        let mut out = Vec::new();
        let mut depth = 1;
        loop {
            let c = *self.b.get(self.pos)?;
            self.pos += 1;
            match c {
                b'(' => {
                    depth += 1;
                    out.push(c);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(out);
                    }
                    out.push(c);
                }
                b'\\' => {
                    let e = *self.b.get(self.pos)?;
                    self.pos += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'\r' => {
                            if self.b.get(self.pos) == Some(&b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'\n' => {}
                        b'0'..=b'7' => {
                            let mut v = u32::from(e - b'0');
                            for _ in 0..2 {
                                match self.b.get(self.pos) {
                                    Some(&d @ b'0'..=b'7') => {
                                        v = v * 8 + u32::from(d - b'0');
                                        self.pos += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push(v as u8);
                        }
                        other => out.push(other),
                    }
                }
                _ => out.push(c),
            }
        }
    }

    /// `N G obj <object>`, and where a stream's data starts if one follows.
    fn indirect(&mut self) -> Option<(u32, Parsed)> {
        let num = u32::try_from(self.uint()?).ok()?;
        self.uint()?;
        if !self.keyword(b"obj") {
            return None;
        }
        let obj = self.object(0)?;
        let mut stream_at = None;
        if matches!(obj, Obj::Dict(_)) && self.keyword(b"stream") {
            // The keyword ends with CRLF or LF; a lone CR is common enough
            // in the wild to accept too.
            match self.b.get(self.pos) {
                Some(b'\r') if self.b.get(self.pos + 1) == Some(&b'\n') => self.pos += 2,
                Some(b'\r' | b'\n') => self.pos += 1,
                _ => {}
            }
            stream_at = Some(self.pos);
        }
        Some((num, Parsed { obj, stream_at }))
    }
}

/// `#xx` escapes in a name.
fn unescape_name(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'#' {
            if let Some(v) = raw
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(raw[i]);
        i += 1;
    }
    out
}

/// A PDF text string: UTF-16BE or UTF-8 with a byte-order mark, otherwise
/// PDFDocEncoding, which is Latin-1 for every character a producer name
/// or title uses in practice.
fn text(bytes: &[u8]) -> String {
    let s = if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| u16::from_be_bytes([p[0], p[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8_lossy(rest).into_owned()
    } else {
        bytes.iter().map(|&b| b as char).collect()
    };
    s.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// Streams

/// Inflated streams stop here, so a compression bomb costs no more.
const MAX_INFLATED: u64 = 64 << 20;

/// Undoes a stream's filters, when they are `FlateDecode` (with or without
/// a PNG predictor) or none. Anything else is `None`.
fn decode(dict: &Dict, raw: &[u8]) -> Option<Vec<u8>> {
    let filters: Vec<&[u8]> = match dict.get(b"Filter") {
        None => vec![],
        Some(Obj::Name(n)) => vec![n.as_slice()],
        Some(Obj::Array(a)) => a
            .iter()
            .map(|o| match o {
                Obj::Name(n) => Some(n.as_slice()),
                _ => None,
            })
            .collect::<Option<_>>()?,
        _ => return None,
    };
    let mut data = raw.to_vec();
    for f in filters {
        if f != b"FlateDecode" && f != b"Fl" {
            return None;
        }
        let mut out = Vec::new();
        flate2::read::ZlibDecoder::new(data.as_slice())
            .take(MAX_INFLATED)
            .read_to_end(&mut out)
            .ok()?;
        data = out;
    }
    let parms = match dict.get(b"DecodeParms") {
        Some(Obj::Dict(d)) => Some(d),
        Some(Obj::Array(a)) => a.iter().find_map(|o| match o {
            Obj::Dict(d) => Some(d),
            _ => None,
        }),
        _ => None,
    };
    let predictor = parms
        .and_then(|p| p.get(b"Predictor"))
        .and_then(Obj::as_int)
        .unwrap_or(1);
    if predictor >= 10 {
        let columns = parms
            .and_then(|p| p.get(b"Columns"))
            .and_then(Obj::as_int)
            .unwrap_or(1);
        data = unpredict_png(&data, usize::try_from(columns).ok()?)?;
    } else if predictor != 1 {
        return None;
    }
    Some(data)
}

/// Undoes PNG row predictors over rows of `columns` bytes, each led by its
/// filter type byte, as cross-reference streams use them.
fn unpredict_png(data: &[u8], columns: usize) -> Option<Vec<u8>> {
    if columns == 0 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len());
    let mut prev = vec![0u8; columns];
    for row in data.chunks(columns + 1) {
        let (&kind, bytes) = row.split_first()?;
        let mut cur = bytes.to_vec();
        cur.resize(columns, 0);
        for i in 0..columns {
            let left = if i > 0 { cur[i - 1] } else { 0 };
            let up = prev[i];
            let up_left = if i > 0 { prev[i - 1] } else { 0 };
            let add = match kind {
                0 => 0,
                1 => left,
                2 => up,
                3 => ((u16::from(left) + u16::from(up)) / 2) as u8,
                4 => {
                    let p = i16::from(left) + i16::from(up) - i16::from(up_left);
                    let (pa, pb, pc) = (
                        (p - i16::from(left)).abs(),
                        (p - i16::from(up)).abs(),
                        (p - i16::from(up_left)).abs(),
                    );
                    if pa <= pb && pa <= pc {
                        left
                    } else if pb <= pc {
                        up
                    } else {
                        up_left
                    }
                }
                _ => return None,
            };
            cur[i] = cur[i].wrapping_add(add);
        }
        out.extend_from_slice(&cur);
        prev = cur;
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// The file

#[derive(Debug, Clone, Copy)]
enum Entry {
    At(u64),
    InStream(u32, usize),
}

/// Where the objects come from: the file, read a piece at a time, or the
/// whole file in memory once the cross-reference turned out damaged.
enum Source {
    File(File),
    Memory(Vec<u8>),
}

struct Pdf {
    src: Source,
    xref: HashMap<u32, Entry>,
    trailer: Dict,
    streams: HashMap<u32, Vec<(u32, usize)>>,
    stream_data: HashMap<u32, Vec<u8>>,
}

/// The most objects a stream may claim to hold.
const MAX_OBJSTM_OBJECTS: usize = 1 << 20;

impl Pdf {
    fn open(file: File) -> Option<Pdf> {
        let mut pdf = Pdf {
            src: Source::File(file),
            xref: HashMap::new(),
            trailer: Dict::default(),
            streams: HashMap::new(),
            stream_data: HashMap::new(),
        };
        let from_xref = pdf.start_xref().is_some_and(|at| pdf.load_xref(at));
        // An encrypted file's catalog may sit in an object stream that
        // cannot be read without the key; reading the whole file again
        // would not find it either.
        let encrypted = pdf.trailer.get(b"Encrypt").is_some();
        if !from_xref || (pdf.catalog().is_none() && !encrypted) {
            pdf.recover()?;
        }
        Some(pdf)
    }

    fn len(&mut self) -> u64 {
        match &mut self.src {
            Source::File(f) => f.metadata().map(|m| m.len()).unwrap_or(0),
            Source::Memory(m) => m.len() as u64,
        }
    }

    /// Up to `n` bytes from `at`.
    fn bytes_at(&mut self, at: u64, n: usize) -> Option<Vec<u8>> {
        match &mut self.src {
            Source::File(f) => {
                f.seek(SeekFrom::Start(at)).ok()?;
                let mut buf = vec![0u8; n];
                let got = read_up_to(f, &mut buf).ok()?;
                buf.truncate(got);
                Some(buf)
            }
            Source::Memory(m) => {
                let at = usize::try_from(at).ok()?;
                Some(m.get(at..)?.iter().take(n).copied().collect())
            }
        }
    }

    /// The offset `startxref` names, from the file's last kilobytes.
    fn start_xref(&mut self) -> Option<u64> {
        let len = self.len();
        let tail_at = len.saturating_sub(2048);
        let tail = self.bytes_at(tail_at, 2048)?;
        let at = rfind(&tail, b"startxref")?;
        let mut lx = Lexer::new(&tail, at + b"startxref".len());
        lx.uint()
    }

    /// Parses an object at `at`, reading more of the file if it runs past
    /// what was read.
    fn parse_at(&mut self, at: u64) -> Option<(Vec<u8>, Parsed)> {
        let mut n = 16 << 10;
        loop {
            let buf = self.bytes_at(at, n)?;
            let full = buf.len() == n;
            let parsed = Lexer::new(&buf, 0).indirect().map(|(_, p)| p);
            match parsed {
                Some(p) => return Some((buf, p)),
                None if full && n < (8 << 20) => n *= 4,
                None => return None,
            }
        }
    }

    /// A stream's raw bytes, `/Length` long, or up to `endstream` when its
    /// length is missing or wrong.
    fn stream_bytes(
        &mut self,
        buf: &[u8],
        dict: &Dict,
        start: usize,
        base: u64,
    ) -> Option<Vec<u8>> {
        let len = match dict.get(b"Length") {
            Some(Obj::Ref(n, _)) => {
                let n = *n;
                self.resolve(&Obj::Ref(n, 0), 0).and_then(|o| o.as_int())
            }
            Some(o) => o.as_int(),
            None => None,
        };
        let data = match len.and_then(|l| usize::try_from(l).ok()) {
            Some(l) if start + l <= buf.len() => buf[start..start + l].to_vec(),
            Some(l) if l as u64 <= MAX_INFLATED => self.bytes_at(base + start as u64, l)?,
            _ => {
                let end = find(&buf[start..], b"endstream")?;
                buf[start..start + end].to_vec()
            }
        };
        Some(data)
    }

    /// Reads the cross-reference sections from `at` back through `/Prev`.
    /// The newest section's entry for an object wins.
    fn load_xref(&mut self, at: u64) -> bool {
        let mut next = Some(at);
        let mut seen = HashSet::new();
        let mut any = false;
        while let Some(at) = next.take() {
            if !seen.insert(at) || seen.len() > 512 {
                break;
            }
            let Some(trailer) = self.xref_section(at) else {
                return any;
            };
            any = true;
            if let Some(stm) = trailer.get(b"XRefStm").and_then(Obj::as_int) {
                if let Ok(stm) = u64::try_from(stm) {
                    self.xref_section(stm);
                }
            }
            next = trailer
                .get(b"Prev")
                .and_then(Obj::as_int)
                .and_then(|p| u64::try_from(p).ok());
            for (k, v) in trailer.0 {
                if self.trailer.get(&k).is_none() {
                    self.trailer.0.push((k, v));
                }
            }
        }
        any
    }

    /// One section, a classic table or a stream, returning its trailer.
    fn xref_section(&mut self, at: u64) -> Option<Dict> {
        let head = self.bytes_at(at, 4)?;
        if head == b"xref" {
            self.xref_table(at)
        } else {
            self.xref_stream(at)
        }
    }

    fn xref_table(&mut self, at: u64) -> Option<Dict> {
        let mut n = 64 << 10;
        let buf = loop {
            let buf = self.bytes_at(at, n)?;
            if find(&buf, b"trailer").is_some() || buf.len() < n || n >= (64 << 20) {
                break buf;
            }
            n *= 4;
        };
        let mut lx = Lexer::new(&buf, 0);
        if !lx.keyword(b"xref") {
            return None;
        }
        loop {
            if lx.keyword(b"trailer") {
                return match lx.object(0)? {
                    Obj::Dict(d) => Some(d),
                    _ => None,
                };
            }
            let first = lx.uint()?;
            let count = lx.uint()?;
            for i in 0..count {
                let offset = lx.uint()?;
                lx.uint()?;
                let kind = lx.word();
                let num = u32::try_from(first + i).ok()?;
                if kind == b"n" && offset > 0 {
                    self.xref.entry(num).or_insert(Entry::At(offset));
                }
            }
        }
    }

    fn xref_stream(&mut self, at: u64) -> Option<Dict> {
        let (buf, parsed) = self.parse_at(at)?;
        let Obj::Dict(dict) = parsed.obj else {
            return None;
        };
        if dict.get(b"Type") != Some(&Obj::Name(b"XRef".to_vec())) {
            return None;
        }
        let raw = self.stream_bytes(&buf, &dict, parsed.stream_at?, at)?;
        let data = decode(&dict, &raw)?;
        let widths: Vec<usize> = match dict.get(b"W")? {
            Obj::Array(a) => a
                .iter()
                .map(|o| o.as_int().and_then(|w| usize::try_from(w).ok()))
                .collect::<Option<_>>()?,
            _ => return None,
        };
        if widths.len() != 3 || widths.iter().any(|&w| w > 8) {
            return None;
        }
        let size = dict.get(b"Size").and_then(Obj::as_int).unwrap_or(0);
        let index: Vec<i64> = match dict.get(b"Index") {
            Some(Obj::Array(a)) => a.iter().map(Obj::as_int).collect::<Option<_>>()?,
            _ => vec![0, size],
        };
        let row = widths.iter().sum::<usize>();
        if row == 0 {
            return None;
        }
        let field = |r: &[u8], from: usize, w: usize| -> u64 {
            r[from..from + w]
                .iter()
                .fold(0u64, |v, &b| (v << 8) | u64::from(b))
        };
        let mut rows = data.chunks_exact(row);
        for pair in index.chunks(2) {
            let [first, count] = pair else { break };
            for i in 0..*count {
                let Some(r) = rows.next() else { break };
                // A missing type field means type 1.
                let kind = if widths[0] == 0 {
                    1
                } else {
                    field(r, 0, widths[0])
                };
                let b = field(r, widths[0], widths[1]);
                let c = field(r, widths[0] + widths[1], widths[2]);
                let Ok(num) = u32::try_from(first + i) else {
                    continue;
                };
                let entry = match kind {
                    1 if b > 0 => Entry::At(b),
                    2 => match (u32::try_from(b), usize::try_from(c)) {
                        (Ok(stm), Ok(idx)) => Entry::InStream(stm, idx),
                        _ => continue,
                    },
                    _ => continue,
                };
                self.xref.entry(num).or_insert(entry);
            }
        }
        Some(dict)
    }

    /// Reads the whole file and finds every `N G obj`, the last copy of an
    /// object winning, as a viewer does with a damaged cross-reference.
    fn recover(&mut self) -> Option<()> {
        let data = match &mut self.src {
            Source::File(f) => {
                f.seek(SeekFrom::Start(0)).ok()?;
                let mut all = Vec::new();
                f.read_to_end(&mut all).ok()?;
                all
            }
            Source::Memory(m) => std::mem::take(m),
        };
        let mut xref = HashMap::new();
        let mut i = 0;
        while let Some(off) = find(&data[i..], b" obj") {
            let end = i + off;
            // Walk back over `G`, a space and `N`.
            let mut s = end;
            let digits_back = |s: &mut usize| {
                let stop = *s;
                while *s > 0 && data[*s - 1].is_ascii_digit() {
                    *s -= 1;
                }
                *s < stop
            };
            if digits_back(&mut s) && s > 0 && data[s - 1] == b' ' {
                let mut n = s - 1;
                if digits_back(&mut n) && (n == 0 || !data[n - 1].is_ascii_alphanumeric()) {
                    if let Some(num) = std::str::from_utf8(&data[n..s - 1])
                        .ok()
                        .and_then(|t| t.parse::<u32>().ok())
                    {
                        xref.insert(num, Entry::At(n as u64));
                    }
                }
            }
            i = end + 4;
        }
        // The last trailer names the catalog, or a cross-reference stream
        // does; failing both, any object typed `/Catalog`.
        let mut trailer = Dict::default();
        if let Some(t) = rfind(&data, b"trailer") {
            if let Some(Obj::Dict(d)) = Lexer::new(&data, t + 7).object(0) {
                trailer = d;
            }
        }
        self.src = Source::Memory(data);
        self.xref = xref;
        self.streams.clear();
        self.stream_data.clear();
        for (k, v) in std::mem::take(&mut self.trailer).0 {
            if trailer.get(&k).is_none() {
                trailer.0.push((k, v));
            }
        }
        self.trailer = trailer;
        if self.catalog().is_none() {
            let nums: Vec<u32> = self.xref.keys().copied().collect();
            let found = nums.into_iter().find(|&n| {
                matches!(self.object(n), Some(Obj::Dict(d))
                    if d.get(b"Type") == Some(&Obj::Name(b"Catalog".to_vec())))
            });
            if let Some(found) = found {
                self.trailer.0.retain(|(k, _)| k != b"Root");
                self.trailer.0.push((b"Root".to_vec(), Obj::Ref(found, 0)));
            }
        }
        Some(())
    }

    /// Object `num`, wherever the cross-reference says it is.
    fn object(&mut self, num: u32) -> Option<Obj> {
        match *self.xref.get(&num)? {
            Entry::At(at) => {
                let (_, parsed) = self.parse_at(at)?;
                Some(parsed.obj)
            }
            Entry::InStream(stm, idx) => {
                if !self.streams.contains_key(&stm) {
                    self.load_object_stream(stm)?;
                }
                let &(n, off) = self.streams.get(&stm)?.get(idx)?;
                if n != num {
                    return None;
                }
                let data = self.stream_data.get(&stm)?;
                Lexer::new(data, off).object(0)
            }
        }
    }

    fn load_object_stream(&mut self, stm: u32) -> Option<()> {
        let Entry::At(at) = *self.xref.get(&stm)? else {
            return None;
        };
        let (buf, parsed) = self.parse_at(at)?;
        let Obj::Dict(dict) = parsed.obj else {
            return None;
        };
        let raw = self.stream_bytes(&buf, &dict, parsed.stream_at?, at)?;
        let data = decode(&dict, &raw)?;
        let n = usize::try_from(dict.get(b"N")?.as_int()?).ok()?;
        let first = usize::try_from(dict.get(b"First")?.as_int()?).ok()?;
        if n > MAX_OBJSTM_OBJECTS {
            return None;
        }
        let mut lx = Lexer::new(&data, 0);
        let mut table = Vec::with_capacity(n);
        for _ in 0..n {
            let num = u32::try_from(lx.uint()?).ok()?;
            let off = usize::try_from(lx.uint()?).ok()?;
            table.push((num, first + off));
        }
        self.streams.insert(stm, table);
        self.stream_data.insert(stm, data);
        Some(())
    }

    /// Follows references until a direct object, `depth` deep at most.
    fn resolve(&mut self, obj: &Obj, depth: usize) -> Option<Obj> {
        match obj {
            Obj::Ref(n, _) if depth < 16 => {
                let next = self.object(*n)?;
                self.resolve(&next, depth + 1)
            }
            Obj::Ref(..) => None,
            other => Some(other.clone()),
        }
    }

    fn dict(&mut self, obj: Option<&Obj>) -> Option<Dict> {
        match self.resolve(obj?, 0)? {
            Obj::Dict(d) => Some(d),
            _ => None,
        }
    }

    fn catalog(&mut self) -> Option<Dict> {
        let root = self.trailer.get(b"Root").cloned();
        self.dict(root.as_ref())
    }

    fn describe(&mut self, info: &mut PdfInfo) {
        if !info.encrypted {
            let info_ref = self.trailer.get(b"Info").cloned();
            if let Some(d) = self.dict(info_ref.as_ref()) {
                let mut field = |key: &[u8]| match self.resolve(d.get(key)?, 0)? {
                    Obj::Str(s) => Some(text(&s)).filter(|t| !t.is_empty()),
                    _ => None,
                };
                info.producer = field(b"Producer");
                info.creator = field(b"Creator");
                info.title = field(b"Title");
            }
        }
        let Some(catalog) = self.catalog() else {
            return;
        };
        let Some(pages) = self.dict(catalog.get(b"Pages")) else {
            return;
        };
        info.pages = pages
            .get(b"Count")
            .and_then(|c| self.resolve(c, 0))
            .and_then(|c| c.as_int())
            .and_then(|c| u64::try_from(c).ok());
        let mut sizes = Vec::new();
        let mut seen = HashSet::new();
        self.walk(pages, Inherited::default(), &mut sizes, &mut seen, 0);
        info.page_size = sizes.first().copied();
        info.mixed_sizes = sizes.iter().any(|s| {
            let f = sizes[0];
            (s.width_pt - f.width_pt).abs() > 1.0 || (s.height_pt - f.height_pt).abs() > 1.0
        });
    }

    /// Collects each page's shown size, in page order, up to `MAX_PAGES`.
    fn walk(
        &mut self,
        node: Dict,
        mut inh: Inherited,
        sizes: &mut Vec<PageSize>,
        seen: &mut HashSet<u32>,
        depth: usize,
    ) {
        if depth > MAX_DEPTH || sizes.len() >= MAX_PAGES {
            return;
        }
        if let Some(b) = node.get(b"MediaBox").and_then(|b| self.rect(b)) {
            inh.media_box = Some(b);
        }
        if let Some(r) = node
            .get(b"Rotate")
            .and_then(|r| self.resolve(r, 0))
            .and_then(|r| r.as_int())
        {
            inh.rotate = r;
        }
        let kids = match node.get(b"Kids").and_then(|k| self.resolve(k, 0)) {
            Some(Obj::Array(a)) => a,
            _ => {
                // A leaf: a page.
                if let Some((w, h)) = inh.media_box {
                    let (w, h) = if inh.rotate.rem_euclid(180) == 90 {
                        (h, w)
                    } else {
                        (w, h)
                    };
                    sizes.push(PageSize {
                        width_pt: w,
                        height_pt: h,
                    });
                }
                return;
            }
        };
        for kid in kids {
            if sizes.len() >= MAX_PAGES {
                return;
            }
            if let Obj::Ref(n, _) = kid {
                if !seen.insert(n) {
                    continue;
                }
            }
            if let Some(d) = self.dict(Some(&kid)) {
                self.walk(d, inh, sizes, seen, depth + 1);
            }
        }
    }

    /// A rectangle's width and height.
    fn rect(&mut self, obj: &Obj) -> Option<(f64, f64)> {
        let Obj::Array(a) = self.resolve(obj, 0)? else {
            return None;
        };
        let mut n = Vec::new();
        for o in a {
            match self.resolve(&o, 0)? {
                Obj::Num(v) => n.push(v),
                _ => return None,
            }
        }
        let [x0, y0, x1, y1] = n[..] else {
            return None;
        };
        let (w, h) = ((x1 - x0).abs(), (y1 - y0).abs());
        (w > 0.0 && h > 0.0).then_some((w, h))
    }
}

/// Pages beyond this are counted by the tree's `/Count` but not sized.
const MAX_PAGES: usize = 5000;

#[derive(Debug, Clone, Copy, Default)]
struct Inherited {
    media_box: Option<(f64, f64)>,
    rotate: i64,
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;

    use super::*;

    /// A PDF with a classic cross-reference table, built with real offsets.
    pub(crate) fn classic(objects: &[&str], trailer: &str) -> Vec<u8> {
        let mut out = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref_at = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} {trailer} >>\nstartxref\n{xref_at}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    fn read_bytes(bytes: &[u8]) -> PdfInfo {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.pdf");
        std::fs::write(&path, bytes).unwrap();
        read(&path).expect("reads as a PDF")
    }

    fn three_pages() -> Vec<u8> {
        classic(
            &[
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 /MediaBox [0 0 595.28 841.89] >>",
                "<< /Type /Page /Parent 2 0 R >>",
                "<< /Type /Page /Parent 2 0 R >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
                "<< /Producer (Pages \\(test\\)) /Title <FEFF00480069> >>",
            ],
            "/Root 1 0 R /Info 6 0 R",
        )
    }

    #[test]
    fn a_classic_pdf_reads_its_pages_sizes_and_producer() {
        let info = read_bytes(&three_pages());
        assert_eq!(info.version.as_deref(), Some("1.4"));
        assert_eq!(info.pages, Some(3));
        let size = info.page_size.unwrap();
        assert_eq!(size.paper(), Some("A4"));
        assert!(info.mixed_sizes, "the third page is Letter");
        assert!(!info.encrypted);
        assert_eq!(info.producer.as_deref(), Some("Pages (test)"));
        assert_eq!(info.title.as_deref(), Some("Hi"));
    }

    /// A turned page reports the size it is shown at.
    #[test]
    fn a_rotated_page_is_sized_as_shown() {
        let info = read_bytes(&classic(
            &[
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Rotate 90 >>",
            ],
            "/Root 1 0 R",
        ));
        let size = info.page_size.unwrap();
        assert_eq!((size.width_pt, size.height_pt), (792.0, 612.0));
        assert_eq!(size.paper(), Some("Letter"));
        assert!(!info.mixed_sizes);
    }

    /// An encrypted file says so, and its ciphertext strings are not shown.
    #[test]
    fn an_encrypted_pdf_says_so_and_shows_no_producer() {
        let info = read_bytes(&classic(
            &[
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 612 792] >>",
                "<< /Type /Page /Parent 2 0 R >>",
                "<< /Producer <8a7f00ee> >>",
                "<< /Filter /Standard /V 2 /R 3 /O <00> /U <00> /P -4 >>",
            ],
            "/Root 1 0 R /Info 4 0 R /Encrypt 5 0 R",
        ));
        assert!(info.encrypted);
        assert_eq!(info.pages, Some(1));
        assert_eq!(info.producer, None);
    }

    /// A cross-reference that points at the wrong place is recovered by
    /// scanning for the objects.
    #[test]
    fn a_damaged_cross_reference_is_recovered() {
        let mut bytes = three_pages();
        let at = rfind(&bytes, b"startxref\n").unwrap() + 10;
        bytes.truncate(at);
        bytes.extend_from_slice(b"12\n%%EOF\n");
        let info = read_bytes(&bytes);
        assert_eq!(info.pages, Some(3));
        assert_eq!(info.producer.as_deref(), Some("Pages (test)"));
    }

    /// PDF 1.5's packed form: the objects in a compressed object stream,
    /// found through a cross-reference stream with a PNG Up predictor.
    #[test]
    fn objects_in_an_object_stream_are_found_through_an_xref_stream() {
        let zip = |data: &[u8]| {
            let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(data).unwrap();
            e.finish().unwrap()
        };
        // Objects 1-4 packed in object 5.
        let packed = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
        ];
        let mut header = String::new();
        let mut body = String::new();
        for (i, o) in packed.iter().enumerate() {
            header.push_str(&format!("{} {} ", i + 1, body.len()));
            body.push_str(o);
            body.push(' ');
        }
        let objstm = zip(format!("{header}{body}").as_bytes());
        let mut out = b"%PDF-1.5\n".to_vec();
        let info_at = out.len();
        out.extend_from_slice(b"6 0 obj\n<< /Producer (Packed) >>\nendobj\n");
        let stm_at = out.len();
        out.extend_from_slice(
            format!(
                "5 0 obj\n<< /Type /ObjStm /N 4 /First {} /Filter /FlateDecode /Length {} >>\nstream\n",
                header.len(),
                objstm.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(&objstm);
        out.extend_from_slice(b"\nendstream\nendobj\n");
        let xref_at = out.len();
        // Rows of W [1 4 2]: type, offset or stream, generation or index.
        let mut rows: Vec<[u8; 7]> = vec![[0, 0, 0, 0, 0, 0xff, 0xff]];
        for i in 0..4u16 {
            let mut r = [2u8, 0, 0, 0, 5, 0, 0];
            r[5..].copy_from_slice(&i.to_be_bytes());
            rows.push(r);
        }
        for at in [stm_at, info_at, xref_at] {
            let mut r = [1u8, 0, 0, 0, 0, 0, 0];
            r[1..5].copy_from_slice(&(at as u32).to_be_bytes());
            rows.push(r);
        }
        let mut predicted = Vec::new();
        let mut prev = [0u8; 7];
        for r in &rows {
            predicted.push(2);
            for i in 0..7 {
                predicted.push(r[i].wrapping_sub(prev[i]));
            }
            prev = *r;
        }
        let xref = zip(&predicted);
        out.extend_from_slice(
            format!(
                "7 0 obj\n<< /Type /XRef /Size 8 /W [1 4 2] /Root 1 0 R /Info 6 0 R \
                 /Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns 7 >> /Length {} >>\nstream\n",
                xref.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(&xref);
        out.extend_from_slice(
            format!("\nendstream\nendobj\nstartxref\n{xref_at}\n%%EOF\n").as_bytes(),
        );
        let info = read_bytes(&out);
        assert_eq!(info.version.as_deref(), Some("1.5"));
        assert_eq!(info.pages, Some(2));
        assert_eq!(info.page_size.unwrap().paper(), Some("Letter"));
        assert_eq!(info.producer.as_deref(), Some("Packed"));
    }

    #[test]
    fn a_file_without_a_header_is_not_a_pdf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.pdf");
        std::fs::write(&path, b"hello").unwrap();
        assert_eq!(read(&path), None);
    }

    #[test]
    fn strings_decode_escapes_octal_and_utf16() {
        let mut lx = Lexer::new(
            br"(a\(b\)\101\
c)",
            0,
        );
        assert_eq!(lx.object(0), Some(Obj::Str(b"a(b)Ac".to_vec())));
        assert_eq!(text(&[0xFE, 0xFF, 0x00, 0xE9]), "\u{e9}");
        assert_eq!(text(b"caf\xe9"), "caf\u{e9}");
    }

    #[test]
    fn papers_are_named_either_way_up() {
        let p = |w, h| PageSize {
            width_pt: w,
            height_pt: h,
        };
        assert_eq!(p(595.0, 842.0).paper(), Some("A4"));
        assert_eq!(p(842.0, 595.0).paper(), Some("A4"));
        assert_eq!(p(500.0, 500.0).paper(), None);
    }
}
