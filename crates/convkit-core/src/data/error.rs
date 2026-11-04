//! What went wrong reading or writing a data file, and where: a line, a
//! column, the line itself with a caret under the spot, and what to do.

use serde::Serialize;
use unicode_width::UnicodeWidthChar;

use crate::{ConvError, ErrorCode, Remediation};

/// Where something is in the text: 1-based line and column (the column
/// counts characters, as editors do), and the byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pos {
    pub line: usize,
    pub column: usize,
    pub offset: usize,
}

impl Pos {
    pub(crate) fn at(&self) -> String {
        format!("line {}, column {}", self.line, self.column)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DataErrorKind {
    /// The text is not valid in its language.
    Syntax,
    /// It is valid, but the target cannot hold something in it: a null in
    /// TOML, NaN in JSON, a nested list in a CSV row.
    Unrepresentable,
    /// Too big to hold in memory, or nested too deep.
    TooBig,
    /// The file could not be read or written.
    Io,
}

/// The line an error is on, as shown under the message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snippet {
    pub line: usize,
    /// The line, or the part of a long one around the spot. Tabs are shown
    /// as four spaces and control characters as `·`, so the caret lines up.
    pub text: String,
    /// Columns of `text` before the caret.
    pub caret: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DataError {
    pub kind: DataErrorKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(skip)]
    pub(crate) offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<Box<Snippet>>,
}

impl DataError {
    pub(crate) fn syntax(at: Pos, message: impl Into<String>) -> DataError {
        DataError {
            kind: DataErrorKind::Syntax,
            message: message.into(),
            line: Some(at.line),
            column: Some(at.column),
            hint: None,
            offset: Some(at.offset),
            snippet: None,
        }
    }

    pub(crate) fn new(kind: DataErrorKind, message: impl Into<String>) -> DataError {
        DataError {
            kind,
            message: message.into(),
            line: None,
            column: None,
            hint: None,
            offset: None,
            snippet: None,
        }
    }

    pub(crate) fn io(e: &std::io::Error, what: &str) -> DataError {
        DataError::new(DataErrorKind::Io, format!("couldn't {what}: {e}"))
    }

    pub(crate) fn hint(mut self, hint: impl Into<String>) -> DataError {
        self.hint = Some(hint.into());
        self
    }

    /// The message with its place in front, when it has one.
    pub fn headline(&self) -> String {
        match (self.line, self.column) {
            (Some(l), Some(c)) => format!("line {l}, column {c}: {}", self.message),
            (Some(l), None) => format!("line {l}: {}", self.message),
            _ => self.message.clone(),
        }
    }

    /// The line with a caret under the spot, ready to print:
    ///
    /// ```text
    ///    3 |   "b": 2,
    ///      |         ^
    /// ```
    pub fn snippet_lines(&self) -> Vec<String> {
        let Some(s) = &self.snippet else {
            return Vec::new();
        };
        let number = s.line.to_string();
        let gutter = " ".repeat(number.len());
        vec![
            format!(" {number} | {}", s.text),
            format!(" {gutter} | {}^", " ".repeat(s.caret)),
        ]
    }

    /// As a conversion's error: the file's name and the place first, the
    /// line and caret under it, and the hint as the fix.
    pub fn into_conv_error(self, name: &str) -> ConvError {
        let code = match self.kind {
            DataErrorKind::Syntax => ErrorCode::InvalidData,
            DataErrorKind::Io | DataErrorKind::Unrepresentable | DataErrorKind::TooBig => {
                ErrorCode::ConversionFailed
            }
        };
        let mut message = format!("{name}: {}", self.headline());
        for line in self.snippet_lines() {
            message.push_str("\n  ");
            message.push_str(&line);
        }
        ConvError {
            code,
            message,
            backend: None,
            remediation: self.hint.map(|h| Remediation {
                managed: None,
                manual: Some(h),
            }),
        }
    }

    /// Fills in the line under the error from the text it was found in.
    pub(crate) fn with_snippet_from(mut self, text: &[u8]) -> DataError {
        if self.snippet.is_none() {
            if let (Some(offset), Some(line)) = (self.offset, self.line) {
                self.snippet = Some(Box::new(snippet(text, offset.min(text.len()), line)));
            }
        }
        self
    }
}

/// How far either side of the spot a line is shown, in columns.
const BEFORE: usize = 60;
const AFTER: usize = 40;
/// How far either way the line's ends are looked for, in bytes.
const SEARCH: usize = 4096;

/// The line of `text` holding byte `offset`, shown around it.
pub(crate) fn snippet(text: &[u8], offset: usize, line: usize) -> Snippet {
    let from = offset.saturating_sub(SEARCH);
    let (start, cut_start) = match text[from..offset].iter().rposition(|&b| b == b'\n') {
        Some(i) => (from + i + 1, false),
        None => (from, from > 0),
    };
    let to = (offset + SEARCH).min(text.len());
    let end = text[offset..to]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(to, |i| offset + i);
    let shown = |bytes: &[u8]| -> Vec<(String, usize)> {
        String::from_utf8_lossy(bytes)
            .trim_end_matches('\r')
            .chars()
            .map(|c| match c {
                '\t' => ("    ".to_string(), 4),
                c if c.is_control() => ("·".to_string(), 1),
                c => (c.to_string(), c.width().unwrap_or(0)),
            })
            .collect()
    };
    let before = shown(&text[start..offset]);
    let after = shown(&text[offset..end]);
    // A long line is cut to the part around the spot.
    let mut width: usize = before.iter().map(|(_, w)| w).sum();
    let mut skip = 0;
    let mut cut = cut_start;
    if width > BEFORE {
        while width > BEFORE && skip < before.len() {
            width -= before[skip].1;
            skip += 1;
        }
        cut = true;
    }
    let caret = width + usize::from(cut);
    let mut out = String::new();
    if cut {
        out.push('…');
    }
    for (s, _) in &before[skip..] {
        out.push_str(s);
    }
    let mut width = 0;
    for (i, (s, w)) in after.iter().enumerate() {
        if width + w > AFTER && i + 1 < after.len() {
            out.push('…');
            break;
        }
        out.push_str(s);
        width += w;
    }
    Snippet {
        line,
        text: out.trim_end().to_string(),
        caret,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_caret_sits_under_the_spot() {
        let text = b"{\n  \"a\": 1,\n}\n";
        let s = snippet(text, 10, 2);
        assert_eq!(s.text, "  \"a\": 1,");
        assert_eq!(s.caret, 8);
    }

    #[test]
    fn tabs_and_wide_characters_keep_it_lined_up() {
        let text = "\t\"日本\": x".as_bytes();
        let offset = text.len() - 1;
        let s = snippet(text, offset, 1);
        assert_eq!(s.text, "    \"日本\": x");
        assert_eq!(s.caret, 4 + 1 + 4 + 1 + 2);
    }

    #[test]
    fn a_long_line_is_cut_around_the_spot() {
        let mut text = vec![b'1'; 5000];
        text[4000] = b'x';
        let s = snippet(&text, 4000, 1);
        assert!(s.text.starts_with('…'), "{}", s.text);
        assert!(s.text.ends_with('…'), "{}", s.text);
        assert_eq!(s.text.chars().nth(s.caret), Some('x'), "{}", s.text);
    }

    #[test]
    fn an_error_at_the_end_points_past_the_last_character() {
        let text = b"[1, 2";
        let s = snippet(text, 5, 1);
        assert_eq!(s.text, "[1, 2");
        assert_eq!(s.caret, 5);
    }
}
