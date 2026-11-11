//! Plain text as conv reads it, and what it becomes for the next step.
//!
//! A `.txt` file says nothing about its encoding. Notepad saves UTF-8, with
//! or without a byte-order mark, or "Unicode", which is UTF-16; older
//! editors on Windows save Windows-1252. pandoc reads only UTF-8 (anything
//! else it reads as Latin-1, so UTF-16 comes out as a spaced-out mess), and
//! a speech engine reads what it is given. So conv decodes the file itself
//! (`decode`) and hands the next step UTF-8: an HTML page for pandoc
//! (`page`), or the text to read aloud (`speakable`).

use std::path::Path;

use serde::Serialize;

use crate::error::{ConvError, ErrorCode, Result};

/// How a text file was written, as `decode` found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    Utf8,
    /// UTF-8 with a byte-order mark, as Notepad saved it until 2019.
    Utf8Bom,
    /// UTF-16 by its byte-order mark, or by the zero bytes every other byte
    /// of English text has in it: Notepad's "Unicode".
    Utf16Le,
    Utf16Be,
    /// Not valid UTF-8: read as Windows-1252, the Western European encoding
    /// Windows editors wrote (Latin-1, plus curly quotes and dashes in
    /// 0x80-0x9F).
    Windows1252,
}

/// What a read of a text source found, for the plan's notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextInfo {
    pub encoding: Encoding,
    /// Nothing but white space: nothing to convert or read aloud.
    pub empty: bool,
}

/// The note for a file read as Windows-1252. The other encodings are
/// unambiguous and need none.
pub const WINDOWS_1252_NOTE: &str = "The text is not UTF-8, so it was read as Windows-1252 \
     (Western European). If letters come out wrong, save it as UTF-8 and convert it again.";

/// Reads a text source for its notes. `None` when it cannot be read; the
/// conversion then reports that itself.
pub fn read(path: &Path) -> Option<std::result::Result<TextInfo, String>> {
    // Only a regular file, as `probe::run` insists: a FIFO named `x.txt`
    // would block a --dry-run forever.
    if !path.is_file() {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    Some(decode(&bytes).map(|(text, encoding)| TextInfo {
        encoding,
        empty: text.trim().is_empty(),
    }))
}

/// Reads what a conversion of text needs before it is planned, into
/// `probed`: how a .txt source is written, and, for text read aloud, say's
/// voices. Shared by the conversion and its `--dry-run`, so both plan the
/// same.
pub fn attach(
    resolver: &crate::Resolver,
    from: crate::Format,
    to: crate::Format,
    input: &Path,
    tuning: &crate::Tuning,
    probed: &mut Option<crate::MediaProbe>,
) {
    if from == crate::Format::Txt {
        if let Some(read) = read(input) {
            probed.get_or_insert_with(crate::MediaProbe::default).text = Some(read);
        }
    }
    if crate::registry::speaks(from, to) {
        let engine = crate::speech::Engine::for_voice(tuning.voice.as_deref());
        if let Some(speech) = crate::speech::read(resolver, engine, tuning.voice.as_deref()) {
            probed.get_or_insert_with(crate::MediaProbe::default).speech = Some(speech);
        }
    }
}

/// The text of `bytes` and how it was written. `Err` names why it is not
/// text at all: zero bytes, as in a binary file, that UTF-16 does not
/// explain.
pub fn decode(bytes: &[u8]) -> std::result::Result<(String, Encoding), String> {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        let text = String::from_utf8_lossy(rest).into_owned();
        return no_nul(text, Encoding::Utf8Bom);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return no_nul(utf16(rest, false), Encoding::Utf16Le);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return no_nul(utf16(rest, true), Encoding::Utf16Be);
    }
    if let Some(big) = utf16_without_mark(bytes) {
        let encoding = if big {
            Encoding::Utf16Be
        } else {
            Encoding::Utf16Le
        };
        return no_nul(utf16(bytes, big), encoding);
    }
    match crate::table::utf8_text(bytes) {
        Some(text) => no_nul(text, Encoding::Utf8),
        None => no_nul(
            bytes
                .iter()
                .map(|&b| crate::table::windows_1252(b))
                .collect(),
            Encoding::Windows1252,
        ),
    }
}

fn no_nul(text: String, encoding: Encoding) -> std::result::Result<(String, Encoding), String> {
    if text.contains('\0') {
        return Err("it holds zero bytes, as binary files do".to_string());
    }
    Ok((text, encoding))
}

fn utf16(bytes: &[u8], big: bool) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| {
            if big {
                u16::from_be_bytes([c[0], c[1]])
            } else {
                u16::from_le_bytes([c[0], c[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// UTF-16 saved without a byte-order mark: in text that is mostly ASCII,
/// every other byte is zero, and the others are not. `Some(true)` for big
/// endian. Judged on the first 4 KB, and only when it holds enough to tell.
fn utf16_without_mark(bytes: &[u8]) -> Option<bool> {
    let head = &bytes[..bytes.len().min(4096) & !1];
    let pairs = head.len() / 2;
    if pairs < 4 {
        return None;
    }
    let zero_at = |odd: bool| {
        head.as_chunks::<2>()
            .0
            .iter()
            .filter(|c| c[usize::from(odd)] == 0)
            .count()
    };
    let (even, odd) = (zero_at(false), zero_at(true));
    // At least two in five characters ASCII, and the other half of each
    // pair almost never zero.
    let mostly = |n: usize| n * 5 >= pairs * 2;
    let rarely = |n: usize| n * 20 <= pairs;
    if mostly(odd) && rarely(even) {
        Some(false)
    } else if mostly(even) && rarely(odd) {
        Some(true)
    } else {
        None
    }
}

/// Reads and decodes a text file for a step, naming the user's file in
/// every failure.
fn load(from: &Path, shown: &Path) -> Result<String> {
    let bytes = std::fs::read(from).map_err(|e| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("could not read {}: {e}", shown.display()),
        )
    })?;
    decode(&bytes)
        .map(|(text, _)| text)
        .map_err(|why| not_text(shown, &why))
}

/// The refusal for a source that is not text.
pub fn not_text(path: &Path, why: &str) -> ConvError {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    ConvError::new(
        ErrorCode::ConversionFailed,
        format!("{name} is not text: {why}"),
    )
}

/// The refusal for a source with nothing in it to read aloud.
pub fn nothing_to_read(path: &Path) -> ConvError {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    ConvError::new(
        ErrorCode::ConversionFailed,
        format!("{name} has no text to read aloud"),
    )
}

/// Line endings as `\n`, and no control characters but tabs and line
/// breaks: a form feed is a paragraph break, and the rest (which no
/// editor shows) would make a .docx that Word refuses to open.
fn normalized(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    text.chars()
        .filter_map(|c| match c {
            '\u{c}' => Some('\n'),
            '\n' | '\t' => Some(c),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

/// Writes `from`, a text file, as an HTML page pandoc reads into any
/// document format. `shown` is the user's file, named in a failure.
pub fn write_page(from: &Path, to: &Path, title: &str, shown: &Path) -> Result<()> {
    let text = load(from, shown)?;
    write(to, &page(&text, title), shown)
}

/// Writes the text in `from` (a .txt source, or what pandoc made of a
/// document) as UTF-8 a speech engine reads aloud. `shown` is the user's
/// file, named in a failure.
pub fn write_speakable(from: &Path, to: &Path, shown: &Path) -> Result<()> {
    let text = speakable(&load(from, shown)?);
    if text.trim().is_empty() {
        return Err(nothing_to_read(shown));
    }
    write(to, &text, shown)
}

fn write(to: &Path, contents: &str, shown: &Path) -> Result<()> {
    std::fs::write(to, contents).map_err(|e| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("could not prepare {}: {e}", shown.display()),
        )
    })
}

/// The text as an HTML page that pandoc reads back as the same text.
///
/// Blank lines separate paragraphs, and a line break inside one is kept,
/// so a poem, an address or a list stays as it was typed. A file with no
/// blank lines at all is written a paragraph to a line, which is how
/// editors that wrap long lines save one. Leading spaces and runs of them
/// are kept as no-break spaces, since HTML would collapse them. The title
/// is the page's own (`pagetitle`), not a heading in the document.
pub fn page(text: &str, title: &str) -> String {
    let text = normalized(text);
    let lines: Vec<&str> = text.lines().collect();
    let blank = |l: &&str| l.trim().is_empty();
    let has_blank_between = {
        let first = lines.iter().position(|l| !blank(l));
        let last = lines.iter().rposition(|l| !blank(l));
        match (first, last) {
            (Some(a), Some(b)) => lines[a..b].iter().any(blank),
            _ => false,
        }
    };
    let paragraphs: Vec<Vec<&str>> = if has_blank_between {
        lines
            .split(blank)
            .filter(|p| !p.is_empty())
            .map(<[&str]>::to_vec)
            .collect()
    } else {
        lines
            .iter()
            .filter(|l| !blank(l))
            .map(|l| vec![*l])
            .collect()
    };
    let mut out = String::with_capacity(text.len() + text.len() / 4 + 256);
    out.push_str("<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"pagetitle\" content=\"");
    out.push_str(&escape(title, true));
    out.push_str("\">\n</head>\n<body>\n");
    for p in paragraphs {
        out.push_str("<p>");
        for (i, line) in p.iter().enumerate() {
            if i > 0 {
                out.push_str("<br>\n");
            }
            out.push_str(&spaced(&escape(line.trim_end(), false)));
        }
        out.push_str("</p>\n");
    }
    out.push_str("</body>\n</html>\n");
    out
}

fn escape(s: &str, attribute: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// Keeps the spaces HTML would collapse: a tab is four spaces, and every
/// space at the start of a line or after another space is a no-break one.
fn spaced(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut after_space = true;
    for c in line.chars() {
        match c {
            '\t' => {
                out.push_str("&nbsp;&nbsp;&nbsp;&nbsp;");
                after_space = true;
            }
            ' ' if after_space => out.push_str("&nbsp;"),
            ' ' => {
                out.push(' ');
                after_space = true;
            }
            c => {
                out.push(c);
                after_space = false;
            }
        }
    }
    out
}

/// The text as a speech engine should be given it.
///
/// say and espeak-ng both read `[[ ]]` as commands inside the text: say's
/// to change the voice, the rate or the volume, espeak-ng's to speak
/// phonemes. So `[[Main Page]]`, a wiki or Obsidian link, was left out of
/// what say read (checked on macOS 26), and `[[volm 0]]` would silence the
/// rest. A space between the brackets makes them plain text; neither engine
/// reads a bracket aloud.
pub fn speakable(text: &str) -> String {
    let text = normalized(text);
    let mut out = String::with_capacity(text.len());
    let mut last = None;
    for c in text.chars() {
        if c == '[' && last == Some('[') {
            out.push(' ');
        }
        out.push(c);
        last = Some(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16le(s: &str, bom: bool) -> Vec<u8> {
        let mut out = if bom { vec![0xFF, 0xFE] } else { vec![] };
        for u in s.encode_utf16() {
            out.extend(u.to_le_bytes());
        }
        out
    }

    fn utf16be(s: &str, bom: bool) -> Vec<u8> {
        let mut out = if bom { vec![0xFE, 0xFF] } else { vec![] };
        for u in s.encode_utf16() {
            out.extend(u.to_be_bytes());
        }
        out
    }

    #[test]
    fn reads_utf8_with_and_without_a_mark() {
        assert_eq!(
            decode("Café".as_bytes()).unwrap(),
            ("Café".to_string(), Encoding::Utf8)
        );
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend("Café".as_bytes());
        assert_eq!(
            decode(&bom).unwrap(),
            ("Café".to_string(), Encoding::Utf8Bom)
        );
    }

    #[test]
    fn reads_utf16_by_its_mark_either_way_round() {
        let text = "Grüße, 日本";
        assert_eq!(
            decode(&utf16le(text, true)).unwrap(),
            (text.to_string(), Encoding::Utf16Le)
        );
        assert_eq!(
            decode(&utf16be(text, true)).unwrap(),
            (text.to_string(), Encoding::Utf16Be)
        );
    }

    #[test]
    fn reads_utf16_without_a_mark_by_its_zero_bytes() {
        let text = "Hello there, this is a note.";
        assert_eq!(
            decode(&utf16le(text, false)).unwrap(),
            (text.to_string(), Encoding::Utf16Le)
        );
        assert_eq!(
            decode(&utf16be(text, false)).unwrap(),
            (text.to_string(), Encoding::Utf16Be)
        );
    }

    #[test]
    fn reads_what_is_not_utf8_as_windows_1252() {
        // "Café “quoted” – naïve" in Windows-1252.
        let bytes = b"Caf\xe9 \x93quoted\x94 \x96 na\xefve";
        assert_eq!(
            decode(bytes).unwrap(),
            ("Café “quoted” – naïve".to_string(), Encoding::Windows1252)
        );
    }

    #[test]
    fn refuses_a_binary_file() {
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01";
        assert!(decode(png).is_err());
        assert!(decode(b"text\0with a zero").is_err());
    }

    #[test]
    fn a_short_file_is_not_mistaken_for_utf16() {
        assert_eq!(decode(b"ok").unwrap().1, Encoding::Utf8);
        assert_eq!(decode(b"").unwrap(), (String::new(), Encoding::Utf8));
    }

    #[test]
    fn blank_lines_make_paragraphs_and_line_breaks_stay() {
        let html = page("Roses are red,\nviolets are blue.\n\nSecond one.\n", "poem");
        assert!(
            html.contains("<p>Roses are red,<br>\nviolets are blue.</p>\n<p>Second one.</p>"),
            "{html}"
        );
        assert!(html.contains(r#"<meta name="pagetitle" content="poem">"#));
        assert!(
            !html.contains("<title>"),
            "a title would print as a heading"
        );
    }

    #[test]
    fn without_blank_lines_every_line_is_a_paragraph() {
        let html = page("First paragraph.\r\nSecond paragraph.\r\n", "t");
        assert!(
            html.contains("<p>First paragraph.</p>\n<p>Second paragraph.</p>"),
            "{html}"
        );
    }

    #[test]
    fn markup_in_the_text_is_text() {
        let html = page("a < b && <script>x</script>", "a \"b\" <c>");
        assert!(html.contains("a &lt; b &amp;&amp; &lt;script&gt;x&lt;/script&gt;"));
        assert!(html.contains(r#"content="a &quot;b&quot; &lt;c&gt;""#));
    }

    #[test]
    fn indentation_and_runs_of_spaces_survive() {
        let html = page("  two\tand  four", "t");
        assert!(
            html.contains("&nbsp;&nbsp;two&nbsp;&nbsp;&nbsp;&nbsp;and &nbsp;four"),
            "{html}"
        );
    }

    #[test]
    fn control_characters_are_dropped_and_a_form_feed_breaks() {
        let html = page("one\u{1}\u{c}two", "t");
        assert!(html.contains("<p>one</p>\n<p>two</p>"), "{html}");
    }

    #[test]
    fn speech_text_has_no_engine_commands() {
        assert_eq!(
            speakable("see [[Main Page]] and [[[x]]]"),
            "see [ [Main Page]] and [ [ [x]]]"
        );
        assert_eq!(speakable("a\r\nb\u{7}"), "a\nb");
    }

    #[test]
    fn a_page_and_speech_are_written_from_any_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("note.txt");
        std::fs::write(&src, utf16le("Hello [[there]]", true)).unwrap();
        let speech = dir.path().join("s.txt");
        write_speakable(&src, &speech, &src).unwrap();
        assert_eq!(
            std::fs::read_to_string(&speech).unwrap(),
            "Hello [ [there]]"
        );
        let html = dir.path().join("p.html");
        write_page(&src, &html, "note", &src).unwrap();
        assert!(std::fs::read_to_string(&html)
            .unwrap()
            .contains("<p>Hello [[there]]</p>"));
    }

    #[test]
    fn nothing_to_read_aloud_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("blank.txt");
        std::fs::write(&src, " \n\n\t").unwrap();
        let e = write_speakable(&src, &dir.path().join("s.txt"), &src).unwrap_err();
        assert_eq!(e.message, "blank.txt has no text to read aloud");
    }

    #[test]
    fn the_read_reports_the_encoding_and_emptiness() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("w.txt");
        std::fs::write(&src, b"Caf\xe9").unwrap();
        assert_eq!(
            read(&src).unwrap().unwrap(),
            TextInfo {
                encoding: Encoding::Windows1252,
                empty: false
            }
        );
        assert!(read(&dir.path().join("missing.txt")).is_none());
    }
}
