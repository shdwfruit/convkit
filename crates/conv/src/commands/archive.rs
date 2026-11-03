//! Making and opening archives from the ordinary `conv IN OUT` grammar:
//!
//! ```text
//! conv photos/ .zip              photos.zip beside the folder
//! conv a.jpg b.pdf out.zip       both files in one zip
//! conv a.jpg b.pdf .zip          Archive.zip, as Finder names it
//! conv photos.zip                unpacked beside it (see Destination::Beside)
//! conv photos.zip ./dest         unpacked into ./dest
//! conv a.tar.gz .zip             converted: a.zip
//! conv *.zip                     each one unpacked
//! conv photos/ docs/ --to zip    one archive per input
//! ```
//!
//! The work is `convkit_core::archive`'s; this file reads the positionals,
//! asks for passwords and the zip-bomb question, and prints the result.

use std::path::{Path, PathBuf};
use std::time::Instant;

use convkit_core::archive::{
    self, ArchiveFormat, Destination, Expansion, PackOptions, Report, UnpackOptions,
};
use convkit_core::{ConvError, ErrorCode, Format, Outcome};
use serde_json::json;

use crate::cli::Cli;
use crate::input::{expand_globs, in_outdir};
use crate::prompt::{self, Gate};
use crate::{output, render};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Pack {
        inputs: Vec<PathBuf>,
        output: PathBuf,
        format: ArchiveFormat,
    },
    Unpack {
        archive: PathBuf,
        dest: Destination,
    },
    Convert {
        input: PathBuf,
        output: PathBuf,
        format: ArchiveFormat,
    },
}

impl Op {
    /// What a failure is reported against.
    fn input(&self) -> &Path {
        match self {
            Op::Pack { inputs, .. } => &inputs[0],
            Op::Unpack { archive, .. } => archive,
            Op::Convert { input, .. } => input,
        }
    }

    /// The target, as a failure's `-> zip` names it.
    fn target(&self) -> &'static str {
        match self {
            Op::Pack { format, .. } | Op::Convert { format, .. } => format.ext(),
            Op::Unpack { .. } => "folder",
        }
    }
}

fn invalid(message: impl Into<String>) -> ConvError {
    ConvError::new(ErrorCode::InvalidInvocation, message)
}

fn is_archive_file(p: &Path) -> bool {
    ArchiveFormat::from_path(p).is_some() && !p.is_dir()
}

/// An output positional that names an archive: the `.zip` / `.tar.gz`
/// shorthand (true), or a path ending in an archive suffix that is not a
/// folder already (false).
fn archive_target(p: &Path) -> Option<(ArchiveFormat, bool)> {
    let text = p.to_string_lossy();
    if let Some(rest) = text.strip_prefix('.') {
        if !rest.contains('/') && !rest.contains('\\') {
            if let Some(f) = ArchiveFormat::from_name(rest) {
                return Some((f, true));
            }
        }
    }
    if p.is_dir() {
        return None;
    }
    ArchiveFormat::from_path(p).map(|f| (f, false))
}

/// The folder an output goes in: `-o`, or beside the input.
fn beside(input: &Path, outdir: Option<&Path>) -> PathBuf {
    if let Some(dir) = outdir {
        return dir.to_path_buf();
    }
    // `.` and `..` have no parent to speak of; their real folder's does.
    let real = if input.file_name().is_none() {
        std::fs::canonicalize(input).unwrap_or_else(|_| input.to_path_buf())
    } else {
        input.to_path_buf()
    };
    match real.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// The name a file or folder is packed under, without any extension for a
/// file: `photos/` -> `photos`, `report.pdf` -> `report`, `.` -> the
/// folder's own name.
fn pack_stem(input: &Path) -> String {
    let real = if input.file_name().is_none() {
        std::fs::canonicalize(input).unwrap_or_else(|_| input.to_path_buf())
    } else {
        input.to_path_buf()
    };
    let name = if real.is_dir() {
        real.file_name()
    } else {
        real.file_stem()
    };
    name.map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Archive".to_string())
}

fn with_ext(stem: &str, format: ArchiveFormat) -> String {
    format!("{stem}.{}", format.ext())
}

/// The archive operations this invocation asks for: `None` when it is not
/// about archives at all, and the ordinary conversion path should read it.
pub fn detect(cli: &Cli) -> Option<Result<Vec<Op>, ConvError>> {
    if cli.command.is_some() || cli.paths.is_empty() && cli.to.is_none() {
        return None;
    }
    let ops = detect_ops(cli)?;
    Some(ops.and_then(|ops| {
        refuse_conversion_flags(cli)?;
        refuse_a_typed_password(cli, &ops)?;
        Ok(ops)
    }))
}

fn detect_ops(cli: &Cli) -> Option<Result<Vec<Op>, ConvError>> {
    let paths = &cli.paths;
    let outdir = cli.outdir.as_deref();
    if let Some(to) = &cli.to {
        let all = expand_globs(paths, paths.len());
        return match ArchiveFormat::from_name(to) {
            Some(format) => Some(for_each(&all, format, outdir)),
            None => all.iter().find(|p| is_archive_file(p)).map(|a| {
                Err(invalid(format!(
                    "{} is an archive; `conv {}` unpacks it, and --to with an archive format \
                     ({}) converts it",
                    a.display(),
                    a.display(),
                    ArchiveFormat::writable_list()
                )))
            }),
        };
    }
    let n = paths.len();
    if n >= 2 {
        if let Some((format, shorthand)) = archive_target(&paths[n - 1]) {
            let inputs = expand_globs(&paths[..n - 1], n - 1);
            // `conv *.zip` names only archives that exist: each one is
            // opened. Reading the last as the output would write one over
            // another.
            let all_existing =
                inputs.iter().all(|p| is_archive_file(p) && p.exists()) && paths[n - 1].is_file();
            if !(all_existing && !shorthand) {
                return Some(pair(&inputs, &paths[n - 1], format, shorthand, outdir));
            }
        }
    }
    let all = expand_globs(paths, n);
    if all.iter().all(|p| is_archive_file(p)) {
        return Some(Ok(all
            .into_iter()
            .map(|archive| Op::Unpack {
                dest: Destination::Beside(beside(&archive, outdir)),
                archive,
            })
            .collect()));
    }
    if let [archive, dest] = paths.as_slice() {
        if is_archive_file(archive) {
            return Some(into_folder(archive, dest, outdir));
        }
    }
    None
}

/// `conv a.zip FOLDER`.
fn into_folder(archive: &Path, dest: &Path, outdir: Option<&Path>) -> Result<Vec<Op>, ConvError> {
    if !dest.is_dir() && Format::from_path(dest).is_some() {
        return Err(invalid(format!(
            "{} is an archive: `conv {}` unpacks it beside itself, `conv {} FOLDER` into a folder, \
             and `conv {} .tar.gz` converts it",
            archive.display(),
            archive.display(),
            archive.display(),
            archive.display()
        )));
    }
    if let Some(dir) = outdir {
        return Err(invalid(format!(
            "{} is already the folder to unpack into; drop -o {}",
            dest.display(),
            dir.display()
        )));
    }
    Ok(vec![Op::Unpack {
        archive: archive.to_path_buf(),
        dest: Destination::Into(dest.to_path_buf()),
    }])
}

/// `conv IN... OUT.zip` and `conv IN... .zip`.
fn pair(
    inputs: &[PathBuf],
    target: &Path,
    format: ArchiveFormat,
    shorthand: bool,
    outdir: Option<&Path>,
) -> Result<Vec<Op>, ConvError> {
    if !format.can_write() {
        return Err(format.unwritable());
    }
    if let [input] = inputs {
        if is_archive_file(input) && input.exists() {
            let from = ArchiveFormat::from_path(input).expect("checked above");
            let output = if shorthand {
                let stem = ArchiveFormat::stem(input).unwrap_or_default();
                beside(input, outdir).join(with_ext(&stem, format))
            } else {
                in_outdir(target, outdir)?
            };
            if from == format && shorthand {
                return Err(invalid(format!(
                    "{} is already a .{} archive",
                    input.display(),
                    format.ext()
                )));
            }
            return Ok(vec![Op::Convert {
                input: input.clone(),
                output,
                format,
            }]);
        }
    }
    let output = match (shorthand, inputs) {
        (false, _) => in_outdir(target, outdir)?,
        (true, [one]) => beside(one, outdir).join(with_ext(&pack_stem(one), format)),
        // Several things into one archive: Finder's name for it.
        (true, [first, ..]) => beside(first, outdir).join(with_ext("Archive", format)),
        (true, []) => unreachable!("a pair has an input"),
    };
    Ok(vec![Op::Pack {
        inputs: inputs.to_vec(),
        output,
        format,
    }])
}

/// `conv A B --to zip`: each input on its own, converted if it is an
/// archive and packed if it is not.
fn for_each(
    inputs: &[PathBuf],
    format: ArchiveFormat,
    outdir: Option<&Path>,
) -> Result<Vec<Op>, ConvError> {
    if !format.can_write() {
        return Err(format.unwritable());
    }
    if inputs.is_empty() {
        return Err(invalid(format!(
            "nothing to put in a .{}; name files or folders",
            format.ext()
        )));
    }
    let mut ops = Vec::new();
    for input in inputs {
        if is_archive_file(input) {
            if ArchiveFormat::from_path(input) == Some(format) {
                return Err(invalid(format!(
                    "{} is already a .{} archive",
                    input.display(),
                    format.ext()
                )));
            }
            let stem = ArchiveFormat::stem(input).unwrap_or_default();
            ops.push(Op::Convert {
                input: input.clone(),
                output: beside(input, outdir).join(with_ext(&stem, format)),
                format,
            });
        } else {
            ops.push(Op::Pack {
                inputs: vec![input.clone()],
                output: beside(input, outdir).join(with_ext(&pack_stem(input), format)),
                format,
            });
        }
    }
    let mut outputs: Vec<&Path> = ops
        .iter()
        .filter_map(|op| match op {
            Op::Pack { output, .. } | Op::Convert { output, .. } => Some(output.as_path()),
            Op::Unpack { .. } => None,
        })
        .collect();
    outputs.sort();
    if let Some(w) = outputs.windows(2).find(|w| w[0] == w[1]) {
        return Err(invalid(format!(
            "two inputs would both be written as {}; convert them one at a time",
            w[0].display()
        )));
    }
    Ok(ops)
}

/// A conversion flag means nothing to an archive, and is refused rather
/// than ignored.
fn refuse_conversion_flags(cli: &Cli) -> Result<(), ConvError> {
    if cli.tuning().is_empty() {
        Ok(())
    } else {
        Err(invalid(
            "--resize, --quality and the other conversion flags don't apply to archives",
        ))
    }
}

/// `--password hunter2` reads as the flag and a file named `hunter2`.
/// Say so, rather than report a missing input, since the password is now
/// in the shell's history.
fn refuse_a_typed_password(cli: &Cli, ops: &[Op]) -> Result<(), ConvError> {
    if !cli.password {
        return Ok(());
    }
    let missing = ops.iter().find_map(|op| match op {
        Op::Pack { inputs, .. } => inputs.iter().find(|p| !p.exists()).cloned(),
        _ => None,
    });
    match missing {
        Some(p) => Err(invalid(format!(
            "--password takes no value: conv asks for the password so it stays out of your \
             shell history. {} is not a file; if it was the password, it is in your shell \
             history now, so pick another",
            p.display()
        ))),
        None => Ok(()),
    }
}

/// Reads a password without showing it.
fn ask_password(question: &str) -> Option<String> {
    eprint!("{question}");
    console::Term::stderr()
        .read_secure_line()
        .ok()
        .filter(|p| !p.is_empty())
}

fn env_password() -> Option<String> {
    std::env::var(archive::PASSWORD_ENV)
        .ok()
        .filter(|p| !p.is_empty())
}

/// The password a new zip is locked with, under `--password`.
fn new_password(
    cli: &Cli,
    output: &Path,
    format: ArchiveFormat,
) -> Result<Option<String>, ConvError> {
    if !cli.password {
        return Ok(None);
    }
    // Refused before asking, not after.
    if format != ArchiveFormat::Zip {
        return Err(invalid(format!(
            "--password locks zip archives only; a .{} can't hold one",
            format.ext()
        )));
    }
    if let Some(p) = env_password() {
        return Ok(Some(p));
    }
    if !prompt::is_interactive_session() {
        return Err(invalid(format!(
            "--password needs a terminal to ask in, or the password in {}",
            archive::PASSWORD_ENV
        )));
    }
    let name = output.file_name().unwrap_or_default().to_string_lossy();
    let first = ask_password(&format!("Password for {name}: "))
        .ok_or_else(|| invalid("no password given; nothing was written"))?;
    let again = ask_password("Again, to check: ").unwrap_or_default();
    if first != again {
        return Err(invalid("the two passwords differ; nothing was written"));
    }
    Ok(Some(first))
}

/// The password for opening an archive: `CONVKIT_PASSWORD`, or asked for
/// up front under `--password` on an unpack.
fn open_password(archive: &Path, ask_now: bool) -> Option<String> {
    if let Some(p) = env_password() {
        return Some(p);
    }
    if !(ask_now && prompt::is_interactive_session()) {
        return None;
    }
    let name = archive.file_name().unwrap_or_default().to_string_lossy();
    ask_password(&format!("Password for {name}: "))
}

/// Runs `attempt` with the password known so far, and once more with one
/// asked for if the archive turns out to need one and there is a terminal.
fn with_open_password(
    archive: &Path,
    known: Option<String>,
    mut attempt: impl FnMut(Option<String>) -> Result<Report, ConvError>,
) -> Result<Report, ConvError> {
    match attempt(known.clone()) {
        Err(e)
            if e.code == ErrorCode::PasswordProtected
                && known.is_none()
                && e.message.ends_with("is password-protected")
                && prompt::is_interactive_session() =>
        {
            let name = archive.file_name().unwrap_or_default().to_string_lossy();
            match ask_password(&format!("{name} is locked. Password: ")) {
                Some(p) => attempt(Some(p)),
                None => Err(e),
            }
        }
        other => other,
    }
}

/// The zip-bomb question, asked as `--upscale` and `--max-size` ask theirs.
fn confirm_large(cli: &Cli) -> impl FnMut(&Expansion) -> bool + '_ {
    move |e: &Expansion| match prompt::extreme_gate(
        cli.yes,
        cli.json,
        cli.quiet,
        prompt::is_interactive_session(),
    ) {
        Gate::Proceed => true,
        Gate::Refuse => false,
        Gate::Ask => {
            eprintln!("{}", e.warning());
            prompt::ask("Unpack anyway? [y/N] ")
        }
    }
}

fn run_op(cli: &Cli, op: &Op) -> Result<Report, ConvError> {
    let overwrite = cli.overwrite;
    match op {
        Op::Pack {
            inputs,
            output,
            format,
        } => {
            for input in inputs {
                if !input.exists() {
                    return Err(ConvError::new(
                        ErrorCode::InputNotFound,
                        format!("input not found: {}", input.display()),
                    ));
                }
            }
            if output.exists() && !overwrite {
                return Err(ConvError::new(
                    ErrorCode::OutputExists,
                    format!("{} exists; pass -y to overwrite", output.display()),
                ));
            }
            let opts = PackOptions {
                overwrite,
                password: new_password(cli, output, *format)?,
            };
            archive::pack(inputs, output, *format, &opts)
        }
        Op::Unpack { archive: a, dest } => {
            let known = open_password(a, cli.password);
            with_open_password(a, known, |password| {
                let opts = UnpackOptions {
                    overwrite,
                    password,
                };
                archive::unpack(a, dest, &opts, &mut confirm_large(cli))
            })
        }
        Op::Convert {
            input,
            output,
            format,
        } => {
            if output.exists() && !overwrite {
                return Err(ConvError::new(
                    ErrorCode::OutputExists,
                    format!("{} exists; pass -y to overwrite", output.display()),
                ));
            }
            let write = PackOptions {
                overwrite,
                password: new_password(cli, output, *format)?,
            };
            let known = open_password(input, false);
            with_open_password(input, known, |password| {
                let read = UnpackOptions {
                    overwrite,
                    password,
                };
                archive::convert(
                    input,
                    output,
                    *format,
                    &read,
                    &write,
                    &mut confirm_large(cli),
                )
            })
        }
    }
}

pub fn run(cli: &Cli, ops: Vec<Op>) -> i32 {
    if cli.dry_run {
        return dry_run(cli, &ops);
    }
    if let Err(e) = output::ensure_dir(cli.outdir.as_deref()) {
        render::print_error(cli.json, &e);
        return e.code.exit_code();
    }
    let results: Vec<(Op, Result<Report, ConvError>, u64)> = ops
        .into_iter()
        .map(|op| {
            let start = Instant::now();
            let r = run_op(cli, &op);
            (op, r, start.elapsed().as_millis() as u64)
        })
        .collect();
    print(cli, &results);
    let failures: Vec<&ConvError> = results
        .iter()
        .filter_map(|(_, r, _)| r.as_ref().err())
        .collect();
    match (failures.as_slice(), results.len()) {
        ([], _) => 0,
        (f, n) if f.len() == n => f[0].code.exit_code(),
        _ => ErrorCode::BatchPartialFailure.exit_code(),
    }
}

fn outcome(r: &Report, elapsed_ms: u64) -> Outcome {
    let mut o = output_outcome(&r.output, r.bytes);
    o.warnings = r.notes.clone();
    o.elapsed_ms = elapsed_ms;
    o
}

fn output_outcome(path: &Path, bytes: u64) -> Outcome {
    Outcome {
        output: path.to_path_buf(),
        bytes,
        warnings: vec![],
        notes: vec![],
        backend_output: vec![],
        backends: vec![],
        remuxed: false,
        elapsed_ms: 0,
        sizing: None,
        enlarged: None,
        range: None,
        speed: None,
        denoise: None,
        rotation: None,
        pages: None,
        crop: None,
        audio: None,
        subtitles: None,
        stills: None,
    }
}

fn action(op: &Op) -> &'static str {
    match op {
        Op::Pack { .. } => "pack",
        Op::Unpack { .. } => "unpack",
        Op::Convert { .. } => "convert",
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// `12 files`, `12 files, 2 links`.
fn holds(r: &Report) -> String {
    let mut s = plural(r.files, "file", "files");
    if r.links > 0 {
        s.push_str(&format!(", {}", plural(r.links, "link", "links")));
    }
    s
}

fn print(cli: &Cli, results: &[(Op, Result<Report, ConvError>, u64)]) {
    if cli.json {
        let arr: Vec<serde_json::Value> = results
            .iter()
            .map(|(op, r, ms)| match r {
                Ok(rep) => {
                    let mut v = render::outcome_json(&outcome(rep, *ms));
                    v["input"] = json!(op.input());
                    v["archive"] = json!({
                        "action": action(op),
                        "format": rep.format,
                        "files": rep.files,
                        "folders": rep.folders,
                        "links": rep.links,
                        "content_bytes": rep.content_bytes,
                        "encrypted": rep.encrypted,
                    });
                    v
                }
                Err(e) => json!({ "ok": false, "input": op.input(), "error": e }),
            })
            .collect();
        let ok = results.iter().all(|(_, r, _)| r.is_ok());
        let envelope = json!({ "ok": ok, "results": arr });
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
        return;
    }
    let styled_out = render::stdout_styled();
    let styled_err = render::stderr_styled();
    for (op, r, ms) in results {
        match r {
            Ok(rep) => {
                if !cli.quiet {
                    let folder = rep.output.is_dir();
                    print!(
                        "{}",
                        render::archive_success_human(
                            &outcome(rep, *ms),
                            folder,
                            &holds(rep),
                            styled_out
                        )
                    );
                }
            }
            Err(e) => eprint!(
                "{}",
                render::conversion_failure_human(op.input(), op.target(), e, styled_err)
            ),
        }
    }
}

/// Where an unpack would land, read from the archive's listing.
fn unpack_target(archive: &Path, dest: &Destination, listing: &archive::Listing) -> PathBuf {
    match dest {
        Destination::Into(out) => out.clone(),
        Destination::Beside(dir) => match listing.top_level().as_slice() {
            [only] => dir.join(only),
            _ => dir.join(ArchiveFormat::stem(archive).unwrap_or_default()),
        },
    }
}

fn dry_run(cli: &Cli, ops: &[Op]) -> i32 {
    let mut code = 0;
    let mut plans = Vec::new();
    for op in ops {
        let plan = match op {
            Op::Pack {
                inputs,
                output,
                format,
            } => {
                let from: Vec<String> = inputs.iter().map(|p| p.display().to_string()).collect();
                Ok((
                    json!({ "action": "pack", "inputs": inputs, "output": output, "format": format }),
                    render::archive_plan_human(
                        "pack",
                        &from.join(" "),
                        output,
                        &format!("{}, built in", format.ext()),
                    ),
                ))
            }
            Op::Convert {
                input,
                output,
                format,
            } => Ok((
                json!({ "action": "convert", "input": input, "output": output, "format": format }),
                render::archive_plan_human(
                    "convert",
                    &input.display().to_string(),
                    output,
                    &format!("{}, built in", format.ext()),
                ),
            )),
            Op::Unpack { archive: a, dest } => {
                archive::list(a, env_password().as_deref()).map(|listing| {
                    let to = unpack_target(a, dest, &listing);
                    let files = listing.files();
                    let detail = format!(
                        "{}, {}, built in",
                        plural(files as u64, "file", "files"),
                        convkit_core::size::human(listing.total())
                    );
                    (
                        json!({ "action": "unpack", "input": a, "output": to, "listing": listing }),
                        render::archive_plan_human(
                            "unpack",
                            &a.display().to_string(),
                            &to,
                            &detail,
                        ),
                    )
                })
            }
        };
        match plan {
            Ok((v, line)) => {
                if !cli.json {
                    print!("{line}");
                }
                plans.push(json!({ "ok": true, "plan": v }));
            }
            Err(e) => {
                if code == 0 {
                    code = e.code.exit_code();
                }
                if !cli.json {
                    eprint!("{}", render::error_human(&e));
                }
                plans.push(json!({ "ok": false, "error": e }));
            }
        }
    }
    if cli.json {
        let ok = code == 0;
        let envelope = json!({ "ok": ok, "dry_run": true, "plans": plans });
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn ops(args: &[&str]) -> Option<Result<Vec<Op>, ConvError>> {
        let cli = Cli::parse_from(std::iter::once("conv").chain(args.iter().copied()));
        detect(&cli)
    }

    fn ok(args: &[&str]) -> Vec<Op> {
        ops(args).expect("an archive invocation").expect("accepted")
    }

    #[test]
    fn ordinary_conversions_are_not_archive_invocations() {
        assert!(ops(&["clip.mp4", "clip.gif"]).is_none());
        assert!(ops(&["a.png", "b.png", "out.pdf"]).is_none());
        assert!(ops(&["photos", "--to", "jpg"]).is_none());
        assert!(ops(&["notes.gz"]).is_none());
    }

    #[test]
    fn a_folder_and_a_bare_extension_pack_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        std::fs::create_dir(&photos).unwrap();
        let p = photos.to_string_lossy().into_owned();
        assert_eq!(
            ok(&[&p, ".tar.gz"]),
            [Op::Pack {
                inputs: vec![photos.clone()],
                output: dir.path().join("photos.tar.gz"),
                format: ArchiveFormat::TarGz,
            }]
        );
        let o = dir.path().join("out").to_string_lossy().into_owned();
        assert_eq!(
            ok(&[&p, ".zip", "-o", &o]),
            [Op::Pack {
                inputs: vec![photos],
                output: dir.path().join("out").join("photos.zip"),
                format: ArchiveFormat::Zip,
            }]
        );
    }

    #[test]
    fn several_files_pack_into_a_named_zip_or_archive_zip() {
        assert_eq!(
            ok(&["a.jpg", "b.pdf", "out.zip"]),
            [Op::Pack {
                inputs: vec!["a.jpg".into(), "b.pdf".into()],
                output: "out.zip".into(),
                format: ArchiveFormat::Zip,
            }]
        );
        assert_eq!(
            ok(&["a.jpg", "b.pdf", ".zip"]),
            [Op::Pack {
                inputs: vec!["a.jpg".into(), "b.pdf".into()],
                output: PathBuf::from(".").join("Archive.zip"),
                format: ArchiveFormat::Zip,
            }]
        );
    }

    #[test]
    fn a_lone_archive_unpacks_beside_itself() {
        assert_eq!(
            ok(&["dl/photos.zip"]),
            [Op::Unpack {
                archive: "dl/photos.zip".into(),
                dest: Destination::Beside("dl".into()),
            }]
        );
        assert_eq!(
            ok(&["photos.tar.gz", "-o", "out"]),
            [Op::Unpack {
                archive: "photos.tar.gz".into(),
                dest: Destination::Beside("out".into()),
            }]
        );
    }

    #[test]
    fn an_archive_and_a_folder_unpack_into_the_folder() {
        assert_eq!(
            ok(&["photos.zip", "dest"]),
            [Op::Unpack {
                archive: "photos.zip".into(),
                dest: Destination::Into("dest".into()),
            }]
        );
        let e = ops(&["photos.zip", "dest", "-o", "x"])
            .unwrap()
            .unwrap_err();
        assert!(e.message.contains("drop -o"), "{}", e.message);
        let e = ops(&["photos.zip", "out.pdf"]).unwrap().unwrap_err();
        assert!(e.message.contains("unpacks it"), "{}", e.message);
    }

    #[test]
    fn an_archive_and_another_format_converts() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.tar.gz");
        std::fs::write(&a, "x").unwrap();
        let s = a.to_string_lossy().into_owned();
        assert_eq!(
            ok(&[&s, ".zip"]),
            [Op::Convert {
                input: a.clone(),
                output: dir.path().join("a.zip"),
                format: ArchiveFormat::Zip,
            }]
        );
        let e = ops(&[&s, ".tgz"]).unwrap().unwrap_err();
        assert!(e.message.contains("already"), "{}", e.message);
    }

    /// `conv *.zip` must never read the last match as the output.
    #[test]
    fn several_existing_archives_each_unpack() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.zip");
        let b = dir.path().join("b.zip");
        std::fs::write(&a, "x").unwrap();
        std::fs::write(&b, "x").unwrap();
        let got = ok(&[&a.to_string_lossy(), &b.to_string_lossy()]);
        assert!(
            matches!(got.as_slice(), [Op::Unpack { .. }, Op::Unpack { .. }]),
            "{got:?}"
        );
    }

    #[test]
    fn to_makes_one_archive_per_input() {
        let got = ok(&["a.txt", "b.tar", "--to", "tar.xz"]);
        assert_eq!(
            got,
            [
                Op::Pack {
                    inputs: vec!["a.txt".into()],
                    output: PathBuf::from(".").join("a.tar.xz"),
                    format: ArchiveFormat::TarXz,
                },
                Op::Convert {
                    input: "b.tar".into(),
                    output: PathBuf::from(".").join("b.tar.xz"),
                    format: ArchiveFormat::TarXz,
                },
            ]
        );
    }

    #[test]
    fn writing_7z_or_rar_is_refused() {
        let e = ops(&["photos", ".7z"]).unwrap().unwrap_err();
        assert!(e.message.contains("doesn't make them"), "{}", e.message);
        let e = ops(&["photos", "--to", "rar"]).unwrap().unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedPair);
    }

    #[test]
    fn conversion_flags_are_refused() {
        let e = ops(&["photos.zip", "--resize", "50%"])
            .unwrap()
            .unwrap_err();
        assert!(
            e.message.contains("don't apply to archives"),
            "{}",
            e.message
        );
    }

    #[test]
    fn a_password_typed_after_the_flag_is_called_out() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_string_lossy().into_owned();
        let e = ops(&[&p, "--password", "hunter2", "out.zip"])
            .unwrap()
            .unwrap_err();
        assert!(e.message.contains("shell history"), "{}", e.message);
    }

    #[test]
    fn a_non_archive_to_with_an_archive_input_says_how() {
        let e = ops(&["a.zip", "--to", "jpg"]).unwrap().unwrap_err();
        assert!(e.message.contains("unpacks it"), "{}", e.message);
    }
}
