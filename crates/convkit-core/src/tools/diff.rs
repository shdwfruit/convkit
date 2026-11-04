//! `conv diff`: what changed between two texts, line by line, with the
//! changed words marked inside each changed line.
//!
//! By default a CRLF line ending and an LF one are the same line ending,
//! as a person reading the text sees them; `Options::eol` counts them as a
//! change. Either way the result says when the endings differ.

use std::time::Duration;

use similar::{Algorithm, ChangeTag, TextDiff};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Lines of unchanged text around each change.
    pub context: usize,
    /// Count CRLF against LF as a change.
    pub eol: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            context: 3,
            eol: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    None,
    Lf,
    Crlf,
    Mixed,
}

impl Eol {
    pub fn of(text: &str) -> Eol {
        let crlf = text.matches("\r\n").count();
        let lf = text.matches('\n').count() - crlf;
        match (crlf, lf) {
            (0, 0) => Eol::None,
            (0, _) => Eol::Lf,
            (_, 0) => Eol::Crlf,
            _ => Eol::Mixed,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Eol::None => "no line breaks",
            Eol::Lf => "LF",
            Eol::Crlf => "CRLF",
            Eol::Mixed => "a mix of CRLF and LF",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Equal,
    Delete,
    Insert,
}

impl Op {
    pub fn key(self) -> &'static str {
        match self {
            Op::Equal => "equal",
            Op::Delete => "delete",
            Op::Insert => "insert",
        }
    }
}

/// One line of a hunk, in pieces: `true` marks a piece that changed
/// inside a changed line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub op: Op,
    /// 1-based line numbers in the old and new text.
    pub old: Option<usize>,
    pub new: Option<usize>,
    pub parts: Vec<(bool, String)>,
    /// The last line of its text, with no line break after it.
    pub no_newline: bool,
}

impl Line {
    pub fn text(&self) -> String {
        self.parts.iter().map(|(_, s)| s.as_str()).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// As in a unified diff's `@@ -old_start,old_len +new_start,new_len @@`.
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub lines: Vec<Line>,
}

impl Hunk {
    pub fn header(&self) -> String {
        let range = |start: usize, len: usize| match len {
            1 => format!("{start}"),
            _ => format!("{start},{len}"),
        };
        format!(
            "@@ -{} +{} @@",
            range(self.old_start, self.old_len),
            range(self.new_start, self.new_len)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub hunks: Vec<Hunk>,
    /// The two line endings, when they differ.
    pub eol: Option<(Eol, Eol)>,
    pub added: usize,
    pub removed: usize,
}

impl Diff {
    pub fn same(&self) -> bool {
        self.hunks.is_empty()
    }
}

pub fn diff(old: &str, new: &str, opts: Options) -> Diff {
    let (old_eol, new_eol) = (Eol::of(old), Eol::of(new));
    let endings_differ = old_eol != new_eol && old_eol != Eol::None && new_eol != Eol::None;
    let (old_n, new_n);
    let (old, new) = if opts.eol {
        (old, new)
    } else {
        old_n = old.replace("\r\n", "\n");
        new_n = new.replace("\r\n", "\n");
        (old_n.as_str(), new_n.as_str())
    };
    let text_diff = TextDiff::configure()
        .algorithm(Algorithm::Patience)
        .timeout(Duration::from_secs(10))
        .diff_lines(old, new);
    let mut hunks = Vec::new();
    let (mut added, mut removed) = (0, 0);
    for group in text_diff.grouped_ops(opts.context) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let old_range = first.old_range().start..last.old_range().end;
        let new_range = first.new_range().start..last.new_range().end;
        let mut lines = Vec::new();
        for op in &group {
            for change in text_diff.iter_inline_changes(op) {
                let op = match change.tag() {
                    ChangeTag::Equal => Op::Equal,
                    ChangeTag::Delete => {
                        removed += 1;
                        Op::Delete
                    }
                    ChangeTag::Insert => {
                        added += 1;
                        Op::Insert
                    }
                };
                let mut parts: Vec<(bool, String)> = change
                    .iter_strings_lossy()
                    .map(|(emph, s)| (emph, s.into_owned()))
                    .collect();
                // The line break is the line's end, not part of its text.
                // With `eol` a CR before it stays, as the change it is.
                if let Some((_, last)) = parts.last_mut() {
                    if last.ends_with('\n') {
                        last.pop();
                    }
                }
                parts.retain(|(_, s)| !s.is_empty());
                // A line that changed throughout is shown whole, not as
                // one long marked word.
                let kept_any = parts
                    .iter()
                    .any(|(changed, s)| !changed && !s.trim().is_empty());
                if !kept_any {
                    for part in &mut parts {
                        part.0 = false;
                    }
                }
                lines.push(Line {
                    op,
                    old: change.old_index().map(|i| i + 1),
                    new: change.new_index().map(|i| i + 1),
                    parts,
                    no_newline: change.missing_newline(),
                });
            }
        }
        let start = |r: &std::ops::Range<usize>| {
            if r.is_empty() {
                r.start
            } else {
                r.start + 1
            }
        };
        hunks.push(Hunk {
            old_start: start(&old_range),
            old_len: old_range.len(),
            new_start: start(&new_range),
            new_len: new_range.len(),
            lines,
        });
    }
    Diff {
        hunks,
        eol: endings_differ.then_some((old_eol, new_eol)),
        added,
        removed,
    }
}

/// Whether bytes are text conv can show: UTF-8 with no NUL.
pub fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Options {
        Options::default()
    }

    #[test]
    fn the_same_text_has_no_hunks() {
        let d = diff("a\nb\n", "a\nb\n", opts());
        assert!(d.same());
        assert_eq!(d.eol, None);
    }

    #[test]
    fn a_changed_word_is_marked_inside_its_line() {
        let d = diff("the quick brown fox\n", "the quick red fox\n", opts());
        assert_eq!(d.hunks.len(), 1);
        let h = &d.hunks[0];
        assert_eq!(h.header(), "@@ -1 +1 @@");
        assert_eq!(h.lines.len(), 2);
        let del = &h.lines[0];
        assert_eq!(del.op, Op::Delete);
        assert_eq!(del.text(), "the quick brown fox");
        let changed: Vec<&str> = del
            .parts
            .iter()
            .filter(|p| p.0)
            .map(|p| p.1.as_str())
            .collect();
        assert_eq!(changed, ["brown"]);
        let ins = &h.lines[1];
        let changed: Vec<&str> = ins
            .parts
            .iter()
            .filter(|p| p.0)
            .map(|p| p.1.as_str())
            .collect();
        assert_eq!(changed, ["red"]);
        assert_eq!((d.added, d.removed), (1, 1));
    }

    #[test]
    fn hunks_carry_context_and_numbers() {
        let old: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 10\n", "line ten\n");
        let d = diff(&old, &new, opts());
        let h = &d.hunks[0];
        assert_eq!(h.header(), "@@ -7,7 +7,7 @@");
        assert_eq!(h.lines.first().unwrap().old, Some(7));
        let del = h.lines.iter().find(|l| l.op == Op::Delete).unwrap();
        assert_eq!(del.old, Some(10));
        let d = diff(
            &old,
            &new,
            Options {
                context: 0,
                eol: false,
            },
        );
        assert_eq!(d.hunks[0].header(), "@@ -10 +10 @@");
    }

    #[test]
    fn added_and_removed_lines() {
        let d = diff("a\nb\n", "a\nb\nc\n", opts());
        assert_eq!(d.hunks[0].header(), "@@ -1,2 +1,3 @@");
        assert_eq!((d.added, d.removed), (1, 0));
        let d = diff("", "new\n", opts());
        assert_eq!(d.hunks[0].header(), "@@ -0,0 +1 @@");
    }

    #[test]
    fn crlf_against_lf_is_the_same_by_default_and_said() {
        let d = diff("one\r\ntwo\r\n", "one\ntwo\n", opts());
        assert!(d.same());
        assert_eq!(d.eol, Some((Eol::Crlf, Eol::Lf)));
        let d = diff(
            "one\r\ntwo\r\n",
            "one\ntwo\n",
            Options {
                context: 3,
                eol: true,
            },
        );
        assert!(!d.same());
        assert_eq!((d.added, d.removed), (2, 2));
        // A real change in a CRLF file shows without a stray \r.
        let d = diff("a\r\nb\r\n", "a\r\nc\r\n", opts());
        let del = d.hunks[0]
            .lines
            .iter()
            .find(|l| l.op == Op::Delete)
            .unwrap();
        assert_eq!(del.text(), "b");
    }

    #[test]
    fn a_missing_final_newline_is_noted() {
        let d = diff("a\nb", "a\nb\n", opts());
        assert!(!d.same());
        let del = d.hunks[0]
            .lines
            .iter()
            .find(|l| l.op == Op::Delete)
            .unwrap();
        assert!(del.no_newline);
    }

    #[test]
    fn a_line_that_changed_throughout_is_not_marked() {
        let d = diff(
            "one\nthe quick brown fox\n",
            "one\nthe quick red fox\nthree\n",
            opts(),
        );
        let added = d.hunks[0].lines.last().unwrap();
        assert_eq!(added.text(), "three");
        assert!(added.parts.iter().all(|p| !p.0));
        let d = diff("x\ny\n", "x\nz\n", opts());
        assert!(d.hunks[0]
            .lines
            .iter()
            .all(|l| l.parts.iter().all(|p| !p.0)));
    }

    #[test]
    fn unrelated_lines_are_not_marked_word_by_word() {
        let d = diff(
            "completely different\n",
            "nothing alike here at all\n",
            opts(),
        );
        assert!(d.hunks[0]
            .lines
            .iter()
            .all(|l| l.parts.iter().all(|p| !p.0)));
    }

    #[test]
    fn text_or_not() {
        assert!(is_text("héllo\n".as_bytes()));
        assert!(!is_text(b"\x89PNG\r\n\x1a\n\0\0"));
        assert!(!is_text(b"\xff\xfe"));
    }
}
