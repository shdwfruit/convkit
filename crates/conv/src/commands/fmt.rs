//! `conv fmt`: JSON, YAML and TOML pretty-printed, packed, sorted,
//! checked, or turned into one another, from a file, a pipe or the
//! clipboard, to the terminal, a file or the clipboard.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anstyle::{AnsiColor, Style};
use convkit_core::data::{
    self, DataError, DataErrorKind, Indent, Lang, Options, Out, Role, Source,
};
use convkit_core::{ConvError, ErrorCode, Format, Outcome};
use serde_json::json;

use crate::cli::{Cli, FmtArgs};
use crate::clipboard::{self, Copied, Got};
use crate::{input, output, paste, render};

/// Piped text up to this size is held in memory; past it, the rest goes
/// to a temporary file, so a pipe of any size is formatted without holding
/// it.
const IN_MEMORY: usize = 64 * 1024 * 1024;

/// A printed result up to this size is held until it is all valid, so an
/// error never follows half a document; a bigger one streams.
const HELD: usize = 16 * 1024 * 1024;

pub fn run(cli: &Cli, args: &FmtArgs) -> i32 {
    match fmt(cli, args) {
        Ok(code) => code,
        Err(Failure::Usage(e)) => {
            render::print_error(cli.json, &e);
            e.code.exit_code()
        }
        Err(Failure::Data {
            name,
            from,
            to,
            guessed,
            e,
        }) => {
            print_data_error(cli, &name, from, to, guessed, &e);
            if e.kind == DataErrorKind::Syntax {
                ErrorCode::InvalidData.exit_code()
            } else {
                ErrorCode::ConversionFailed.exit_code()
            }
        }
    }
}

enum Failure {
    Usage(ConvError),
    Data {
        name: String,
        from: Lang,
        to: Lang,
        /// The language was told from how the text looks.
        guessed: bool,
        e: Box<DataError>,
    },
}

impl From<ConvError> for Failure {
    fn from(e: ConvError) -> Failure {
        Failure::Usage(e)
    }
}

fn usage(message: impl Into<String>) -> Failure {
    Failure::Usage(ConvError::new(ErrorCode::InvalidInvocation, message))
}

/// Where the text came from.
enum Input {
    File(PathBuf),
    Text(Vec<u8>),
    /// Piped text too big to hold, in a temporary file.
    Spooled(tempfile::NamedTempFile),
}

impl Input {
    fn source(&self) -> Source<'_> {
        match self {
            Input::File(p) => Source::File(p),
            Input::Text(t) => Source::Text(t),
            Input::Spooled(f) => Source::File(f.path()),
        }
    }

    fn path(&self) -> Option<&Path> {
        match self {
            Input::File(p) => Some(p),
            _ => None,
        }
    }

    /// The first few KB, to tell its language by.
    fn head(&self) -> Vec<u8> {
        let mut head = Vec::new();
        match self {
            Input::Text(t) => head.extend_from_slice(&t[..t.len().min(4096)]),
            Input::File(p) => read_head(p, &mut head),
            Input::Spooled(f) => read_head(f.path(), &mut head),
        }
        head
    }
}

fn read_head(p: &Path, head: &mut Vec<u8>) {
    if let Ok(f) = std::fs::File::open(p) {
        let _ = f.take(4096).read_to_end(head);
    }
}

fn fmt(cli: &Cli, args: &FmtArgs) -> Result<i32, Failure> {
    // With --paste, the one path given is OUT.
    let (input_arg, output_arg) = if args.paste {
        if args.output.is_some() {
            return Err(usage(
                "--paste takes the text from the clipboard; give at most the file to write",
            ));
        }
        (None, args.input.as_deref())
    } else {
        (args.input.as_deref(), args.output.as_deref())
    };
    let (input, name, pasted) = read_input(args.paste, input_arg)?;
    let in_format = input.path().and_then(Format::from_path);
    let ext = input
        .path()
        .and_then(|p| p.extension())
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    let declared = matches!(ext.as_deref(), Some("jsonc" | "json5"));

    let from = match &args.from {
        Some(l) => Lang::parse(l).expect("clap checks the language"),
        None => match in_format {
            Some(f) => Lang::of(f).ok_or_else(|| {
                usage(format!(
                    "conv fmt reads JSON, YAML, TOML and CSV, and {name} is {}; to convert it, use conv {name} OUT",
                    f.ext()
                ))
            })?,
            None => data::sniff(&input.head()),
        },
    };
    let sniffed = args.from.is_none() && in_format.is_none();

    // Where it goes: OUT as typed, a bare `.ext`, or into -o's directory.
    let out_path = out_path(
        output_arg,
        args.outdir.as_deref(),
        &input,
        pasted,
        from,
        args,
    )?;
    let out_lang = out_path
        .as_deref()
        .and_then(Format::from_path)
        .and_then(Lang::of);
    let to = match (&args.to, out_lang) {
        (Some(t), Some(o)) => {
            let t = Lang::parse(t).expect("clap checks the language");
            if t != o {
                return Err(usage(format!(
                    "--to {} and the output's .{} name two languages; give one",
                    t.format().ext(),
                    o.format().ext()
                )));
            }
            t
        }
        (Some(t), None) => Lang::parse(t).expect("clap checks the language"),
        (None, Some(o)) => o,
        (None, None) => from,
    };
    if let Some(p) = &out_path {
        if out_lang.is_none() {
            return Err(usage(format!(
                "{} isn't a JSON, YAML, TOML or CSV file name",
                p.display()
            )));
        }
    }
    if from == Lang::Csv && to == Lang::Csv && !args.check {
        return Err(usage(
            "conv doesn't reformat a CSV; --to json or --to yaml writes it as JSON or YAML",
        ));
    }

    let opts = Options {
        lenient: args.lenient || declared,
        indent: match (args.tab, args.indent) {
            (true, _) => Some(Indent::Tab),
            (false, Some(n)) => Some(Indent::Spaces(n)),
            (false, None) => None,
        },
        minify: args.minify,
        sort_keys: args.sort_keys,
        name: name.clone(),
    };
    if !args.check {
        if let Some(why) = data::option_problem(to, &opts) {
            return Err(usage(why));
        }
    }
    let failed = |e: DataError| Failure::Data {
        name: name.clone(),
        from,
        to,
        guessed: sniffed,
        e: Box::new(e),
    };

    let start = Instant::now();
    if args.check {
        let report = data::check(&input.source(), from, &opts).map_err(failed)?;
        if cli.json {
            let v = json!({
                "ok": true,
                "results": [{
                    "ok": true,
                    "input": name,
                    "language": from,
                    "valid": true,
                    "comments": report.comments,
                    "notes": report.notes,
                }],
            });
            println!("{}", serde_json::to_string_pretty(&v).unwrap());
        } else if !cli.quiet {
            let glyph = if render::stdout_styled() {
                "\u{2713}"
            } else {
                "OK"
            };
            println!("{glyph} {name} is valid {}", from.name());
            eprint!(
                "{}",
                render::notes_human(&report.notes, render::stderr_styled())
            );
        }
        return Ok(0);
    }

    match out_path {
        Some(path) => {
            let report = write_file(&input, from, to, &opts, &path, args.overwrite, &name)
                .map_err(|w| match w {
                    Written::Data(e) => failed(e),
                    Written::Usage(e) => Failure::Usage(e),
                })?;
            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let outcome = Outcome {
                output: path.clone(),
                bytes,
                warnings: report.notes.clone(),
                notes: vec![],
                backend_output: vec![],
                backends: vec![],
                remuxed: false,
                elapsed_ms: start.elapsed().as_millis() as u64,
                sizing: None,
                enlarged: None,
                range: None,
                speed: None,
                rotation: None,
                pages: None,
                crop: None,
                audio: None,
                subtitles: None,
                denoise: None,
            };
            let copied = args.copy.then(|| {
                std::fs::read_to_string(&path)
                    .map_err(|e| format!("couldn't read {}: {e}", path.display()))
                    .and_then(|text| copy(&text, to))
            });
            if cli.json {
                let mut v = render::outcome_json(&outcome);
                v["input"] = json!(name);
                v["language"] = json!(from);
                v["output_language"] = json!(to);
                v["comments"] = json!(report.comments);
                let mut envelope = json!({ "ok": true, "results": [v] });
                if let Some(c) = &copied {
                    envelope["clipboard"] = copy_json(c);
                }
                println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
            } else {
                if !cli.quiet {
                    print!(
                        "{}",
                        render::conversion_success_human(&outcome, render::stdout_styled())
                    );
                }
                if let Some(c) = &copied {
                    print_copy(cli, c);
                }
            }
            Ok(0)
        }
        None if cli.json || args.copy => {
            let mut out = data::Plain(Vec::new());
            let report =
                data::transform(&input.source(), from, to, &opts, &mut out).map_err(failed)?;
            let text = String::from_utf8(out.0).expect("the writers write UTF-8");
            let copied = args.copy.then(|| copy(&text, to));
            if cli.json {
                let mut result = json!({
                    "ok": true,
                    "input": name,
                    "language": from,
                    "output": null,
                    "output_language": to,
                    "bytes": text.len(),
                    "comments": report.comments,
                    "notes": report.notes,
                });
                if !args.copy {
                    result["text"] = json!(text);
                }
                let mut envelope = json!({ "ok": true, "results": [result] });
                if let Some(c) = &copied {
                    envelope["clipboard"] = copy_json(c);
                }
                println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
            } else {
                if !cli.quiet {
                    eprint!(
                        "{}",
                        render::notes_human(&report.notes, render::stderr_styled())
                    );
                }
                if let Some(c) = &copied {
                    print_copy(cli, c);
                }
            }
            Ok(0)
        }
        None => {
            let stdout = std::io::stdout();
            let styled = render::stdout_styled();
            let mut out = Terminal {
                w: std::io::BufWriter::with_capacity(1 << 16, stdout.lock()),
                styled,
                broken: false,
                held: Some(Vec::new()),
            };
            let result = data::transform(&input.source(), from, to, &opts, &mut out);
            if result.is_ok() {
                out.release();
            }
            let _ = out.w.flush();
            // `conv fmt big.json | head` closes the pipe early; that is the
            // reader's choice, not a failure.
            if out.broken {
                return Ok(0);
            }
            let report = result.map_err(failed)?;
            if !cli.quiet {
                eprint!(
                    "{}",
                    render::notes_human(&report.notes, render::stderr_styled())
                );
            }
            Ok(0)
        }
    }
}

/// The text to format: the clipboard's, a file, or standard input, and
/// how messages name it. `pasted` is true for the clipboard's text.
fn read_input(paste: bool, path: Option<&Path>) -> Result<(Input, String, bool), Failure> {
    if paste {
        return match paste::get()? {
            Got::Text(t) if t.trim().is_empty() => Err(usage("the clipboard is empty")),
            Got::Text(t) => Ok((
                Input::Text(t.into_bytes()),
                "the pasted text".to_string(),
                true,
            )),
            Got::Files(files) => match files.as_slice() {
                [one] => {
                    let name = file_name(one);
                    Ok((Input::File(one.clone()), name, false))
                }
                many => Err(usage(format!(
                    "the clipboard holds {} files; conv fmt takes one",
                    many.len()
                ))),
            },
            Got::Picture { .. } => Err(usage("the clipboard holds a picture, not text")),
        };
    }
    match path {
        Some(p) if p.as_os_str() == "-" => stdin_input(),
        Some(p) if p.is_dir() => Err(usage(format!(
            "{} is a directory; conv fmt formats one file",
            p.display()
        ))),
        Some(p) if !p.is_file() => Err(Failure::Usage(ConvError::new(
            ErrorCode::InputNotFound,
            format!("input not found: {}", p.display()),
        ))),
        Some(p) => Ok((Input::File(p.to_path_buf()), file_name(p), false)),
        None if !std::io::stdin().is_terminal() => stdin_input(),
        None => Err(Failure::Usage(ConvError {
            code: ErrorCode::InvalidInvocation,
            message: "conv fmt needs a file, text piped in, or --paste".to_string(),
            backend: None,
            remediation: Some(convkit_core::Remediation {
                managed: None,
                manual: Some("e.g. `conv fmt data.json`, or `pbpaste | conv fmt`".to_string()),
            }),
        })),
    }
}

fn stdin_input() -> Result<(Input, String, bool), Failure> {
    let name = "standard input".to_string();
    let mut stdin = std::io::stdin().lock();
    let mut held = Vec::new();
    let read = |e: std::io::Error| usage(format!("couldn't read standard input: {e}"));
    (&mut stdin)
        .take(IN_MEMORY as u64)
        .read_to_end(&mut held)
        .map_err(read)?;
    if held.len() < IN_MEMORY {
        return Ok((Input::Text(held), name, false));
    }
    let mut spool = tempfile::NamedTempFile::new().map_err(read)?;
    spool.write_all(&held).map_err(read)?;
    std::io::copy(&mut stdin, &mut spool).map_err(read)?;
    spool.flush().map_err(read)?;
    Ok((Input::Spooled(spool), name, false))
}

fn file_name(p: &Path) -> String {
    p.file_name().map_or_else(
        || p.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// The file to write, if any: OUT as typed, a bare `.ext` for the input's
/// name with that extension, or the input's name in `-o`'s directory.
fn out_path(
    output: Option<&Path>,
    outdir: Option<&Path>,
    input: &Input,
    pasted: bool,
    from: Lang,
    args: &FmtArgs,
) -> Result<Option<PathBuf>, Failure> {
    // The name a written result takes when only its extension is given.
    let base = || -> Result<PathBuf, Failure> {
        match input.path() {
            Some(p) => Ok(PathBuf::from(p.file_name().unwrap_or_default())),
            None if pasted => Ok(PathBuf::from("clipboard")),
            None => Err(usage(
                "text from a pipe has no name; give the file to write, like conv fmt - out.json",
            )),
        }
    };
    let ext_of = |lang: Lang| lang.format().ext();
    let named = match output {
        None if outdir.is_none() => return Ok(None),
        None => {
            let lang = args.to.as_deref().and_then(Lang::parse).unwrap_or(from);
            base()?.with_extension(ext_of(lang))
        }
        Some(p) if p.is_dir() => {
            return Err(usage(format!(
                "{} is a directory; to write into it, pass -o {}",
                p.display(),
                p.display()
            )))
        }
        Some(p) if input::is_extension_shorthand(p) => {
            let ext = p.to_string_lossy();
            let ext = ext.trim_start_matches('.');
            if Format::is_read_only_ext(ext) {
                return Err(Failure::Usage(read_only(ext)));
            }
            match input.path() {
                // Beside the input, as `conv in.json .yaml` writes.
                Some(p) if outdir.is_none() => p.with_extension(ext),
                _ => base()?.with_extension(ext),
            }
        }
        Some(p) => {
            if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                if Format::is_read_only_ext(ext) {
                    return Err(Failure::Usage(read_only(ext)));
                }
            }
            p.to_path_buf()
        }
    };
    let path = input::in_outdir(&named, outdir)?;
    output::ensure_dir(outdir)?;
    Ok(Some(path))
}

/// `.jsonc` and the like, which conv reads but never writes.
fn read_only(ext: &str) -> ConvError {
    let canonical = Format::from_ext(ext).map_or("json", |f| f.ext());
    ConvError::read_only_format(ext, canonical)
}

/// What can stop a write: the text, or where it would go.
enum Written {
    Data(DataError),
    Usage(ConvError),
}

/// Writes the result beside `path` and renames it into place, so a failed
/// run never leaves half a file, and checks before it does that writing
/// over the input would lose none of its comments.
fn write_file(
    input: &Input,
    from: Lang,
    to: Lang,
    opts: &Options,
    path: &Path,
    overwrite: bool,
    name: &str,
) -> Result<data::Report, Written> {
    if path.exists() && !overwrite {
        return Err(Written::Usage(ConvError::new(
            ErrorCode::OutputExists,
            format!("{} exists; pass -y to overwrite", path.display()),
        )));
    }
    let same = input.path().is_some_and(|p| same_file(p, path));
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = path.with_file_name(format!(".{file_name}.conv-{}", std::process::id()));
    let io = |e: std::io::Error| {
        Written::Usage(ConvError::new(
            ErrorCode::ConversionFailed,
            format!("cannot write {}: {e}", path.display()),
        ))
    };
    let file = std::fs::File::create(&temp).map_err(io)?;
    let mut out = data::Plain(std::io::BufWriter::with_capacity(1 << 16, file));
    let result = data::transform(&input.source(), from, to, opts, &mut out);
    let flushed = out.0.flush();
    drop(out);
    let report = match (result, flushed) {
        (Ok(r), Ok(())) => r,
        (Err(e), _) => {
            let _ = std::fs::remove_file(&temp);
            return Err(Written::Data(e));
        }
        (Ok(_), Err(e)) => {
            let _ = std::fs::remove_file(&temp);
            return Err(io(e));
        }
    };
    if same && report.comments > 0 {
        let _ = std::fs::remove_file(&temp);
        let n = report.comments;
        let comments = if n == 1 {
            "a comment".to_string()
        } else {
            format!("{n} comments")
        };
        return Err(Written::Usage(ConvError {
            code: ErrorCode::InvalidInvocation,
            message: format!(
                "{name} has {comments}, which conv's output can't keep, so it won't write over {name}"
            ),
            backend: None,
            remediation: Some(convkit_core::Remediation {
                managed: None,
                manual: Some(format!(
                    "write it to another file, or print it: conv fmt {name}"
                )),
            }),
        }));
    }
    std::fs::rename(&temp, path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        io(e)
    })?;
    Ok(report)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// How the text went onto the clipboard, or why it didn't.
type CopyResult = Result<String, String>;

fn copy(text: &str, lang: Lang) -> CopyResult {
    let chars = text.chars().count();
    clipboard::copy(text).map(|how| {
        let mut s = format!(
            "copied {} characters of {} to the clipboard",
            group(chars),
            lang.name()
        );
        if how == Copied::Osc52 {
            s.push_str("; asked the terminal to, which some terminals don't allow");
        }
        s
    })
}

fn copy_json(c: &CopyResult) -> serde_json::Value {
    match c {
        Ok(m) => json!({ "ok": true, "message": m }),
        Err(m) => json!({ "ok": false, "message": format!("couldn't copy it: {m}") }),
    }
}

fn print_copy(cli: &Cli, c: &CopyResult) {
    match c {
        Ok(m) if !cli.quiet => eprintln!("{m}"),
        Ok(_) => {}
        Err(m) => {
            let line = format!("warning  couldn't copy it: {m}");
            eprintln!("{}", render::paint_warning(&line, render::stderr_styled()));
        }
    }
}

/// `12,345`.
fn group(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A data error, as `conv fmt` shows it: what was being read or written,
/// where, the line with a caret under the spot, and the fix.
fn print_data_error(cli: &Cli, name: &str, from: Lang, to: Lang, guessed: bool, e: &DataError) {
    let header = match e.kind {
        DataErrorKind::Syntax => format!("{name} is not valid {}", from.name()),
        _ if from == to => name.to_string(),
        _ => format!("{name} -> {}", to.name()),
    };
    if cli.json {
        let mut error = json!({
            "code": if e.kind == DataErrorKind::Syntax { "invalid_data" } else { "conversion_failed" },
            "kind": e.kind,
            "message": e.headline(),
        });
        if let Some(l) = e.line {
            error["line"] = json!(l);
        }
        if let Some(c) = e.column {
            error["column"] = json!(c);
        }
        if let Some(h) = &e.hint {
            error["hint"] = json!(h);
        }
        if let Some(s) = &e.snippet {
            error["snippet"] = json!(s);
        }
        let v = json!({
            "ok": false,
            "results": [{
                "ok": false,
                "input": name,
                "language": from,
                "language_guessed": guessed,
                "error": error,
            }],
        });
        // On stdout, as a conversion's results are, failures too.
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
        return;
    }
    let styled = render::stderr_styled();
    let mut lines = vec![e.headline()];
    lines.extend(e.snippet_lines());
    // JSON is told by its first character; YAML is what text that is
    // neither JSON nor TOML is taken for, which can be wrong.
    if guessed && e.kind == DataErrorKind::Syntax && from != Lang::Json {
        lines.push(format!(
            "conv took the text for {} by how it looks; --from says what it is",
            from.name()
        ));
    }
    let err = ConvError {
        code: ErrorCode::InvalidData,
        message: lines.join("\n  "),
        backend: None,
        remediation: e.hint.as_ref().map(|h| convkit_core::Remediation {
            managed: None,
            manual: Some(h.clone()),
        }),
    };
    eprint!("{}", render::failure_human(&header, &err, styled));
}

/// Standard output, in colour on a terminal, noting a closed pipe. What
/// is written is held until `release`, up to `HELD`.
struct Terminal<W: Write> {
    w: W,
    styled: bool,
    broken: bool,
    held: Option<Vec<u8>>,
}

impl<W: Write> Terminal<W> {
    /// Writes what is held, and everything after it straight through.
    fn release(&mut self) {
        if let Some(held) = self.held.take() {
            if let Err(e) = self.w.write_all(&held) {
                self.broken |= e.kind() == std::io::ErrorKind::BrokenPipe;
            }
        }
    }
}

impl<W: Write> Out for Terminal<W> {
    fn put(&mut self, role: Role, text: &str) -> std::io::Result<()> {
        let style = if self.styled { style_of(role) } else { None };
        if let Some(held) = &mut self.held {
            match style {
                Some(s) => write!(held, "{}{text}{}", s.render(), s.render_reset())?,
                None => held.extend_from_slice(text.as_bytes()),
            }
            if held.len() > HELD {
                self.release();
            }
            return Ok(());
        }
        let result = match style {
            Some(s) => write!(self.w, "{}{text}{}", s.render(), s.render_reset()),
            None => self.w.write_all(text.as_bytes()),
        };
        if let Err(e) = &result {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                self.broken = true;
            }
        }
        result
    }
}

/// The colours, as jq's: keys blue, strings green, null dim, the rest as
/// the terminal writes text.
fn style_of(role: Role) -> Option<Style> {
    match role {
        Role::Key => Some(AnsiColor::Blue.on_default().bold()),
        Role::Header => Some(AnsiColor::Cyan.on_default().bold()),
        Role::Str => Some(AnsiColor::Green.on_default()),
        Role::Num | Role::Date => Some(AnsiColor::Yellow.on_default()),
        Role::Bool => Some(AnsiColor::Magenta.on_default()),
        Role::Null => Some(Style::new().dimmed()),
        Role::Space | Role::Punct => None,
    }
}
