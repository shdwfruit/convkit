//! `conv diff`: what changed between two files or texts.

use std::path::{Path, PathBuf};

use convkit_core::tools::diff::{self as d, Diff, Line, Op, Options};
use convkit_core::ConvError;
use serde_json::{json, Value};

use super::{Done, Paint};
use crate::cli::Cli;

pub const ABOUT: &str = "\
Shows what changed from OLD to NEW, as a unified diff with the changed \
words inside each changed line marked: coloured on a terminal, and \
[-old-] {+new+} when piped. --unified leaves the words unmarked, for \
patch and git apply.

Exits 0 when they are the same, 1 when they differ and 2 on trouble, \
as diff does. -q prints nothing, for a script that only wants that.

A CRLF line ending and an LF one count as the same, and conv says when \
the files' endings differ; --eol counts them as a change. - reads one \
side from what is piped in, and --text compares the two arguments as \
texts.";

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// The old file; - for what is piped in.
    #[arg(value_name = "OLD")]
    pub old: PathBuf,
    /// The new file; - for what is piped in.
    #[arg(value_name = "NEW")]
    pub new: PathBuf,
    /// Compare OLD and NEW as texts, not files of those names.
    #[arg(long)]
    pub text: bool,
    /// Lines of unchanged text around each change.
    #[arg(short = 'U', long, value_name = "N", default_value_t = 3)]
    pub context: usize,
    /// Count CRLF against LF line endings as a change.
    #[arg(long)]
    pub eol: bool,
    /// A plain unified diff, without the changed words marked.
    #[arg(long)]
    pub unified: bool,
    /// Also copy the diff to the clipboard.
    #[arg(long)]
    pub copy: bool,
}

/// Exit codes as diff(1) has them.
const SAME: i32 = 0;
const DIFFERENT: i32 = 1;
const TROUBLE: i32 = 2;

pub fn run(cli: &Cli, args: &Args) -> i32 {
    match diff(cli, args) {
        Ok(code) => code,
        Err(e) => {
            crate::render::print_error(cli.json, &e);
            TROUBLE
        }
    }
}

struct Side {
    name: String,
    bytes: Vec<u8>,
}

fn side(cli: &Cli, p: &Path, text: bool, stdin_taken: &mut bool) -> Result<Side, ConvError> {
    if text {
        let t = p.to_string_lossy().into_owned();
        if Path::new(&t).is_file() {
            super::note(
                cli,
                &format!("compared the text {t:?}, not the file of that name; leave out --text to compare files"),
            );
        }
        // A text typed as an argument has no final line break, and saying
        // so for both sides would only be noise.
        let mut bytes = t.into_bytes();
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        return Ok(Side {
            name: "text".to_string(),
            bytes,
        });
    }
    if super::is_dash(p) {
        if *stdin_taken {
            return Err(super::usage("only one side can be - (what is piped in)"));
        }
        *stdin_taken = true;
        return Ok(Side {
            name: "-".to_string(),
            bytes: super::stdin()?.bytes,
        });
    }
    let bytes = super::read_file(p).map_err(|mut e| {
        if e.code == convkit_core::ErrorCode::InputNotFound {
            e.message.push_str("; to compare texts, use --text");
        }
        e
    })?;
    Ok(Side {
        name: p.display().to_string(),
        bytes,
    })
}

fn diff(cli: &Cli, args: &Args) -> Result<i32, ConvError> {
    let mut stdin_taken = false;
    let old = side(cli, &args.old, args.text, &mut stdin_taken)?;
    let new = side(cli, &args.new, args.text, &mut stdin_taken)?;
    let (old_name, new_name) = if args.text {
        ("a".to_string(), "b".to_string())
    } else {
        (old.name.clone(), new.name.clone())
    };
    // Bytes that aren't text are only the same or not, as diff says them.
    if !d::is_text(&old.bytes) || !d::is_text(&new.bytes) {
        let same = old.bytes == new.bytes;
        let human = if same {
            String::new()
        } else {
            format!("Binary files {old_name} and {new_name} differ\n")
        };
        let json = json!({ "same": same, "binary": true, "old": old_name, "new": new_name });
        let code = if same { SAME } else { DIFFERENT };
        return Ok(output(cli, args, human, json, code));
    }
    let old_text = String::from_utf8(old.bytes).expect("checked");
    let new_text = String::from_utf8(new.bytes).expect("checked");
    let result = d::diff(
        &old_text,
        &new_text,
        Options {
            context: args.context,
            eol: args.eol,
        },
    );
    if let Some((a, b)) = result.eol {
        let what = if result.same() {
            "they are the same apart from their line endings: "
        } else {
            ""
        };
        let how = if args.eol {
            "they are counted as changes"
        } else {
            "--eol counts that as a change"
        };
        super::note(
            cli,
            &format!(
                "{what}{old_name} ends its lines with {} and {new_name} with {}; {how}",
                a.name(),
                b.name()
            ),
        );
    }
    if result.same() {
        if !cli.json && result.eol.is_none() {
            super::note(cli, "no differences");
        }
        let json = json_of(&result, &old_name, &new_name);
        return Ok(output(cli, args, String::new(), json, SAME));
    }
    let marks = !args.unified;
    let human = render(&result, &old_name, &new_name, Paint::stdout(), marks);
    let plain = render(&result, &old_name, &new_name, Paint { on: false }, marks);
    let json = json_of(&result, &old_name, &new_name);
    let mut done = Done::new(human, Some(plain), json).code(DIFFERENT);
    if cli.quiet && !cli.json {
        done.human.clear();
    }
    Ok(super::finish(cli, args.copy, done))
}

fn output(cli: &Cli, args: &Args, human: String, json: Value, code: i32) -> i32 {
    let human = if cli.quiet { String::new() } else { human };
    let copy = (!human.is_empty()).then(|| human.clone());
    super::finish(cli, args.copy, Done::new(human, copy, json).code(code))
}

/// The diff as text: unified, with changed words marked when `marks`,
/// in colour when `paint` is on and as `[-old-]` `{+new+}` when not.
pub fn render(diff: &Diff, old: &str, new: &str, paint: Paint, marks: bool) -> String {
    let mut out = String::new();
    out.push_str(&paint.bold(&format!("--- {old}")));
    out.push('\n');
    out.push_str(&paint.bold(&format!("+++ {new}")));
    out.push('\n');
    for hunk in &diff.hunks {
        out.push_str(&paint.cyan(&hunk.header()));
        out.push('\n');
        for line in &hunk.lines {
            out.push_str(&line_text(line, paint, marks));
            out.push('\n');
            if line.no_newline {
                out.push_str("\\ No newline at end of file\n");
            }
        }
    }
    out
}

fn line_text(line: &Line, paint: Paint, marks: bool) -> String {
    use anstyle::{AnsiColor, Color, Style};
    let (sign, open, close) = match line.op {
        Op::Equal => (" ", "", ""),
        Op::Delete => ("-", "[-", "-]"),
        Op::Insert => ("+", "{+", "+}"),
    };
    let colour = |s: &str| match line.op {
        Op::Equal => s.to_string(),
        Op::Delete => paint.red(s),
        Op::Insert => paint.green(s),
    };
    // A changed word: the line's colour behind it.
    let emphasis = |s: &str| {
        let (bg, fg) = match line.op {
            Op::Insert => (AnsiColor::Green, AnsiColor::Black),
            _ => (AnsiColor::Red, AnsiColor::BrightWhite),
        };
        paint.style(
            s,
            Style::new()
                .fg_color(Some(Color::Ansi(fg)))
                .bg_color(Some(Color::Ansi(bg))),
        )
    };
    let mut out = colour(sign);
    let last = line.parts.len().saturating_sub(1);
    for (i, (changed, text)) in line.parts.iter().enumerate() {
        // A CR kept by --eol, shown rather than left invisible; a plain
        // unified diff keeps it as it is, for patch.
        let shown_cr;
        let text = match text.strip_suffix('\r') {
            Some(body) if i == last && marks => {
                shown_cr = format!("{body}\u{240d}");
                &shown_cr
            }
            _ => text,
        };
        if *changed && marks && line.op != Op::Equal {
            if paint.on {
                out.push_str(&emphasis(text));
            } else {
                out.push_str(open);
                out.push_str(text);
                out.push_str(close);
            }
        } else {
            out.push_str(&colour(text));
        }
    }
    out
}

fn json_of(diff: &Diff, old: &str, new: &str) -> Value {
    let hunks: Vec<Value> = diff
        .hunks
        .iter()
        .map(|h| {
            json!({
                "header": h.header(),
                "old_start": h.old_start,
                "old_lines": h.old_len,
                "new_start": h.new_start,
                "new_lines": h.new_len,
                "lines": h.lines.iter().map(|l| json!({
                    "op": l.op.key(),
                    "old": l.old,
                    "new": l.new,
                    "text": l.text(),
                    "parts": l.parts.iter().map(|(c, t)| json!({ "changed": c, "text": t })).collect::<Vec<_>>(),
                    "no_newline": l.no_newline,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "same": diff.same(),
        "binary": false,
        "old": old,
        "new": new,
        "added": diff.added,
        "removed": diff.removed,
        "eol": diff.eol.map(|(a, b)| json!({ "old": a.name(), "new": b.name() })),
        "hunks": hunks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piped_marks_and_unified_plain() {
        let diff = d::diff("a b c\n", "a x c\n", Options::default());
        let marked = render(&diff, "old", "new", Paint { on: false }, true);
        assert_eq!(
            marked,
            "--- old\n+++ new\n@@ -1 +1 @@\n-a [-b-] c\n+a {+x+} c\n"
        );
        let plain = render(&diff, "old", "new", Paint { on: false }, false);
        assert!(plain.ends_with("-a b c\n+a x c\n"), "{plain}");
    }

    #[test]
    fn a_terminal_gets_colour_instead_of_marks() {
        let diff = d::diff("a b c\n", "a x c\n", Options::default());
        let coloured = render(&diff, "old", "new", Paint { on: true }, true);
        assert!(!coloured.contains("[-"), "{coloured}");
        assert!(coloured.contains("\u{1b}[41m"), "{coloured:?}");
    }

    #[test]
    fn a_missing_final_newline_is_said_as_diff_says_it() {
        let diff = d::diff("a\n", "b", Options::default());
        let out = render(&diff, "old", "new", Paint { on: false }, true);
        assert!(out.ends_with("+b\n\\ No newline at end of file\n"), "{out}");
    }
}
