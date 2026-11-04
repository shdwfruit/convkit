//! `--subtitles`: burn a subtitle file's text into the picture.
//!
//! ffmpeg can draw subtitles itself only through libass, which Homebrew's
//! ffmpeg is built without (no `subtitles`, `ass` or `drawtext` filter), so
//! conv draws them. Every cue is laid out and drawn in-process with resvg,
//! in the system's own fonts: shaped, with fallback fonts for any script
//! the first lacks, and right-to-left lines set as such. The frames go
//! into one animated PNG timed to the millisecond, each frame only the box
//! its text covers, and ffmpeg lays it over the picture with `overlay`,
//! read in the filter chain through a `movie` source. That needs nothing
//! but `overlay` and `movie`, which every ffmpeg has, and it looks the same
//! on every machine. libass, where an ffmpeg has it, is not used: one route
//! that gives one look is worth more than a second that differs.
//!
//! The text is white with a black outline and a soft shadow, centred near
//! the bottom, its size a fixed share of the picture's shorter side, and
//! wrapped to stay inside the picture. Italic and bold come through from
//! SRT and WebVTT tags and from ASS's `\i` and `\b`; every other tag, and
//! ASS's own styles and positions, are left out, with a note for ASS.
//!
//! Times are the source's: a cut shifts them and a speed change scales
//! them, so a subtitle stays with what is said. The text goes on after any
//! turn and crop, and before any `--resize`, which then scales it with the
//! picture.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg;
use serde::{Serialize, Serializer};

use crate::error::{ConvError, ErrorCode, Result};
use crate::probe::MediaProbe;
use crate::Format;

/// A run of text in one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub italic: bool,
    pub bold: bool,
}

/// One line as the file breaks it.
pub type Line = Vec<Span>;

/// One subtitle: when it shows, in the source's time, and its lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub lines: Vec<Line>,
}

/// The subtitle formats conv reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Srt,
    Vtt,
    /// ASS and SSA.
    Ass,
}

/// A subtitle file, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub kind: Kind,
    pub cues: Vec<Cue>,
    /// The encoding it was read in when that was not UTF-8.
    pub encoding: Option<&'static str>,
}

/// The format of a subtitle file, by its extension.
pub fn kind_of(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "srt" => Some(Kind::Srt),
        "vtt" | "webvtt" => Some(Kind::Vtt),
        "ass" | "ssa" => Some(Kind::Ass),
        _ => None,
    }
}

/// Reads and parses a subtitle file. A file with no cue in it is an error,
/// since burning it in would change nothing.
pub fn read(path: &Path) -> Result<Track> {
    let name = path.display();
    let kind = kind_of(path).ok_or_else(|| {
        ConvError::new(
            ErrorCode::InvalidInvocation,
            format!("{name} is no subtitle file conv reads; it reads .srt, .vtt and .ass"),
        )
    })?;
    let bytes = std::fs::read(path).map_err(|e| {
        let code = if e.kind() == std::io::ErrorKind::NotFound {
            ErrorCode::InputNotFound
        } else {
            ErrorCode::ConversionFailed
        };
        ConvError::new(code, format!("could not read the subtitles {name}: {e}"))
    })?;
    let (text, encoding) = decode(&bytes);
    let cues = parse(&text, kind);
    if cues.is_empty() {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!("found no subtitles in {name}"),
        ));
    }
    Ok(Track {
        kind,
        cues,
        encoding,
    })
}

/// The text of a subtitle file: UTF-8 (a byte-order mark dropped), UTF-16
/// by its byte-order mark, and anything else as Windows-1252, which most
/// old subtitle files that are not UTF-8 are in.
fn decode(bytes: &[u8]) -> (String, Option<&'static str>) {
    if let Some(rest) = bytes.strip_prefix(b"\xEF\xBB\xBF") {
        return (String::from_utf8_lossy(rest).into_owned(), None);
    }
    let utf16 = |rest: &[u8], le: bool| {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| {
                if le {
                    u16::from_le_bytes([c[0], c[1]])
                } else {
                    u16::from_be_bytes([c[0], c[1]])
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(rest) = bytes.strip_prefix(b"\xFF\xFE") {
        return (utf16(rest, true), Some("UTF-16"));
    }
    if let Some(rest) = bytes.strip_prefix(b"\xFE\xFF") {
        return (utf16(rest, false), Some("UTF-16"));
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), None),
        Err(_) => (
            bytes.iter().map(|&b| cp1252(b)).collect(),
            Some("Windows-1252"),
        ),
    }
}

/// One Windows-1252 byte as a char.
fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž',
        '\u{8F}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}',
        'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9F => HIGH[usize::from(b - 0x80)],
        _ => char::from(b),
    }
}

/// Parses a subtitle file's text, in time order.
pub fn parse(text: &str, kind: Kind) -> Vec<Cue> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut cues = match kind {
        Kind::Srt | Kind::Vtt => parse_blocks(&text, kind),
        Kind::Ass => parse_ass(&text),
    };
    cues.retain(|c| c.end_ms > c.start_ms && !c.lines.is_empty());
    cues.sort_by_key(|c| c.start_ms);
    cues
}

/// SRT and WebVTT: blocks apart by blank lines, each a timing line
/// (`00:00:01,000 --> 00:00:04,000`, WebVTT's with its settings after),
/// then the text. An SRT's number, a WebVTT cue's name and its header,
/// notes, styles and regions are no timing line, and are passed over.
fn parse_blocks(text: &str, kind: Kind) -> Vec<Cue> {
    let mut cues = Vec::new();
    for block in text.split("\n\n") {
        let lines: Vec<&str> = block.lines().collect();
        let Some(at) = lines.iter().position(|l| l.contains("-->")) else {
            continue;
        };
        let (start, rest) = lines[at].split_once("-->").expect("checked above");
        let end = rest.split_whitespace().next().unwrap_or("");
        let (Some(start_ms), Some(end_ms)) = (clock_ms(start.trim()), clock_ms(end)) else {
            continue;
        };
        let body = lines[at + 1..].join("\n");
        let lines = markup(&body, kind);
        cues.push(Cue {
            start_ms,
            end_ms,
            lines,
        });
    }
    cues
}

/// `h:mm:ss,mmm`, `mm:ss.mmm` or ASS's `h:mm:ss.cc`, in milliseconds.
fn clock_ms(text: &str) -> Option<u64> {
    let parts: Vec<&str> = text.split(':').collect();
    let (h, m, s) = match parts.as_slice() {
        [h, m, s] => (*h, *m, *s),
        [m, s] => ("0", *m, *s),
        _ => return None,
    };
    let num = |t: &str| -> Option<u64> {
        (!t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))
            .then(|| t.parse().ok())
            .flatten()
    };
    let (whole, frac) = s.split_once([',', '.']).unwrap_or((s, ""));
    let digits: String = frac.chars().chain("000".chars()).take(3).collect();
    let frac_ms = if frac.is_empty() { 0 } else { num(&digits)? };
    Some(((num(h)? * 60 + num(m)?) * 60 + num(whole)?) * 1000 + frac_ms)
}

/// SRT and WebVTT text, with its HTML-like tags: `<i>` and `<b>` kept as
/// style, every other tag (`<font>`, `<c.x>`, `<v Name>`, timestamps) and
/// ASS's `{\an8}` that some SRT files carry left out, and WebVTT's entities
/// read.
fn markup(body: &str, kind: Kind) -> Vec<Line> {
    let mut lines = Vec::new();
    let (mut italic, mut bold) = (0u32, 0u32);
    for raw in body.split('\n') {
        let mut line: Line = Vec::new();
        let mut text = String::new();
        let mut chars = raw.chars().peekable();
        let flush = |text: &mut String, line: &mut Line, italic: u32, bold: u32| {
            if !text.is_empty() {
                push_span(line, std::mem::take(text), italic > 0, bold > 0);
            }
        };
        while let Some(c) = chars.next() {
            match c {
                '<' => {
                    let tag: String = chars.by_ref().take_while(|&c| c != '>').collect();
                    let tag = tag.trim().to_ascii_lowercase();
                    let (closing, name) = match tag.strip_prefix('/') {
                        Some(n) => (true, n),
                        None => (false, tag.as_str()),
                    };
                    let name = name.split([' ', '.']).next().unwrap_or("");
                    flush(&mut text, &mut line, italic, bold);
                    let depth = match name {
                        "i" | "em" => &mut italic,
                        "b" | "strong" => &mut bold,
                        _ => continue,
                    };
                    *depth = if closing {
                        depth.saturating_sub(1)
                    } else {
                        *depth + 1
                    };
                }
                '{' if chars.peek() == Some(&'\\') => {
                    chars.by_ref().take_while(|&c| c != '}').for_each(drop);
                }
                '&' if kind == Kind::Vtt || chars.peek().is_some_and(|c| c.is_alphabetic()) => {
                    let mut name = String::new();
                    while let Some(&n) = chars.peek() {
                        if n == ';' || name.len() > 8 || n.is_whitespace() {
                            break;
                        }
                        name.push(n);
                        chars.next();
                    }
                    let read = match name.as_str() {
                        "amp" => Some('&'),
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "quot" => Some('"'),
                        "apos" | "#39" => Some('\''),
                        "nbsp" => Some('\u{a0}'),
                        "lrm" => Some('\u{200e}'),
                        "rlm" => Some('\u{200f}'),
                        _ => None,
                    };
                    match read {
                        Some(r) if chars.peek() == Some(&';') => {
                            chars.next();
                            text.push(r);
                        }
                        _ => {
                            text.push('&');
                            text.push_str(&name);
                        }
                    }
                }
                _ => text.push(c),
            }
        }
        flush(&mut text, &mut line, italic, bold);
        lines.push(line);
    }
    tidy(lines)
}

/// Adds `text` to `line`, joining the last span when its style is the same.
fn push_span(line: &mut Line, text: String, italic: bool, bold: bool) {
    match line.last_mut() {
        Some(last) if last.italic == italic && last.bold == bold => last.text.push_str(&text),
        _ => line.push(Span { text, italic, bold }),
    }
}

/// Trims each line, and drops the empty ones a file leaves at either end.
fn tidy(lines: Vec<Line>) -> Vec<Line> {
    let mut lines: Vec<Line> = lines
        .into_iter()
        .map(|mut line| {
            if let Some(first) = line.first_mut() {
                first.text = first.text.trim_start().to_string();
            }
            if let Some(last) = line.last_mut() {
                last.text = last.text.trim_end().to_string();
            }
            line.retain(|s| !s.text.is_empty());
            line
        })
        .collect();
    while lines.first().is_some_and(Vec::is_empty) {
        lines.remove(0);
    }
    while lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    lines
}

/// ASS and SSA: the `Dialogue:` lines of `[Events]`, their fields named by
/// its `Format:` line. The text is the last field and may hold commas.
fn parse_ass(text: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut in_events = false;
    let mut fields: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_events = line.eq_ignore_ascii_case("[events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(format) = line.strip_prefix("Format:") {
            fields = format
                .split(',')
                .map(|f| f.trim().to_ascii_lowercase())
                .collect();
            continue;
        }
        let Some(values) = line.strip_prefix("Dialogue:") else {
            continue;
        };
        if fields.is_empty() {
            fields = [
                "layer", "start", "end", "style", "name", "marginl", "marginr", "marginv",
            ]
            .iter()
            .map(|s| s.to_string())
            .chain(["effect".to_string(), "text".to_string()])
            .collect();
        }
        let values: Vec<&str> = values.trim_start().splitn(fields.len(), ',').collect();
        let field = |name: &str| {
            fields
                .iter()
                .position(|f| f == name)
                .and_then(|i| values.get(i))
                .map(|v| v.trim())
        };
        let (Some(start), Some(end), Some(body)) = (
            field("start").and_then(clock_ms),
            field("end").and_then(clock_ms),
            fields
                .iter()
                .position(|f| f == "text")
                .and_then(|i| values.get(i)),
        ) else {
            continue;
        };
        let lines = ass_markup(body);
        cues.push(Cue {
            start_ms: start,
            end_ms: end,
            lines,
        });
    }
    cues
}

/// ASS text: `{...}` override blocks, of which `\i` and `\b` are kept as
/// style; `\N` and `\n` break the line and `\h` is a space that holds. A
/// drawing (`\p1` and on) is no text at all.
fn ass_markup(body: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut line: Line = Vec::new();
    let mut text = String::new();
    let (mut italic, mut bold, mut drawing) = (false, false, false);
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                let block: String = chars.by_ref().take_while(|&c| c != '}').collect();
                if !text.is_empty() {
                    if !drawing {
                        push_span(&mut line, std::mem::take(&mut text), italic, bold);
                    }
                    text.clear();
                }
                for tag in block.split('\\').map(str::trim) {
                    if let Some(v) = tag.strip_prefix('i').filter(|v| v.len() == 1) {
                        italic = v == "1";
                    } else if let Some(v) = tag.strip_prefix('b').filter(|v| !v.is_empty()) {
                        if v.bytes().all(|b| b.is_ascii_digit()) {
                            bold = v.parse::<u32>().is_ok_and(|w| w == 1 || w >= 600);
                        }
                    } else if let Some(v) = tag.strip_prefix('p').filter(|v| !v.is_empty()) {
                        if v.bytes().all(|b| b.is_ascii_digit()) {
                            drawing = v != "0";
                        }
                    }
                }
            }
            '\\' => match chars.peek() {
                Some('N' | 'n') => {
                    chars.next();
                    if !drawing && !text.is_empty() {
                        push_span(&mut line, std::mem::take(&mut text), italic, bold);
                    }
                    text.clear();
                    lines.push(std::mem::take(&mut line));
                }
                Some('h') => {
                    chars.next();
                    text.push('\u{a0}');
                }
                _ => text.push('\\'),
            },
            _ => text.push(c),
        }
    }
    if !drawing && !text.is_empty() {
        push_span(&mut line, text, italic, bold);
    }
    lines.push(line);
    tidy(lines)
}

// --- Planning -------------------------------------------------------------

/// The note on a video re-encoded only to draw its subtitles.
pub(crate) const VIDEO_NOTE: &str = "The video is re-encoded to draw the subtitles on it.";

/// The share of the picture's shorter side a line of text is tall: 5.5%,
/// 59 px on 1080p, as broadcast subtitles are.
const FONT_SHARE: f64 = 0.055;

/// The share of the picture's height kept clear below the text.
const BOTTOM_SHARE: f64 = 0.06;

/// The share of the picture's width a line may take.
const WIDTH_SHARE: f64 = 0.9;

/// How the text is drawn on a picture of one size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub font_px: u32,
    /// The widest a line may be.
    pub max_width: u32,
    /// The space kept clear below the text.
    pub bottom: u32,
}

impl Style {
    /// The style for a picture of `w` by `h`.
    pub fn for_size(w: u32, h: u32) -> Style {
        let short = f64::from(w.min(h));
        Style {
            font_px: ((short * FONT_SHARE).round() as u32).max(10),
            max_width: ((f64::from(w) * WIDTH_SHARE).round() as u32).max(1),
            bottom: (f64::from(h) * BOTTOM_SHARE).round() as u32,
        }
    }
}

/// A stretch of the output with one set of subtitles on screen, in the
/// output's time. No lines is no subtitle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub lines: Vec<Line>,
}

/// Everything drawing the subtitles needs, for `draw`. The plan's: the
/// executor draws it just before the step that reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Render {
    pub file: PathBuf,
    pub width: u32,
    pub height: u32,
    pub style: Style,
    /// Back to back from 0.
    pub segments: Vec<Segment>,
}

impl Serialize for Render {
    /// A summary: the frames themselves are no business of a plan's reader.
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("Render", 4)?;
        st.serialize_field("file", &self.file)?;
        st.serialize_field("size", &[self.width, self.height])?;
        st.serialize_field("font_px", &self.style.font_px)?;
        st.serialize_field(
            "shown",
            &self.segments.iter().filter(|s| !s.lines.is_empty()).count(),
        )?;
        st.end()
    }
}

/// `Render`'s summary through the `Arc` a plan shares it in.
pub(crate) fn serialize_shared<S: Serializer>(
    render: &Arc<Render>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    render.as_ref().serialize(s)
}

/// The subtitles to lay over the picture: the animated PNG `draw` writes
/// at `path`, and what it draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Burn {
    pub path: PathBuf,
    pub render: Arc<Render>,
    /// Lay them over in RGB, for a GIF's frames; else in the video's own
    /// 4:2:0.
    pub rgb: bool,
}

impl Burn {
    /// The filters that lay the subtitles over the picture, to go in the
    /// chain after the turn and crop: the chain so far is labelled, the
    /// animated PNG read by `movie`, and the two overlaid, centred (they
    /// are the picture's own size). `eof_action=pass` leaves the picture
    /// alone after the last frame, which is empty anyway. Ends with the
    /// comma the next filter follows.
    ///
    /// The overlay's format is named: left to pick its own (`auto`) next to
    /// an RGBA input, ffmpeg 9 moved a video to full range (`yuvj420p`)
    /// and handed VP9 a `gbrap` it refuses to encode.
    pub(crate) fn filter(&self) -> String {
        let format = if self.rgb { "rgb" } else { "yuv420" };
        format!(
            "null[convkit_v];movie=filename={}:format_name=apng,format=rgba[convkit_s];\
             [convkit_v][convkit_s]overlay=x=(W-w)/2:y=(H-h)/2:eof_action=pass:format={format},",
            filter_path(&self.path)
        )
    }

    /// What exec does before the step that reads it.
    pub(crate) fn prepare(&self) -> crate::plan::Prepare {
        crate::plan::Prepare::Subtitles {
            path: self.path.clone(),
            render: Arc::clone(&self.render),
        }
    }
}

/// A path as a filter option's value inside a filter graph: escaped once
/// for the option (`\`, `'`, `:`), then again for the graph (`\`, `'`,
/// `[`, `]`, `,`, `;`), as ffmpeg's filtergraph escaping asks. Windows
/// paths are written with `/`, which ffmpeg takes, so only the drive's
/// colon needs escaping there.
pub(crate) fn filter_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let raw = if cfg!(windows) {
        raw.replace('\\', "/")
    } else {
        raw.into_owned()
    };
    let escape = |s: &str, special: &[char]| {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if special.contains(&c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    };
    let option = escape(&raw, &['\\', '\'', ':']);
    escape(&option, &['\\', '\'', '[', ']', ',', ';'])
}

/// Where the animated PNG goes: beside `output`, which in a run is in its
/// scratch folder.
pub(crate) fn burn_path(output: &Path) -> PathBuf {
    output.with_extension("convkit-subs.apng")
}

/// What `--json` says about burned-in subtitles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubtitlesReport {
    pub file: PathBuf,
    /// How many of the file's subtitles were drawn.
    pub drawn: usize,
}

/// Subtitles resolved for one conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    /// `None` when no subtitle falls inside the output.
    pub burn: Option<Burn>,
    pub notes: Vec<String>,
    pub report: SubtitlesReport,
}

/// Whether a file of `f` can have subtitles burned into its own format;
/// `Err` says what to do instead.
pub fn in_place(f: Format) -> std::result::Result<(), String> {
    if crate::sized::is_video_target(f) || f == Format::Gif {
        return Ok(());
    }
    if f.kind() == crate::Kind::Video {
        return Err(format!(
            "conv cannot keep {0} files as {0}; add --to mp4 to burn the subtitles in",
            f.ext()
        ));
    }
    Err(format!(
        "--subtitles does not apply to {0} -> {0}: it draws text onto video and GIFs",
        f.ext()
    ))
}

/// Refuses subtitles where there is no picture to draw them on.
pub(crate) fn refuse(tuning: &crate::Tuning, from: Format, to: Format) -> Result<()> {
    if tuning.subtitles.is_none() {
        return Ok(());
    }
    let video = crate::sized::is_video_target(to) || to == Format::Gif;
    let source = from.kind() == crate::Kind::Video || from == Format::Gif;
    if video && source {
        return Ok(());
    }
    Err(ConvError::new(
        ErrorCode::InvalidInvocation,
        format!(
            "--subtitles does not apply to {} -> {}: it draws text onto video and GIFs",
            from.ext(),
            to.ext()
        ),
    ))
}

/// Resolves the subtitles against `probe`, the source as the output shows
/// it (after any cut, speed change, turn and crop), for an output at
/// `output`. `cut` and `speed` move the cues into the output's time.
pub(crate) fn resolve(
    tuning: &crate::Tuning,
    from: Format,
    to: Format,
    probe: Option<&MediaProbe>,
    cut: Option<&crate::trim::Cut>,
    speed: Option<crate::speed::Speed>,
    output: &Path,
) -> Result<Option<Resolved>> {
    let Some(file) = tuning.subtitles.as_ref() else {
        return Ok(None);
    };
    refuse(tuning, from, to)?;
    let name = file.file_name().map_or_else(
        || file.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let track = probe
        .and_then(|p| p.subtitle_file.as_ref())
        .ok_or_else(|| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                "--subtitles needs ffprobe to read the picture's size; install ffprobe or \
                 check that it runs",
            )
        })?;
    let (width, height) = probe.and_then(|p| p.display_dimensions()).ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            "cannot read the picture size the subtitles are drawn at".to_string(),
        )
    })?;
    let length = probe.and_then(|p| p.duration_ms);
    let start = cut.map_or(0, |c| c.start_ms);
    let stop = cut.and_then(|c| c.end_ms);
    let to_output = |t: u64| {
        let t = t.saturating_sub(start);
        speed.map_or(t, |s| s.scale(t))
    };
    let mut shown = Vec::new();
    for cue in &track.cues {
        let (s, e) = (
            cue.start_ms.max(start),
            stop.map_or(cue.end_ms, |x| cue.end_ms.min(x)),
        );
        if e <= s {
            continue;
        }
        let (s, mut e) = (to_output(s), to_output(e));
        if let Some(len) = length {
            e = e.min(len);
        }
        if e > s {
            shown.push((s, e, &cue.lines));
        }
    }
    let mut notes = Vec::new();
    if let Some(enc) = track.encoding {
        notes.push(format!("{name} is not UTF-8; it was read as {enc}."));
    }
    if track.kind == Kind::Ass {
        notes.push(format!(
            "{name} keeps its text, italics and bold, not its ASS styles or positions: conv \
             draws every line in its own style."
        ));
    }
    let left_out = track.cues.len() - shown.len();
    if left_out > 0 && !shown.is_empty() {
        notes.push(format!(
            "{left_out} of {name}'s {} subtitles fall outside the clip and are left out.",
            track.cues.len()
        ));
    }
    let report = SubtitlesReport {
        file: file.clone(),
        drawn: shown.len(),
    };
    if shown.is_empty() {
        notes.push(format!(
            "None of {name}'s subtitles fall inside the clip, so none were drawn."
        ));
        return Ok(Some(Resolved {
            burn: None,
            notes,
            report,
        }));
    }
    let render = Render {
        file: file.clone(),
        width,
        height,
        style: Style::for_size(width, height),
        segments: segments(&shown),
    };
    Ok(Some(Resolved {
        burn: Some(Burn {
            path: burn_path(output),
            render: Arc::new(render),
            rgb: from == Format::Gif,
        }),
        notes,
        report,
    }))
}

/// The output's time cut where any subtitle starts or ends, each stretch
/// with the subtitles on screen through it, earliest at the bottom: back to
/// back from 0, and ending with an empty one.
fn segments(cues: &[(u64, u64, &Vec<Line>)]) -> Vec<Segment> {
    let mut edges: Vec<u64> = cues.iter().flat_map(|&(s, e, _)| [s, e]).collect();
    edges.push(0);
    edges.sort_unstable();
    edges.dedup();
    let mut out: Vec<Segment> = Vec::new();
    for pair in edges.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let mut on: Vec<&(u64, u64, &Vec<Line>)> =
            cues.iter().filter(|&&(s, e, _)| s <= a && a < e).collect();
        on.sort_by_key(|c| c.0);
        // Earliest at the bottom: the later ones stack above.
        let lines: Vec<Line> = on.iter().rev().flat_map(|c| c.2.iter().cloned()).collect();
        match out.last_mut() {
            Some(last) if last.lines == lines => last.end_ms = b,
            _ => out.push(Segment {
                start_ms: a,
                end_ms: b,
                lines,
            }),
        }
    }
    let end = edges.last().copied().unwrap_or(0);
    out.push(Segment {
        start_ms: end,
        end_ms: end + 1000,
        lines: Vec::new(),
    });
    out
}

/// Reads the subtitle file before planning, onto the probe. Without a
/// probe there is no picture to draw on, and the plan says so. Shared by
/// `exec::run` and the `--dry-run` preview.
pub fn attach(
    from: Format,
    to: Format,
    tuning: &crate::Tuning,
    probed: &mut Option<MediaProbe>,
) -> Result<()> {
    let Some(file) = tuning.subtitles.as_ref() else {
        return Ok(());
    };
    refuse(tuning, from, to)?;
    let track = read(file)?;
    if let Some(p) = probed.as_mut() {
        p.subtitle_file = Some(Arc::new(track));
    }
    Ok(())
}

// --- Drawing --------------------------------------------------------------

/// The fonts a line is set in, first choice first: the Latin ones every
/// system has one of, then ones for the scripts they lack. A line takes the
/// first that holds every letter in it, so Arabic is shaped by a font that
/// joins it rather than letter by letter from wherever each was found.
/// Where none holds them all, resvg fills in each missing letter from any
/// font that has it.
const FAMILIES: &[&str] = &[
    "Helvetica Neue",
    "Helvetica",
    "Arial",
    "Segoe UI",
    "Noto Sans",
    "DejaVu Sans",
    "Liberation Sans",
    // Arabic, Hebrew.
    "Geeza Pro",
    "Arial Hebrew",
    "Noto Sans Arabic",
    "Noto Naskh Arabic",
    "Noto Sans Hebrew",
    "Tahoma",
    // Chinese, Japanese, Korean.
    "PingFang SC",
    "Hiragino Sans",
    "Apple SD Gothic Neo",
    "Noto Sans CJK SC",
    "Noto Sans CJK JP",
    "Noto Sans CJK KR",
    "Microsoft YaHei",
    "Yu Gothic",
    "Malgun Gothic",
    // Thai, Devanagari.
    "Thonburi",
    "Noto Sans Thai",
    "Leelawadee UI",
    "Kohinoor Devanagari",
    "Noto Sans Devanagari",
    "Nirmala UI",
];

/// `FAMILIES` as CSS, with `first` ahead of them.
fn family_list(first: Option<&str>) -> String {
    first
        .into_iter()
        .chain(FAMILIES.iter().copied())
        .map(|f| format!("'{f}'"))
        .chain(std::iter::once("sans-serif".to_string()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What to install for fonts, said when they are missing.
const FONT_FIX: &str = "install a font such as DejaVu Sans or Noto Sans (on Debian and Ubuntu: \
     apt-get install fonts-dejavu-core fonts-noto-core)";

/// The system's fonts, loaded once a run. resvg's last resort for a family
/// it cannot find is the generic serif, which fontdb takes to be Times New
/// Roman; on a system with none of `FAMILIES` and no Times, every line
/// came out blank. The generic families are set to the first of
/// `FAMILIES` the system has, or else to any face it has.
fn fonts() -> Result<Arc<usvg::fontdb::Database>> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    let db = FONTS.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        let has = |name: &str| {
            db.faces()
                .any(|f| f.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)))
        };
        let fallback = FAMILIES
            .iter()
            .find(|f| has(f))
            .map(|f| f.to_string())
            .or_else(|| {
                db.faces()
                    .find_map(|f| f.families.first().map(|(n, _)| n.clone()))
            });
        if let Some(name) = fallback {
            db.set_sans_serif_family(name.clone());
            db.set_serif_family(name);
        }
        Arc::new(db)
    });
    if db.is_empty() {
        return Err(ConvError::new(
            ErrorCode::ConversionFailed,
            format!("found no fonts to draw the subtitles with; {FONT_FIX}"),
        ));
    }
    Ok(Arc::clone(db))
}

/// Whether `c` is drawn as nothing in any font: spaces, and the marks that
/// only steer direction or joining.
fn invisible(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || matches!(
            c,
            '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}'
        )
}

/// Draws `render`'s subtitles into an animated PNG at `path`, the
/// picture's size: one frame a stretch, each only the box its text covers
/// (the first whole, as the format asks), cleared when the next comes, and
/// timed to the millisecond. A frame shows for at most 65.535 s, so a
/// longer stretch is several.
///
/// A letter no font on the system has is left out, and the notes returned
/// name them. If no subtitle has a letter any font can draw, nothing would
/// show at all, which is an error that says what to install rather than a
/// video with blank subtitles.
pub fn draw(render: &Render, path: &Path) -> Result<Vec<String>> {
    let db = fonts()?;
    let opts = usvg::Options {
        fontdb: Arc::clone(&db),
        font_family: "sans-serif".to_string(),
        ..usvg::Options::default()
    };
    let failed = |what: &str, e: &dyn std::fmt::Display| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!(
                "could not draw the subtitles of {}: {what}: {e}",
                render.file.display()
            ),
        )
    };
    let chunks = |s: &Segment| (s.end_ms - s.start_ms).max(1).div_ceil(u64::from(u16::MAX));
    let frames: u64 = render.segments.iter().map(chunks).sum();
    let file = std::fs::File::create(path).map_err(|e| failed("cannot write", &e))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), render.width, render.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let frames = u32::try_from(frames).map_err(|e| failed("too many subtitles", &e))?;
    encoder
        .set_animated(frames, 1)
        .map_err(|e| failed("png", &e))?;
    let mut writer = encoder.write_header().map_err(|e| failed("png", &e))?;
    let mut layout = Layout::new(&opts, &db, render.style);
    let mut first = true;
    let (mut shown, mut missed_all) = (0usize, 0usize);
    for segment in &render.segments {
        let picture = if segment.lines.is_empty() {
            None
        } else {
            let picture = layout.block(&segment.lines, render)?;
            if layout.last_missed_all {
                missed_all += 1;
            }
            shown += 1;
            picture
        };
        let mut left = (segment.end_ms - segment.start_ms).max(1);
        while left > 0 {
            let delay = left.min(u64::from(u16::MAX));
            left -= delay;
            let (x, y, w, h, data) = match (&picture, first) {
                (Some(p), true) => p.whole(render.width, render.height),
                (Some(p), false) => (p.x, p.y, p.w, p.h, p.rgba.clone()),
                (None, true) => (
                    0,
                    0,
                    render.width,
                    render.height,
                    vec![0; render.width as usize * render.height as usize * 4],
                ),
                (None, false) => (0, 0, 1, 1, vec![0; 4]),
            };
            first = false;
            let mut frame = || -> std::result::Result<(), png::EncodingError> {
                writer.reset_frame_position()?;
                writer.set_frame_dimension(w, h)?;
                writer.set_frame_position(x, y)?;
                writer.set_frame_delay(delay as u16, 1000)?;
                writer.set_dispose_op(png::DisposeOp::Background)?;
                writer.set_blend_op(png::BlendOp::Source)?;
                writer.write_image_data(&data)
            };
            frame().map_err(|e| failed("png", &e))?;
        }
    }
    writer.finish().map_err(|e| failed("png", &e))?;
    let name = render.file.display();
    if shown > 0 && missed_all == shown {
        return Err(ConvError::new(
            ErrorCode::ConversionFailed,
            format!(
                "no font on this system has the letters of {name}'s subtitles, so none would \
                 show; {FONT_FIX}, or one for the subtitles' script"
            ),
        ));
    }
    if layout.missing.is_empty() {
        return Ok(Vec::new());
    }
    let mut letters: Vec<char> = layout.missing.iter().copied().collect();
    let more = letters.len().saturating_sub(12);
    letters.truncate(12);
    let more = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    Ok(vec![format!(
        "No font on this system has some letters of {name}'s subtitles ({}{more}), so they \
         are left out; install a font that has them (on Debian and Ubuntu, fonts-noto-core, \
         and fonts-noto-cjk for Chinese, Japanese and Korean).",
        letters.into_iter().collect::<String>()
    )])
}

/// A drawn block of text: its box in the picture, and its pixels, straight
/// (not premultiplied) RGBA.
struct Picture {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    rgba: Vec<u8>,
}

impl Picture {
    /// This block on a whole transparent picture.
    fn whole(&self, width: u32, height: u32) -> (u32, u32, u32, u32, Vec<u8>) {
        let mut data = vec![0u8; width as usize * height as usize * 4];
        for row in 0..self.h {
            let from = (row * self.w) as usize * 4;
            let to = (((self.y + row) * width) + self.x) as usize * 4;
            data[to..to + self.w as usize * 4]
                .copy_from_slice(&self.rgba[from..from + self.w as usize * 4]);
        }
        (0, 0, width, height, data)
    }
}

/// One line as it is set: its spans, whether it runs right to left, and
/// the family that holds all its letters, if one does.
#[derive(Clone)]
struct Set {
    line: Line,
    rtl: bool,
    family: Option<&'static str>,
}

/// Lays lines out and draws them, measuring with the fonts it draws with.
struct Layout<'a> {
    opts: &'a usvg::Options<'a>,
    db: &'a usvg::fontdb::Database,
    style: Style,
    /// Each family's face, looked up once.
    faces: std::collections::HashMap<&'static str, Option<usvg::fontdb::ID>>,
    /// Whether a face holds a letter, looked up once each.
    holds: std::collections::HashMap<(usvg::fontdb::ID, char), bool>,
    /// Every letter no font drew, over all the blocks.
    missing: std::collections::BTreeSet<char>,
    /// Whether the last block drew none of its letters.
    last_missed_all: bool,
}

impl<'a> Layout<'a> {
    fn new(
        opts: &'a usvg::Options<'a>,
        db: &'a usvg::fontdb::Database,
        style: Style,
    ) -> Layout<'a> {
        Layout {
            opts,
            db,
            style,
            faces: std::collections::HashMap::new(),
            holds: std::collections::HashMap::new(),
            missing: std::collections::BTreeSet::new(),
            last_missed_all: false,
        }
    }

    /// The first of `FAMILIES` this system has that holds every letter of
    /// `text`.
    fn family_for(&mut self, text: &str) -> Option<&'static str> {
        let letters: Vec<char> = text
            .chars()
            .filter(|c| !c.is_whitespace() && !c.is_control())
            .filter(|c| !matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}'))
            .collect();
        for &family in FAMILIES {
            let db = self.db;
            let face = *self.faces.entry(family).or_insert_with(|| {
                db.query(&usvg::fontdb::Query {
                    families: &[usvg::fontdb::Family::Name(family)],
                    weight: usvg::fontdb::Weight(600),
                    ..usvg::fontdb::Query::default()
                })
                .filter(|id| {
                    db.face(*id).is_some_and(|f| {
                        f.families
                            .iter()
                            .any(|(n, _)| n.eq_ignore_ascii_case(family))
                    })
                })
            });
            let Some(id) = face else {
                continue;
            };
            let all = letters.iter().all(|&c| {
                *self.holds.entry((id, c)).or_insert_with(|| {
                    db.with_face_data(id, |data, index| {
                        use skrifa::MetadataProvider;
                        skrifa::FontRef::from_index(data, index)
                            .ok()
                            .and_then(|f| f.charmap().map(c))
                            .is_some()
                    })
                    .unwrap_or(false)
                })
            });
            if all {
                return Some(family);
            }
        }
        None
    }

    /// The line's width in pixels as it would be drawn.
    fn width(&self, set: &Set) -> f32 {
        let fs = self.style.font_px;
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><text x="0" y="{fs}" xml:space="preserve" font-family="{}" font-size="{fs}" font-weight="600">{}</text></svg>"#,
            family_list(set.family),
            tspans(&set.line, set.rtl)
        );
        usvg::Tree::from_str(&svg, self.opts)
            .map(|t| t.root().abs_bounding_box().width())
            .unwrap_or(0.0)
    }

    /// `set` broken into lines no wider than the style allows: at spaces,
    /// and within a word that is wider on its own (as CJK text, which has
    /// no spaces, always is).
    fn wrap(&self, set: &Set) -> Vec<Set> {
        let max = self.style.max_width as f32;
        let with = |line: Line| Set {
            line,
            ..set.clone()
        };
        // Words, each with the style it is in, and the space before it,
        // which may end the span before.
        let mut words: Vec<Span> = Vec::new();
        let mut space = false;
        for span in &set.line {
            for (i, word) in span.text.split(' ').enumerate() {
                space |= i > 0;
                if word.is_empty() {
                    continue;
                }
                let text = if space && !words.is_empty() {
                    format!(" {word}")
                } else {
                    word.to_string()
                };
                space = false;
                words.push(Span {
                    text,
                    italic: span.italic,
                    bold: span.bold,
                });
            }
        }
        let mut out: Vec<Set> = Vec::new();
        let mut current: Line = Vec::new();
        for word in words {
            let mut candidate = current.clone();
            push_span(&mut candidate, word.text.clone(), word.italic, word.bold);
            if current.is_empty() || self.width(&with(candidate.clone())) <= max {
                current = candidate;
            } else {
                out.push(with(std::mem::take(&mut current)));
                let bare = word.text.trim_start().to_string();
                push_span(&mut current, bare, word.italic, word.bold);
            }
            // A word wider than a line on its own is broken where it must.
            while text_of(&current).chars().count() > 1 && self.width(&with(current.clone())) > max
            {
                let (head, tail) = self.split_to_fit(set, &current, max);
                out.push(with(head));
                current = tail;
            }
        }
        if !current.is_empty() {
            out.push(with(current));
        }
        out
    }

    /// The longest start of `line` that fits `max`, by characters, and the
    /// rest.
    fn split_to_fit(&self, set: &Set, line: &[Span], max: f32) -> (Line, Line) {
        let chars: Vec<(char, bool, bool)> = line
            .iter()
            .flat_map(|s| s.text.chars().map(move |c| (c, s.italic, s.bold)))
            .collect();
        let build = |part: &[(char, bool, bool)]| {
            let mut l: Line = Vec::new();
            for &(c, i, b) in part {
                push_span(&mut l, c.to_string(), i, b);
            }
            l
        };
        let fits = |part: &[(char, bool, bool)]| {
            self.width(&Set {
                line: build(part),
                ..set.clone()
            }) <= max
        };
        let (mut lo, mut hi) = (1, chars.len() - 1);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if fits(&chars[..mid]) {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let mut tail = build(&chars[lo..]);
        if let Some(first) = tail.first_mut() {
            first.text = first.text.trim_start().to_string();
        }
        tail.retain(|s| !s.text.is_empty());
        (build(&chars[..lo]), tail)
    }

    /// The block's SVG: each line centred, one under another, white with a
    /// black outline (drawn under the fill, so the letters keep their
    /// width), over a hard shadow a little below. A blurred shadow looked
    /// no better and took half the time drawing a subtitle took. `pad` is
    /// the room left around the lines.
    fn svg(&self, sets: &[Set], width: u32, block_h: u32, pad: f64) -> String {
        let fs = f64::from(self.style.font_px);
        let line_h = fs * 1.25;
        let stroke = fs * 0.12;
        let shadow = (fs * 0.05).max(1.0);
        let mut texts = String::new();
        for (i, set) in sets.iter().enumerate() {
            let baseline = pad + line_h * i as f64 + fs * 0.95;
            texts.push_str(&format!(
                r#"<text x="{}" y="{baseline:.1}" xml:space="preserve" font-family="{}">{}</text>"#,
                f64::from(width) / 2.0,
                family_list(set.family),
                tspans(&set.line, set.rtl)
            ));
        }
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{block_h}"><g font-size="{fs}" font-weight="600" text-anchor="middle" stroke-width="{stroke:.1}" stroke-linejoin="round" paint-order="stroke"><g transform="translate(0 {shadow:.1})" fill="#000" stroke="#000" opacity="0.5">{texts}</g><g fill="#fff" stroke="#000">{texts}</g></g></svg>"##
        )
    }

    /// Draws a stretch's lines as a block at the bottom of the picture, and
    /// keeps only the box its pixels cover. `None` when nothing was drawn.
    /// The lines are laid out once as the file breaks them, and measured in
    /// that layout; only a line too wide is wrapped and the block laid out
    /// again.
    fn block(&mut self, lines: &[Line], render: &Render) -> Result<Option<Picture>> {
        let style = self.style;
        let fs = f64::from(style.font_px);
        // Each line as the file has it is one paragraph: its direction is
        // its first strong letter's, for every line it is wrapped into.
        let mut sets: Vec<Set> = Vec::new();
        for line in lines {
            let text = text_of(line);
            let rtl = matches!(
                unicode_bidi::get_base_direction(text.as_str()),
                unicode_bidi::Direction::Rtl
            );
            let family = self.family_for(&text);
            sets.push(Set {
                line: line.clone(),
                rtl,
                family,
            });
        }
        let pad = (fs * 0.3).ceil();
        let parse = |sets: &[Set]| -> Result<(usvg::Tree, u32)> {
            let block_h = (pad * 2.0 + fs * 1.25 * sets.len() as f64).ceil() as u32;
            let block_h = block_h.clamp(1, render.height);
            let svg = self.svg(sets, render.width, block_h, pad);
            let tree = usvg::Tree::from_str(&svg, self.opts).map_err(|e| {
                ConvError::new(
                    ErrorCode::ConversionFailed,
                    format!(
                        "could not lay out the subtitles of {}: {e}",
                        render.file.display()
                    ),
                )
            })?;
            Ok((tree, block_h))
        };
        let (mut tree, mut block_h) = parse(&sets)?;
        // Each line is drawn twice, its shadow first: the last of the
        // widths are the lines'.
        let all = text_widths(tree.root());
        let widths = &all[all.len().saturating_sub(sets.len())..];
        let max = style.max_width as f32;
        if widths.len() == sets.len() && widths.iter().any(|&w| w > max) {
            let wrapped: Vec<Set> = sets
                .iter()
                .zip(widths)
                .flat_map(|(set, &w)| {
                    if w > max {
                        self.wrap(set)
                    } else {
                        vec![set.clone()]
                    }
                })
                .collect();
            (tree, block_h) = parse(&wrapped)?;
        }
        let top = render.height.saturating_sub(style.bottom + block_h);
        let mut pixmap = Pixmap::new(render.width, block_h).ok_or_else(|| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                "the picture is too large to draw subtitles on",
            )
        })?;
        resvg::render(&tree, Transform::identity(), &mut pixmap.as_mut());
        // A letter no font has is laid out as glyph 0 and drawn as nothing,
        // or not laid out at all when no font could be found for the line.
        let mut drawn = std::collections::BTreeSet::new();
        let mut missed = std::collections::BTreeSet::new();
        glyphs(tree.root(), &mut drawn, &mut missed);
        let letters: Vec<char> = sets
            .iter()
            .flat_map(|s| text_of(&s.line).chars().collect::<Vec<_>>())
            .filter(|&c| !invisible(c))
            .collect();
        let absent: Vec<char> = letters
            .iter()
            .copied()
            .filter(|c| missed.contains(c) || !drawn.contains(c))
            .collect();
        self.last_missed_all = !letters.is_empty() && absent.len() == letters.len();
        self.missing.extend(absent);
        Ok(cut_out(&pixmap, top))
    }
}

/// The letters of every text in `group`, as laid out: those a font drew,
/// and those it had no glyph for.
fn glyphs(
    group: &usvg::Group,
    drawn: &mut std::collections::BTreeSet<char>,
    missed: &mut std::collections::BTreeSet<char>,
) {
    for node in group.children() {
        match node {
            usvg::Node::Text(t) => {
                for glyph in t.layouted().iter().flat_map(|s| &s.positioned_glyphs) {
                    let set = if glyph.id.0 == 0 {
                        &mut *missed
                    } else {
                        &mut *drawn
                    };
                    set.extend(glyph.text.chars().filter(|&c| !invisible(c)));
                }
            }
            usvg::Node::Group(g) => glyphs(g, drawn, missed),
            _ => {}
        }
    }
}

/// The width of each text in `group`, in document order.
fn text_widths(group: &usvg::Group) -> Vec<f32> {
    let mut out = Vec::new();
    for node in group.children() {
        match node {
            usvg::Node::Text(t) => out.push(t.bounding_box().width()),
            usvg::Node::Group(g) => out.extend(text_widths(g)),
            _ => {}
        }
    }
    out
}

/// The box of `pixmap`'s drawn pixels, as straight RGBA, placed `top` down
/// the picture. `None` when it is all transparent.
fn cut_out(pixmap: &Pixmap, top: u32) -> Option<Picture> {
    let (w, h) = (pixmap.width(), pixmap.height());
    let data = pixmap.data();
    let alpha = |x: u32, y: u32| data[((y * w + x) * 4 + 3) as usize];
    let rows: Vec<u32> = (0..h)
        .filter(|&y| (0..w).any(|x| alpha(x, y) > 0))
        .collect();
    let (&y0, &y1) = (rows.first()?, rows.last()?);
    let cols = |x: u32| (y0..=y1).any(|y| alpha(x, y) > 0);
    let x0 = (0..w).find(|&x| cols(x))?;
    let x1 = (0..w).rev().find(|&x| cols(x))?;
    let (bw, bh) = (x1 - x0 + 1, y1 - y0 + 1);
    let mut rgba = Vec::with_capacity((bw * bh * 4) as usize);
    for y in y0..=y1 {
        for x in x0..=x1 {
            let i = ((y * w + x) * 4) as usize;
            let a = data[i + 3];
            let un = |c: u8| match a {
                0 => 0,
                255 => c,
                _ => ((u32::from(c) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8,
            };
            rgba.extend([un(data[i]), un(data[i + 1]), un(data[i + 2]), a]);
        }
    }
    Some(Picture {
        x: x0,
        y: top + y0,
        w: bw,
        h: bh,
        rgba,
    })
}

/// A line's text, without its styles.
fn text_of(line: &[Span]) -> String {
    line.iter().map(|s| s.text.as_str()).collect()
}

/// A line as SVG `tspan`s, escaped. A right-to-left line is embedded as
/// one (U+202B ... U+202C): resvg sets every paragraph left to right, which
/// would put a sentence's full stop, and any Latin word in it, on the wrong
/// side.
fn tspans(line: &[Span], rtl: bool) -> String {
    let mut out = String::new();
    let last = line.len().saturating_sub(1);
    for (i, span) in line.iter().enumerate() {
        // resvg drops a space at either end of a tspan, preserved or not,
        // which ran `Hello <i>there</i>` together; one that holds does not.
        let held = hold_ends(&span.text);
        let mut text = escape(&held);
        if rtl && i == 0 {
            text.insert(0, '\u{202B}');
        }
        if rtl && i == last {
            text.push('\u{202C}');
        }
        let style = match (span.italic, span.bold) {
            (true, true) => r#" font-style="italic" font-weight="800""#,
            (true, false) => r#" font-style="italic""#,
            (false, true) => r#" font-weight="800""#,
            (false, false) => "",
        };
        out.push_str(&format!("<tspan{style}>{text}</tspan>"));
    }
    out
}

/// `text` with a space at either end as one that holds (U+00A0).
fn hold_ends(text: &str) -> String {
    let start = text.len() - text.trim_start_matches(' ').len();
    let end = text.trim_end_matches(' ').len().max(start);
    format!(
        "{}{}{}",
        "\u{a0}".repeat(start),
        &text[start..end],
        "\u{a0}".repeat(text.len() - end)
    )
}

/// Text for XML: the five characters it reserves, and control characters
/// it cannot hold.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> Span {
        Span {
            text: text.into(),
            italic: false,
            bold: false,
        }
    }

    fn italic(text: &str) -> Span {
        Span {
            text: text.into(),
            italic: true,
            bold: false,
        }
    }

    #[test]
    fn srt_cues_read_with_their_times_and_tags() {
        let srt = "\u{feff}1\r\n00:00:01,000 --> 00:00:04,500\r\nHello <i>there</i>,\r\n\
                   {\\an8}friend\r\n\r\n2\r\n00:01:02.250 --> 00:01:03,000\r\n&amp; more\r\n";
        let (text, enc) = decode(srt.as_bytes());
        assert_eq!(enc, None);
        let cues = parse(&text, Kind::Srt);
        assert_eq!(cues.len(), 2);
        assert_eq!((cues[0].start_ms, cues[0].end_ms), (1000, 4500));
        assert_eq!(
            cues[0].lines,
            [
                vec![plain("Hello "), italic("there"), plain(",")],
                vec![plain("friend")]
            ]
        );
        assert_eq!((cues[1].start_ms, cues[1].end_ms), (62_250, 63_000));
        // An SRT's & stays an & unless it reads as an entity.
        assert_eq!(cues[1].lines, [vec![plain("& more")]]);
    }

    #[test]
    fn webvtt_cues_skip_the_header_notes_and_settings() {
        let vtt = "WEBVTT - a test\n\nNOTE this is not a cue\n\nSTYLE\n::cue { color: red }\n\n\
                   intro\n00:01.000 --> 00:02.000 align:start line:0\n<v Bob>Hi</v> &lt;3\n\n\
                   01:00:00.000 --> 01:00:01.000\n<c.yellow>שלום</c>\n";
        let cues = parse(vtt, Kind::Vtt);
        assert_eq!(cues.len(), 2);
        assert_eq!((cues[0].start_ms, cues[0].end_ms), (1000, 2000));
        assert_eq!(cues[0].lines, [vec![plain("Hi <3")]]);
        assert_eq!(cues[1].start_ms, 3_600_000);
        assert_eq!(cues[1].lines, [vec![plain("שלום")]]);
    }

    #[test]
    fn ass_dialogue_reads_its_text_field_and_override_tags() {
        let ass = "[Script Info]\nTitle: x\n\n[V4+ Styles]\nFormat: Name, Fontname\nStyle: \
                   Default,Arial\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, \
                   MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.50,0:00:03.00,Default,,0,\
                   0,0,,{\\pos(10,10)}Hello, {\\i1}world{\\i0}\\NSecond\\hline\nComment: 0,0:00:\
                   04.00,0:00:05.00,Default,,0,0,0,,not shown\nDialogue: 0,0:00:06.00,0:00:07.00,\
                   Default,,0,0,0,,{\\p1}m 0 0 l 10 10{\\p0}\n";
        let cues = parse(ass, Kind::Ass);
        assert_eq!(cues.len(), 1, "{cues:?}");
        assert_eq!((cues[0].start_ms, cues[0].end_ms), (1500, 3000));
        assert_eq!(
            cues[0].lines,
            [
                vec![plain("Hello, "), italic("world")],
                vec![plain("Second\u{a0}line")]
            ]
        );
    }

    #[test]
    fn a_file_not_in_utf8_is_read_by_its_mark_or_as_windows_1252() {
        let (text, enc) = decode(b"caf\xe9 \x93hi\x94");
        assert_eq!(text, "café “hi”");
        assert_eq!(enc, Some("Windows-1252"));
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend("é1".encode_utf16().flat_map(|u| u.to_le_bytes()));
        assert_eq!(decode(&utf16), ("é1".to_string(), Some("UTF-16")));
    }

    #[test]
    fn times_read_in_every_form() {
        assert_eq!(clock_ms("00:00:01,000"), Some(1000));
        assert_eq!(clock_ms("1:02:03.5"), Some(3_723_500));
        assert_eq!(clock_ms("02:03.250"), Some(123_250));
        assert_eq!(clock_ms("0:00:01.50"), Some(1500));
        assert_eq!(clock_ms("x:00:01"), None);
        assert_eq!(clock_ms("1"), None);
    }

    /// Overlapping subtitles stack, the earlier at the bottom, and the
    /// output's time is covered back to back from 0, ending empty.
    #[test]
    fn segments_stack_overlapping_cues_and_cover_the_output() {
        let a = vec![vec![plain("a")]];
        let b = vec![vec![plain("b")]];
        let segs = segments(&[(1000, 3000, &a), (2000, 4000, &b)]);
        let lines = |s: &Segment| -> Vec<String> { s.lines.iter().map(|l| text_of(l)).collect() };
        let shape: Vec<(u64, u64, Vec<String>)> = segs
            .iter()
            .map(|s| (s.start_ms, s.end_ms, lines(s)))
            .collect();
        assert_eq!(
            shape,
            [
                (0, 1000, vec![]),
                (1000, 2000, vec!["a".to_string()]),
                (2000, 3000, vec!["b".to_string(), "a".to_string()]),
                (3000, 4000, vec!["b".to_string()]),
                (4000, 5000, vec![]),
            ]
        );
    }

    fn track(cues: Vec<(u64, u64, &str)>) -> MediaProbe {
        MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            width: Some(1920),
            height: Some(1080),
            duration_ms: Some(60_000),
            subtitle_file: Some(Arc::new(Track {
                kind: Kind::Srt,
                encoding: None,
                cues: cues
                    .into_iter()
                    .map(|(s, e, t)| Cue {
                        start_ms: s,
                        end_ms: e,
                        lines: vec![vec![plain(t)]],
                    })
                    .collect(),
            })),
            ..MediaProbe::default()
        }
    }

    fn tuned() -> crate::Tuning {
        crate::Tuning {
            subtitles: Some(PathBuf::from("subs/film.srt")),
            ..crate::Tuning::default()
        }
    }

    /// A cut shifts the subtitles and a speed change scales them, so they
    /// stay with what is said; one outside the clip is left out.
    #[test]
    fn cues_move_into_the_output_time() {
        let p = track(vec![
            (1000, 2000, "gone"),
            (12_000, 14_000, "kept"),
            (19_000, 25_000, "cut"),
        ]);
        let cut = crate::trim::Cut {
            start_ms: 10_000,
            end_ms: Some(20_000),
        };
        let p = MediaProbe {
            duration_ms: Some(5_000),
            ..p
        };
        let speed = crate::speed::parse("2").unwrap();
        let r = resolve(
            &tuned(),
            Format::Mp4,
            Format::Mp4,
            Some(&p),
            Some(&cut),
            Some(speed),
            Path::new("out.mp4"),
        )
        .unwrap()
        .unwrap();
        let burn = r.burn.unwrap();
        let shown: Vec<(u64, u64, String)> = burn
            .render
            .segments
            .iter()
            .filter(|s| !s.lines.is_empty())
            .map(|s| (s.start_ms, s.end_ms, text_of(&s.lines[0])))
            .collect();
        assert_eq!(
            shown,
            [
                (1000, 2000, "kept".to_string()),
                (4500, 5000, "cut".to_string())
            ]
        );
        assert_eq!(r.report.drawn, 2);
        assert_eq!(
            r.notes,
            ["1 of film.srt's 3 subtitles fall outside the clip and are left out."]
        );
        assert_eq!(burn.path, PathBuf::from("out.convkit-subs.apng"));
        assert_eq!(burn.render.style, Style::for_size(1920, 1080));
        assert_eq!(burn.render.style.font_px, 59);
    }

    #[test]
    fn subtitles_go_only_where_there_is_a_picture() {
        let p = track(vec![(0, 1000, "x")]);
        for (from, to) in [(Format::Mp4, Format::Mp3), (Format::Png, Format::Jpg)] {
            let e = resolve(&tuned(), from, to, Some(&p), None, None, Path::new("o")).unwrap_err();
            assert!(
                e.message.contains("it draws text onto video and GIFs"),
                "{e}"
            );
        }
        assert!(resolve(
            &tuned(),
            Format::Gif,
            Format::Gif,
            Some(&p),
            None,
            None,
            Path::new("o")
        )
        .is_ok());
    }

    /// The path survives both levels of ffmpeg's filter graph escaping.
    #[test]
    fn a_path_is_escaped_for_a_filter_graph() {
        assert_eq!(
            filter_path(Path::new("/tmp/a b, c'd [x];y:z/s.apng")),
            r"/tmp/a b\, c\\\'d \[x\]\;y\\:z/s.apng"
        );
    }

    #[test]
    fn a_line_is_escaped_and_a_right_to_left_one_embedded() {
        assert_eq!(
            tspans(&[plain("a<b & \"c\"")], false),
            "<tspan>a&lt;b &amp; &quot;c&quot;</tspan>"
        );
        assert_eq!(
            tspans(&[plain("שלום "), italic("עולם")], true),
            "<tspan>\u{202B}שלום\u{a0}</tspan><tspan font-style=\"italic\">עולם\u{202C}</tspan>"
        );
        assert_eq!(hold_ends("  a b "), "\u{a0}\u{a0}a b\u{a0}");
        assert_eq!(hold_ends("   "), "\u{a0}\u{a0}\u{a0}");
    }

    /// Needs a system font. A letter no font has is left out, and said; a
    /// file none of whose letters any font has is refused rather than burned
    /// in blank. U+10FFFD is a private-use letter no font maps.
    #[test]
    #[ignore = "needs system fonts; run with --ignored"]
    fn letters_no_font_has_are_said_and_none_at_all_is_refused() {
        let render = |text: &str| Render {
            file: PathBuf::from("film.srt"),
            width: 320,
            height: 240,
            style: Style::for_size(320, 240),
            segments: vec![
                Segment {
                    start_ms: 0,
                    end_ms: 500,
                    lines: vec![vec![plain(text)]],
                },
                Segment {
                    start_ms: 500,
                    end_ms: 1500,
                    lines: Vec::new(),
                },
            ],
        };
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("subs.apng");
        assert_eq!(draw(&render("Hello"), &out).unwrap(), Vec::<String>::new());
        let notes = draw(&render("Hello \u{10FFFD}"), &out).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("(\u{10FFFD})"), "{notes:?}");
        let e = draw(&render("\u{10FFFD}\u{10FFFD}"), &out).unwrap_err();
        assert!(
            e.message.contains("no font on this system has the letters"),
            "{e}"
        );
        assert!(e.message.contains("fonts-dejavu-core"), "{e}");
    }

    /// Needs a system font. Text wider than the picture is wrapped, at
    /// spaces, or within a run of CJK, which has none.
    #[test]
    #[ignore = "needs system fonts; run with --ignored"]
    fn a_long_line_wraps_inside_the_picture() {
        let db = fonts().unwrap();
        let opts = usvg::Options {
            fontdb: Arc::clone(&db),
            ..usvg::Options::default()
        };
        let style = Style::for_size(640, 360);
        let mut layout = Layout::new(&opts, &db, style);
        let set = |line: Line, layout: &mut Layout| Set {
            family: layout.family_for(&text_of(&line)),
            line,
            rtl: false,
        };
        let line = vec![plain(
            "This line is far too long to fit on a small picture in one go, so it has to be \
             broken into two or more lines",
        )];
        let styled = vec![
            plain("A styled line, far too long for one go: "),
            italic("this part leans"),
            plain(" and this part does not, at all"),
        ];
        let styled_set = set(styled.clone(), &mut layout);
        let lines = layout.wrap(&styled_set);
        assert_eq!(
            lines
                .iter()
                .map(|l| text_of(&l.line))
                .collect::<Vec<_>>()
                .join(" "),
            text_of(&styled)
        );
        let long = set(line.clone(), &mut layout);
        let wrapped = layout.wrap(&long);
        assert!(wrapped.len() >= 2, "{}", wrapped.len());
        for l in &wrapped {
            assert!(layout.width(l) <= style.max_width as f32, "{:?}", l.line);
        }
        assert_eq!(
            wrapped
                .iter()
                .map(|l| text_of(&l.line))
                .collect::<Vec<_>>()
                .join(" "),
            text_of(&line)
        );
        let cjk = vec![plain(&"字幕".repeat(30))];
        let wide = set(cjk.clone(), &mut layout);
        let wrapped = layout.wrap(&wide);
        assert!(wrapped.len() >= 2, "{}", wrapped.len());
        assert_eq!(
            wrapped.iter().map(|l| text_of(&l.line)).collect::<String>(),
            text_of(&cjk)
        );
    }
}
