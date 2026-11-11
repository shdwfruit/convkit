//! `conv hash`: checksums of files, and checking them.

use std::path::{Path, PathBuf};

use convkit_core::tools::hash::{self as h, Algorithm, Expected, Hashed};
use convkit_core::{ConvError, ErrorCode};
use serde_json::{json, Map, Value};

use super::{Done, Paint};
use crate::cli::Cli;
use crate::clipboard::{self, Got};

pub const ABOUT: &str = "\
Prints the SHA-256 of each file, as sha256sum does, so the output can be \
saved as a SHA256SUMS file. --md5, --sha1, --sha384 and --sha512 pick \
others, and --all prints all five.

--check HASH compares each file with a hash, in either case; the length \
says which algorithm it is (64 hex digits is SHA-256). A mismatch says so \
and exits 1. --sums SHA256SUMS checks files against a sums file, with the \
files named in it found next to it.

Without FILE, conv hashes what is piped in; - does too. --text hashes \
the arguments themselves.";

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// Files to hash; - for what is piped in. With --text, the texts.
    #[arg(value_name = "FILE")]
    pub files: Vec<PathBuf>,
    /// MD5. Fine for spotting a damaged download, not against tampering.
    #[arg(long)]
    pub md5: bool,
    /// SHA-1. Like MD5, no longer safe against tampering.
    #[arg(long)]
    pub sha1: bool,
    /// SHA-256 (the default).
    #[arg(long)]
    pub sha256: bool,
    /// SHA-384.
    #[arg(long)]
    pub sha384: bool,
    /// SHA-512.
    #[arg(long)]
    pub sha512: bool,
    /// All five.
    #[arg(long)]
    pub all: bool,
    /// Compare each file with this hash. Exits 1 when one doesn't match.
    #[arg(
        long,
        value_name = "HASH",
        conflicts_with_all = ["md5", "sha1", "sha256", "sha384", "sha512", "all", "sums"]
    )]
    pub check: Option<String>,
    /// Check files against a SHA256SUMS-style file: every file it lists
    /// that is next to it, or just FILE. Exits 1 when one doesn't match.
    #[arg(
        long,
        value_name = "SUMS",
        conflicts_with_all = ["md5", "sha1", "sha256", "sha384", "sha512", "all", "text"]
    )]
    pub sums: Option<PathBuf>,
    /// Hash the arguments as text, rather than files of those names.
    #[arg(long)]
    pub text: bool,
    /// Hash what is on the clipboard: copied files, text or a picture.
    #[arg(long, conflicts_with_all = ["files", "text", "sums"])]
    pub paste: bool,
    /// Also copy the hash to the clipboard.
    #[arg(long)]
    pub copy: bool,
}

/// One thing to hash.
enum Source {
    File(PathBuf),
    Stdin,
    Text(String),
    Bytes(&'static str, Vec<u8>),
}

impl Source {
    fn name(&self) -> String {
        match self {
            // For what is printed; a file's name is anyone's.
            Source::File(p) => crate::render::safe_for_terminal(&p.display().to_string()),
            Source::Stdin => "-".to_string(),
            Source::Text(t) => format!("{t:?}"),
            Source::Bytes(name, _) => name.to_string(),
        }
    }

    fn hash(&self, algorithms: &[Algorithm]) -> Result<Hashed, ConvError> {
        match self {
            Source::File(p) => {
                if p.is_dir() {
                    return Err(super::usage(format!(
                        "{} is a folder; conv hash takes files",
                        p.display()
                    )));
                }
                let f = std::fs::File::open(p).map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => ConvError::new(
                        ErrorCode::InputNotFound,
                        format!(
                            "no such file: {}; to hash the text itself, use --text",
                            p.display()
                        ),
                    ),
                    _ => super::failed(format!("couldn't read {}: {e}", p.display())),
                })?;
                h::hash_reader(f, algorithms)
                    .map_err(|e| super::failed(format!("couldn't read {}: {e}", p.display())))
            }
            Source::Stdin => h::hash_reader(std::io::stdin().lock(), algorithms)
                .map_err(|e| super::failed(format!("couldn't read what was piped in: {e}"))),
            Source::Text(t) => Ok(h::hash_bytes(t.as_bytes(), algorithms)),
            Source::Bytes(_, b) => Ok(h::hash_bytes(b, algorithms)),
        }
    }

    fn json(&self, hashed: &Hashed) -> Value {
        let mut m = Map::new();
        match self {
            Source::File(p) => {
                m.insert("name".into(), json!(p.display().to_string()));
                m.insert(
                    "path".into(),
                    json!(std::path::absolute(p).unwrap_or(p.clone())),
                );
            }
            Source::Stdin => {
                m.insert("name".into(), json!("-"));
            }
            Source::Text(t) => {
                m.insert("text".into(), json!(t));
            }
            Source::Bytes(name, _) => {
                m.insert("name".into(), json!(name));
            }
        }
        m.insert("bytes".into(), json!(hashed.bytes));
        for (a, hex) in &hashed.hashes {
            m.insert(a.key().into(), json!(hex));
        }
        Value::Object(m)
    }
}

pub fn run(cli: &Cli, args: &Args) -> i32 {
    let sources = match sources(cli, args) {
        Ok(s) => s,
        Err(e) => return super::fail(cli, &e),
    };
    if let Some(sums) = &args.sums {
        return check_sums(cli, args, sums);
    }
    if let Some(expected) = &args.check {
        return match parse_check(expected) {
            Ok(e) => check(cli, args, &sources, &e),
            Err(e) => super::fail(cli, &e),
        };
    }
    let algorithms = algorithms(args);
    let mut human = String::new();
    let mut files = Vec::new();
    let mut first_hash = None;
    let mut code = 0;
    for source in &sources {
        let hashed = match source.hash(&algorithms) {
            Ok(h) => h,
            Err(e) => {
                code = code.max(e.code.exit_code());
                render_error(cli, &e);
                continue;
            }
        };
        if first_hash.is_none() {
            first_hash = Some(hashed.hashes[0].1.clone());
        }
        human.push_str(&hashed_human(source, &hashed, algorithms.len() > 1));
        files.push(source.json(&hashed));
    }
    if files.is_empty() {
        return code;
    }
    // One hash copies as the hash alone, ready to paste into a form.
    let copy = if sources.len() == 1 && algorithms.len() == 1 {
        first_hash
    } else {
        Some(human.clone())
    };
    super::finish(
        cli,
        args.copy,
        Done::new(human, copy, json!({ "files": files })).code(code),
    )
}

fn render_error(cli: &Cli, e: &ConvError) {
    if cli.json {
        crate::render::print_error(true, e);
    } else {
        eprint!("{}", crate::render::error_human(e));
    }
}

fn algorithms(args: &Args) -> Vec<Algorithm> {
    if args.all {
        return Algorithm::ALL.to_vec();
    }
    let picked: Vec<Algorithm> = [
        (args.md5, Algorithm::Md5),
        (args.sha1, Algorithm::Sha1),
        (args.sha256, Algorithm::Sha256),
        (args.sha384, Algorithm::Sha384),
        (args.sha512, Algorithm::Sha512),
    ]
    .into_iter()
    .filter(|(on, _)| *on)
    .map(|(_, a)| a)
    .collect();
    if picked.is_empty() {
        vec![Algorithm::Sha256]
    } else {
        picked
    }
}

fn sources(cli: &Cli, args: &Args) -> Result<Vec<Source>, ConvError> {
    if args.paste {
        return Ok(
            match clipboard::get().map_err(|e| super::usage(format!("couldn't paste: {e}")))? {
                Got::Files(files) => files.into_iter().map(Source::File).collect(),
                Got::Text(t) => {
                    super::note(cli, "hashed the text on the clipboard, as it is");
                    vec![Source::Text(t)]
                }
                Got::Picture { bytes, .. } => vec![Source::Bytes("the pasted picture", bytes)],
            },
        );
    }
    if args.files.is_empty() {
        if args.sums.is_some() {
            return Ok(vec![]);
        }
        if args.text {
            return Err(super::usage(
                "--text hashes the texts given after it; give one",
            ));
        }
        if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            return Err(super::usage(
                "conv hash needs files: conv hash FILE..., or pipe something in (or --text TEXT, or --paste)",
            ));
        }
        return Ok(vec![Source::Stdin]);
    }
    Ok(args
        .files
        .iter()
        .map(|p| {
            if args.text {
                let t = p.to_string_lossy().into_owned();
                if Path::new(&t).is_file() {
                    super::note(
                        cli,
                        &format!("hashed the text {t:?}, not the file of that name; leave out --text to hash the file"),
                    );
                }
                Source::Text(t)
            } else if super::is_dash(p) {
                Source::Stdin
            } else {
                Source::File(p.clone())
            }
        })
        .collect())
}

fn hashed_human(source: &Source, hashed: &Hashed, several: bool) -> String {
    if !several {
        let hex = &hashed.hashes[0].1;
        return match source {
            Source::Text(_) => format!("{hex}\n"),
            // As sha256sum prints it, so the output is a SHA256SUMS file.
            _ => format!("{hex}  {}\n", source.name()),
        };
    }
    let rows: Vec<(String, String)> = hashed
        .hashes
        .iter()
        .map(|(a, hex)| (format!("  {}", a.name()), hex.clone()))
        .collect();
    format!("{}\n{}", source.name(), super::rows(&rows))
}

fn parse_check(expected: &str) -> Result<Expected, ConvError> {
    h::parse_expected(expected).map_err(|e| {
        if Path::new(expected.trim()).is_file() {
            super::usage(format!(
                "{expected} is a file, not a hash; to check against a sums file, use --sums {expected}"
            ))
        } else {
            super::usage(e)
        }
    })
}

fn check(cli: &Cli, args: &Args, sources: &[Source], expected: &Expected) -> i32 {
    let out = Paint::stdout();
    let err = Paint::stderr();
    let mut files = Vec::new();
    let mut code = 0;
    let mut human = String::new();
    let mut all_match = true;
    for source in sources {
        let hashed = match source.hash(&[expected.algorithm]) {
            Ok(h) => h,
            Err(e) => {
                code = code.max(e.code.exit_code());
                render_error(cli, &e);
                continue;
            }
        };
        let actual = &hashed.hashes[0].1;
        let matched = *actual == expected.hex;
        all_match &= matched;
        let name = source.name();
        if matched {
            let line = format!(
                "{} {name} matches ({})\n",
                out.ok(),
                expected.algorithm.name()
            );
            human.push_str(&line);
        } else {
            code = code.max(1);
            if !cli.json {
                eprint!(
                    "{} {name} does NOT match\n  expected  {}\n  actual    {actual}\n",
                    err.fail(),
                    expected.hex
                );
            }
        }
        let mut v = source.json(&hashed);
        v["check"] = json!({
            "algorithm": expected.algorithm.key(),
            "expected": expected.hex,
            "match": matched,
        });
        files.push(v);
    }
    if files.is_empty() {
        return code;
    }
    if all_match && expected.algorithm.broken() {
        super::note(
            cli,
            &format!(
                "{} shows the file wasn't damaged on the way, but whoever made it could have made another with the same hash; a SHA-256 says more",
                expected.algorithm.name()
            ),
        );
    }
    if cli.quiet && !cli.json {
        human.clear();
    }
    let copy = files
        .first()
        .and_then(|f| f[expected.algorithm.key()].as_str().map(str::to_string));
    super::finish(
        cli,
        args.copy,
        Done::new(human, copy, json!({ "files": files, "matched": all_match })).code(code),
    )
}

fn check_sums(cli: &Cli, args: &Args, sums_path: &Path) -> i32 {
    let text = match super::read_file(sums_path).and_then(|b| {
        String::from_utf8(b).map_err(|_| {
            super::failed(format!(
                "{} isn't a text file of checksums",
                sums_path.display()
            ))
        })
    }) {
        Ok(t) => t,
        Err(e) => return super::fail(cli, &e),
    };
    let sums = h::parse_sums(&text);
    if sums.lines.is_empty() {
        return super::fail(
            cli,
            &super::failed(format!(
                "{} has no checksums conv can read (HASH  NAME, or SHA256 (NAME) = HASH)",
                sums_path.display()
            )),
        );
    }
    let base = sums_path.parent().unwrap_or(Path::new("."));
    let sums_name = sums_path
        .file_name()
        .map(|n| crate::render::safe_for_terminal(&n.to_string_lossy()))
        .unwrap_or_default();
    // Which lines to check: the FILEs given, or every listed file here.
    let mut plan: Vec<(String, PathBuf, Option<&h::SumsLine>)> = Vec::new();
    let mut missing = 0usize;
    if args.files.is_empty() {
        for line in &sums.lines {
            let path = base.join(&line.file);
            if path.is_file() {
                plan.push((line.file.clone(), path, Some(line)));
            } else {
                missing += 1;
            }
        }
    } else {
        for f in &args.files {
            let name = f.file_name().map(|n| n.to_string_lossy().into_owned());
            let given = f.to_string_lossy();
            // The exact path first: two folders can each hold an app.zip.
            let line = sums.lines.iter().find(|l| l.file == given).or_else(|| {
                sums.lines.iter().find(|l| {
                    Path::new(&l.file)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        == name
                })
            });
            plan.push((f.display().to_string(), f.clone(), line));
        }
    }
    if plan.is_empty() {
        return super::fail(
            cli,
            &ConvError::new(
                ErrorCode::InputNotFound,
                format!(
                    "none of the {} files {sums_name} lists are in {}; give the file to check, or put it next to {sums_name}",
                    sums.lines.len(),
                    base.display()
                ),
            ),
        );
    }
    let out = Paint::stdout();
    let err = Paint::stderr();
    let mut human = String::new();
    let mut files = Vec::new();
    let (mut good, mut bad) = (0usize, 0usize);
    let mut code = 0;
    for (name, path, line) in plan {
        // A SUMS file names whatever it likes.
        let shown = crate::render::safe_for_terminal(&name);
        let Some(line) = line else {
            bad += 1;
            code = code.max(1);
            if !cli.json {
                eprintln!("{} {shown} isn't listed in {sums_name}", err.fail());
            }
            files.push(json!({ "name": name, "listed": false, "match": false }));
            continue;
        };
        let algorithm = line.expected.algorithm;
        let hashed = match Source::File(path.clone()).hash(&[algorithm]) {
            Ok(h) => h,
            Err(e) => {
                code = code.max(e.code.exit_code());
                render_error(cli, &e);
                continue;
            }
        };
        let actual = &hashed.hashes[0].1;
        let matched = *actual == line.expected.hex;
        if matched {
            good += 1;
            human.push_str(&format!("{} {shown}\n", out.ok()));
        } else {
            bad += 1;
            code = code.max(1);
            if !cli.json {
                eprint!(
                    "{} {shown} does NOT match\n  expected  {}\n  actual    {actual}\n",
                    err.fail(),
                    line.expected.hex
                );
            }
        }
        files.push(json!({
            "name": name,
            "path": std::path::absolute(&path).unwrap_or(path.clone()),
            "algorithm": algorithm.key(),
            "expected": line.expected.hex,
            "actual": actual,
            "match": matched,
        }));
    }
    let checked = good + bad;
    let summary = if bad == 0 {
        format!(
            "{} {sums_name}",
            convkit_core::tools::plural(good as u64, "file matches", "files match")
        )
    } else {
        format!(
            "{bad} of {} {sums_name}",
            convkit_core::tools::plural(
                checked as u64,
                "file doesn't match",
                if bad == 1 {
                    "files doesn't match"
                } else {
                    "files don't match"
                }
            )
        )
    };
    human.push_str(&summary);
    human.push('\n');
    if missing > 0 {
        super::note(
            cli,
            &format!(
                "{} that {sums_name} lists {} here, so {} checked",
                convkit_core::tools::plural(missing as u64, "file", "files"),
                if missing == 1 { "isn't" } else { "aren't" },
                if missing == 1 {
                    "it wasn't"
                } else {
                    "they weren't"
                }
            ),
        );
    }
    if !sums.malformed.is_empty() {
        let lines: Vec<String> = sums.malformed.iter().map(|n| n.to_string()).collect();
        super::warn(&format!(
            "skipped {} of {sums_name} that {} no checksum: {}",
            if lines.len() == 1 { "a line" } else { "lines" },
            if lines.len() == 1 { "holds" } else { "hold" },
            lines.join(", ")
        ));
    }
    if cli.quiet && bad == 0 && !cli.json {
        human.clear();
    }
    super::finish(
        cli,
        args.copy,
        Done::new(
            human.clone(),
            Some(human),
            json!({ "files": files, "matched": bad == 0, "missing": missing }),
        )
        .code(code),
    )
}
