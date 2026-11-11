//! The text and developer tools (`conv base64`, `hash`, `uuid`, `jwt`,
//! `time`, `link`, `count`, `case`, `chars`, `lorem`, `diff`), and what
//! they share: where their input comes from, and how a result is printed,
//! copied and reported.
//!
//! Input is never guessed. A tool's positional is text (for `hash` and
//! `diff`, files); `-f FILE` reads a file instead (`--text` for `hash` and
//! `diff`); `-` or a pipe reads stdin; `--paste` takes the clipboard. Text
//! that names a file here is still text, with a note saying how to read
//! the file. Results go to stdout, `--copy` also puts them on the
//! clipboard, `--json` prints one JSON document, and colour is only for a
//! terminal.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anstyle::{AnsiColor, Style};
use clap::Subcommand;
use convkit_core::{ConvError, ErrorCode};
use serde_json::{json, Value};

use crate::cli::Cli;
use crate::clipboard::{self, Got};
use crate::commands::convert::{self, CopyReport};
use crate::render;

pub mod base64;
pub mod case;
pub mod chars;
pub mod count;
pub mod diff;
pub mod hash;
pub mod jwt;
pub mod link;
pub mod lorem;
pub mod time;
pub mod uuid;

/// The tools, listed in `conv --help` after the file commands.
#[derive(Subcommand, Debug, Clone)]
pub enum Tool {
    /// Encode or decode base64, and data: URIs.
    #[command(display_order = 100, long_about = base64::ABOUT)]
    Base64(base64::Args),
    /// Checksums (SHA-256 and others), and checking a file against one.
    #[command(display_order = 101, long_about = hash::ABOUT)]
    Hash(hash::Args),
    /// Make UUIDs, or say what one is.
    #[command(display_order = 102, long_about = uuid::ABOUT)]
    Uuid(uuid::Args),
    /// Read a JSON Web Token, and check its signature.
    #[command(display_order = 103, long_about = jwt::ABOUT)]
    Jwt(jwt::Args),
    /// Unix timestamps to dates and back.
    #[command(display_order = 104, long_about = time::ABOUT, allow_negative_numbers = true)]
    Time(time::Args),
    /// Take the tracking out of links, and the redirect wrappers off them.
    #[command(display_order = 105, long_about = link::ABOUT)]
    Link(link::Args),
    /// Count words, characters, sentences and lines.
    #[command(display_order = 106, long_about = count::ABOUT)]
    Count(count::Args),
    /// Change case: upper, lower, title, sentence, camel, snake, ...
    #[command(display_order = 107, long_about = case::ABOUT)]
    Case(case::Args),
    /// List every character, and flag invisible and lookalike ones.
    #[command(display_order = 108, long_about = chars::ABOUT)]
    Chars(chars::Args),
    /// Placeholder text: lorem ipsum.
    #[command(display_order = 109, long_about = lorem::ABOUT)]
    Lorem(lorem::Args),
    /// Compare two files or texts, with the changed words marked.
    #[command(display_order = 110, long_about = diff::ABOUT)]
    Diff(diff::Args),
}

pub fn run(cli: &Cli, tool: &Tool) -> i32 {
    match tool {
        Tool::Base64(a) => base64::run(cli, a),
        Tool::Hash(a) => hash::run(cli, a),
        Tool::Uuid(a) => uuid::run(cli, a),
        Tool::Jwt(a) => jwt::run(cli, a),
        Tool::Time(a) => time::run(cli, a),
        Tool::Link(a) => link::run(cli, a),
        Tool::Count(a) => count::run(cli, a),
        Tool::Case(a) => case::run(cli, a),
        Tool::Chars(a) => chars::run(cli, a),
        Tool::Lorem(a) => lorem::run(cli, a),
        Tool::Diff(a) => diff::run(cli, a),
    }
}

/// The input flags of the tools that read text.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct Common {
    /// Read the input from FILE instead (- for what is piped in).
    #[arg(short = 'f', long, value_name = "FILE")]
    pub file: Option<PathBuf>,
    /// Take the input from the clipboard.
    #[arg(long)]
    pub paste: bool,
    /// Also copy the result to the clipboard.
    #[arg(long)]
    pub copy: bool,
}

/// Where the input came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Arg,
    Stdin,
    File(PathBuf),
    Pasted,
    /// A file copied to the clipboard.
    PastedFile(PathBuf),
    /// A copied picture, with no file of its own.
    PastedPicture,
}

impl Origin {
    /// The file the input was read from, if any.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Origin::File(p) | Origin::PastedFile(p) => Some(p),
            _ => None,
        }
    }
}

pub struct Input {
    pub bytes: Vec<u8>,
    pub origin: Origin,
}

impl Input {
    /// The input as text: UTF-8, or an error naming where it came from.
    pub fn text(self) -> Result<String, ConvError> {
        let origin = self.origin.clone();
        String::from_utf8(self.bytes).map_err(|e| {
            let what = match &origin {
                Origin::File(p) | Origin::PastedFile(p) => p.display().to_string(),
                Origin::Stdin => "what was piped in".to_string(),
                Origin::PastedPicture => "the pasted picture".to_string(),
                _ => "the input".to_string(),
            };
            failed(format!(
                "{what} isn't UTF-8 text (byte {} isn't), so it can't be read as text",
                e.utf8_error().valid_up_to() + 1
            ))
        })
    }
}

/// Whether a tool reads stdin when nothing else is given and it is piped.
/// `time` and `uuid` have something to do with no input, so they read it
/// only for `-`, and a pipe left open never makes them wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piped {
    Read,
    OnlyWithDash,
}

pub fn usage(message: impl Into<String>) -> ConvError {
    ConvError::new(ErrorCode::InvalidInvocation, message)
}

/// The input isn't what the tool reads: invalid base64, a malformed
/// token, a date it can't read. Exits 1, like a conversion that failed.
pub fn failed(message: impl Into<String>) -> ConvError {
    ConvError::new(ErrorCode::ConversionFailed, message)
}

/// Reads a tool's input from the one place it was given. `None` when
/// nothing was given and nothing is piped in.
pub fn read(
    cli: &Cli,
    arg: Option<&str>,
    common: &Common,
    piped: Piped,
    command: &str,
) -> Result<Option<Input>, ConvError> {
    let given = [arg.is_some(), common.file.is_some(), common.paste]
        .iter()
        .filter(|g| **g)
        .count();
    if given > 1 {
        return Err(usage(format!(
            "give conv {command} its input one way: as an argument, with -f FILE, or with --paste"
        )));
    }
    if common.paste {
        return pasted(cli, command).map(Some);
    }
    match (arg, common.file.as_deref()) {
        (Some("-"), _) => stdin().map(Some),
        (Some(text), _) => {
            note_if_file(cli, text, &format!("-f {}", shell_word(text)));
            Ok(Some(Input {
                bytes: text.as_bytes().to_vec(),
                origin: Origin::Arg,
            }))
        }
        (None, Some(p)) if p.as_os_str() == "-" => stdin().map(Some),
        (None, Some(p)) => read_file(p).map(|bytes| {
            Some(Input {
                bytes,
                origin: Origin::File(p.to_path_buf()),
            })
        }),
        (None, None) if piped == Piped::Read && !std::io::stdin().is_terminal() => {
            stdin().map(Some)
        }
        (None, None) => Ok(None),
    }
}

/// The error for a tool given nothing to work on.
pub fn nothing(command: &str, what: &str) -> ConvError {
    usage(format!(
        "conv {command} needs {what}: give it as an argument, with -f FILE or --paste, or pipe it in"
    ))
}

pub fn stdin() -> Result<Input, ConvError> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .map_err(|e| failed(format!("couldn't read what was piped in: {e}")))?;
    Ok(Input {
        bytes,
        origin: Origin::Stdin,
    })
}

pub fn read_file(p: &Path) -> Result<Vec<u8>, ConvError> {
    if p.is_dir() {
        return Err(usage(format!("{} is a folder, not a file", p.display())));
    }
    std::fs::read(p).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ConvError::new(
            ErrorCode::InputNotFound,
            format!("no such file: {}", p.display()),
        ),
        _ => failed(format!("couldn't read {}: {e}", p.display())),
    })
}

/// The clipboard as input: copied text, the one file copied, or a copied
/// picture's bytes.
pub fn pasted(cli: &Cli, command: &str) -> Result<Input, ConvError> {
    let got = clipboard::get().map_err(|e| usage(format!("couldn't paste: {e}")))?;
    match got {
        Got::Text(text) => Ok(Input {
            bytes: text.into_bytes(),
            origin: Origin::Pasted,
        }),
        Got::Files(files) => match files.as_slice() {
            [one] => {
                note(
                    cli,
                    &format!("read {}, the file on the clipboard", one.display()),
                );
                Ok(Input {
                    bytes: read_file(one)?,
                    origin: Origin::PastedFile(one.clone()),
                })
            }
            many => Err(usage(format!(
                "the clipboard holds {} files; conv {command} takes one",
                many.len()
            ))),
        },
        Got::Picture { bytes, .. } => Ok(Input {
            bytes,
            origin: Origin::PastedPicture,
        }),
    }
}

/// Says, on stderr, when text given as an argument is also the name of a
/// file here: it is still the text, and `how` reads the file.
pub fn note_if_file(cli: &Cli, text: &str, how: &str) {
    let plausible = !text.is_empty() && text.len() < 4096 && !text.contains('\n');
    if plausible && Path::new(text).is_file() {
        note(
            cli,
            &format!("{text:?} is used as the text; to read the file of that name, use {how}"),
        );
    }
}

/// A word as it would be typed in a shell, quoted if it needs to be.
pub fn shell_word(s: &str) -> String {
    render::shell_quote(s)
}

/// A dim `note  ...` line on stderr, unless `--quiet`.
pub fn note(cli: &Cli, message: &str) {
    if !cli.quiet {
        eprint!(
            "{}",
            render::notes_human(&[message.to_string()], render::stderr_styled())
        );
    }
}

/// A yellow `warning  ...` line on stderr, even under `--quiet`.
pub fn warn(message: &str) {
    let line = format!("warning  {message}");
    eprintln!("{}", render::paint_warning(&line, render::stderr_styled()));
}

/// A finished tool's result: what to print, what `--copy` copies, and the
/// `--json` document (without `ok`, which is added here).
pub struct Done {
    pub human: String,
    pub copy: Option<String>,
    /// A file the tool wrote, which `--copy` copies as a file (a picture
    /// as itself) rather than as text.
    pub copy_file: Option<PathBuf>,
    pub json: Value,
    pub code: i32,
}

impl Done {
    pub fn new(human: String, copy: Option<String>, json: Value) -> Done {
        Done {
            human,
            copy,
            copy_file: None,
            json,
            code: 0,
        }
    }

    pub fn code(mut self, code: i32) -> Done {
        self.code = code;
        self
    }
}

/// Prints `done` as text or JSON, and copies it when asked.
pub fn finish(cli: &Cli, copy: bool, done: Done) -> i32 {
    let copied = match (copy, &done.copy_file, &done.copy) {
        (false, _, _) => None,
        (true, Some(file), _) => Some(convert::copy_outputs(std::slice::from_ref(file))),
        (true, None, Some(text)) => Some(copy_text(text)),
        (true, None, None) => None,
    };
    if cli.json {
        let mut v = json!({ "ok": true });
        if let (Value::Object(out), Value::Object(body)) = (&mut v, done.json) {
            out.extend(body);
        }
        if let Some(c) = &copied {
            v["clipboard"] = c.json();
        }
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(done.human.as_bytes());
        if !done.human.is_empty() && !done.human.ends_with('\n') {
            let _ = out.write_all(b"\n");
        }
        let _ = out.flush();
        drop(out);
        if let Some(c) = &copied {
            c.print(cli);
        }
    }
    done.code
}

/// Prints `e` as conv prints any failure, and gives its exit code.
pub fn fail(cli: &Cli, e: &ConvError) -> i32 {
    render::print_error(cli.json, e);
    e.code.exit_code()
}

/// `--copy` for text.
pub fn copy_text(text: &str) -> CopyReport {
    convert::copy_text(text)
}

/// Colours for a terminal, nothing otherwise.
#[derive(Debug, Clone, Copy)]
pub struct Paint {
    pub on: bool,
}

impl Paint {
    pub fn stdout() -> Paint {
        Paint {
            on: render::stdout_styled(),
        }
    }

    pub fn stderr() -> Paint {
        Paint {
            on: render::stderr_styled(),
        }
    }

    pub fn style(self, text: &str, style: Style) -> String {
        if self.on && !text.is_empty() {
            format!("{}{text}{}", style.render(), style.render_reset())
        } else {
            text.to_string()
        }
    }

    pub fn dim(self, text: &str) -> String {
        self.style(text, Style::new().dimmed())
    }

    pub fn bold(self, text: &str) -> String {
        self.style(text, Style::new().bold())
    }

    pub fn red(self, text: &str) -> String {
        self.style(text, AnsiColor::Red.on_default())
    }

    pub fn green(self, text: &str) -> String {
        self.style(text, AnsiColor::Green.on_default())
    }

    pub fn yellow(self, text: &str) -> String {
        self.style(text, AnsiColor::Yellow.on_default())
    }

    pub fn cyan(self, text: &str) -> String {
        self.style(text, AnsiColor::Cyan.on_default())
    }

    /// `✓` on a terminal, `OK` otherwise.
    pub fn ok(self) -> String {
        if self.on {
            self.style("\u{2713}", AnsiColor::Green.on_default().bold())
        } else {
            "OK".to_string()
        }
    }

    /// `✗` on a terminal, `FAIL` otherwise.
    pub fn fail(self) -> String {
        if self.on {
            self.style("\u{2717}", AnsiColor::Red.on_default().bold())
        } else {
            "FAIL".to_string()
        }
    }
}

/// Rows of `label  value`, the labels padded to one width.
pub fn rows(rows: &[(String, String)]) -> String {
    let width = rows
        .iter()
        .map(|(l, _)| unicode_width::UnicodeWidthStr::width(l.as_str()))
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (label, value) in rows {
        let pad = width - unicode_width::UnicodeWidthStr::width(label.as_str());
        out.push_str(label);
        out.push_str(&" ".repeat(pad + 2));
        out.push_str(value);
        out.push('\n');
    }
    out
}

/// `OK NAME - SIZE` and the full path below it, for a file a tool wrote.
pub fn written_human(path: &Path, bytes: u64, paint: Paint) -> String {
    let name = render::safe_for_terminal(&render::file_name_of(path));
    let sep = if paint.on { " \u{b7} " } else { " - " };
    let full = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    format!(
        "{} {name}{sep}{}\n  {}\n",
        paint.ok(),
        convkit_core::size::human(bytes),
        render::safe_for_terminal(&full.display().to_string())
    )
}

/// Whether a path was given as `-`.
pub fn is_dash(p: &Path) -> bool {
    p.as_os_str() == "-"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_line_up_wide_labels() {
        let out = rows(&[
            ("a".to_string(), "1".to_string()),
            ("東京".to_string(), "2".to_string()),
        ]);
        assert_eq!(out, "a     1\n東京  2\n");
    }

    #[test]
    fn plain_paint_adds_nothing() {
        let p = Paint { on: false };
        assert_eq!(p.red("x"), "x");
        assert_eq!(p.ok(), "OK");
        assert_eq!(p.fail(), "FAIL");
        let on = Paint { on: true };
        assert!(on.red("x").contains("\u{1b}["));
        // Nothing to colour, no escape codes.
        assert_eq!(on.red(""), "");
    }

    #[test]
    fn a_written_file_reports_its_name_size_and_path() {
        let out = written_human(Path::new("decoded.png"), 2048, Paint { on: false });
        let mut lines = out.lines();
        assert_eq!(lines.next(), Some("OK decoded.png - 2 KB"));
        assert!(lines.next().unwrap().ends_with("decoded.png"));
    }

    #[test]
    fn text_that_isnt_utf8_names_where_it_came_from() {
        let input = Input {
            bytes: b"ok\xff".to_vec(),
            origin: Origin::File(PathBuf::from("blob.bin")),
        };
        let e = input.text().unwrap_err();
        assert_eq!(e.code, ErrorCode::ConversionFailed);
        assert!(
            e.message.contains("blob.bin isn't UTF-8 text (byte 3"),
            "{}",
            e.message
        );
    }
}
