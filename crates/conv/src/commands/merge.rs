//! `conv merge IN... OUT`: join files into one. PDFs alone on qpdf
//! (`pdf::plan_merge`), PDFs and pictures through Typst and then qpdf
//! (`merge::pages`), pictures alone as the photo merge (`conv a.jpg b.jpg
//! out.pdf`), and video or audio files on ffmpeg (`merge::clips`).

use std::path::{Path, PathBuf};
use std::time::Instant;

use convkit_core::merge::{clips, pages};
use convkit_core::{
    pdf, sized, Backend, ConvError, ErrorCode, Event, Format, Kind, Remediation, ResolvedBackend,
};
use serde_json::json;

use crate::cli::{Cli, MergeArgs};
use crate::commands::{install, pdf_support};
use crate::password::{self, Opener};
use crate::prompt::{self, Gate};
use crate::{input, install_prompt, render};

/// What the output is made from, by its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    /// PDFs and pictures.
    Pdf,
    Video,
    Audio,
}

/// The output formats a merge writes: a PDF, and the video and audio
/// formats conv writes (not avi, which conv only reads).
fn family_of(to: Format) -> Option<Family> {
    match to {
        Format::Pdf => Some(Family::Pdf),
        Format::Mp4 | Format::Mov | Format::Mkv | Format::Webm => Some(Family::Video),
        f if f.kind() == Kind::Audio => Some(Family::Audio),
        _ => None,
    }
}

impl Family {
    /// Whether an input of format `f` goes into this family's output.
    fn takes(self, f: Format) -> bool {
        match self {
            Family::Pdf => f == Format::Pdf || f.kind() == Kind::Image,
            // A GIF is a clip without sound.
            Family::Video => f.kind() == Kind::Video || f == Format::Gif,
            Family::Audio => matches!(f.kind(), Kind::Video | Kind::Audio),
        }
    }

    /// Whether a folder's file of format `f` is added: what the output
    /// takes, except that a folder given to an audio merge adds its audio
    /// files, not its videos.
    fn takes_from_folder(self, f: Format) -> bool {
        match self {
            Family::Audio => f.kind() == Kind::Audio,
            Family::Video => f.kind() == Kind::Video,
            Family::Pdf => self.takes(f),
        }
    }

    /// What a folder of this family's files holds, for "has no ...".
    fn files(self) -> &'static str {
        match self {
            Family::Pdf => "PDFs or pictures",
            Family::Video => "videos",
            Family::Audio => "audio files",
        }
    }
}

pub fn run(cli: &Cli, args: &MergeArgs) -> i32 {
    let typed = args.lock.password.then(|| args.paths.last()).flatten();
    if let Some(last) = typed.filter(|p| Format::from_path(p).is_none() && !p.exists()) {
        let e = password::typed_after_the_flag(&last.display().to_string());
        render::print_error(cli.json, &e);
        return e.code.exit_code();
    }
    let (inputs, output, to) = match split_paths(&args.paths) {
        Ok(p) => p,
        Err(e) => {
            render::print_error(cli.json, &e);
            return e.code.exit_code();
        }
    };
    let family = family_of(to).expect("split_paths only returns a format a merge writes");
    if let Some(e) =
        refused_knob(args, family, &output).or_else(|| refused_lock(args, family, &inputs, &output))
    {
        render::print_error(cli.json, &e);
        return e.code.exit_code();
    }
    let subject = json!({ "inputs": inputs, "output": output });
    let header = name_of(&output);
    // Checked before offering to install anything.
    if let Some(missing) = inputs.iter().find(|p| !p.is_file()) {
        let e = ConvError::new(
            ErrorCode::InputNotFound,
            format!("input not found: {}", missing.display()),
        );
        return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run);
    }
    match family {
        Family::Pdf => {
            let pictures = inputs.iter().filter(|p| pages::is_picture(p)).count();
            if pictures == 0 {
                run_pdfs(cli, args, &inputs, &output)
            } else if pictures == inputs.len() {
                run_pictures(cli, args, &inputs, &output)
            } else {
                run_pages(cli, args, &inputs, &output)
            }
        }
        Family::Video | Family::Audio => run_clips(cli, args, &inputs, &output, to),
    }
}

/// `--resize`, `--crf` and `--max-size` where they mean nothing: on a PDF,
/// and on sound alone.
fn refused_knob(args: &MergeArgs, family: Family, output: &Path) -> Option<ConvError> {
    let given = [
        (args.resize.is_some(), "--resize"),
        (args.crf.is_some(), "--crf"),
        (args.max_size.is_some(), "--max-size"),
    ];
    let flag = given.iter().find_map(|(g, f)| g.then_some(*f))?;
    let why = match family {
        Family::Video => return None,
        Family::Pdf => format!(
            "{flag} works on a merge of videos, not on {}",
            name_of(output)
        ),
        Family::Audio if flag == "--max-size" => format!(
            "--max-size works on a merge of videos; {} holds only sound",
            name_of(output)
        ),
        Family::Audio => format!(
            "{flag} sets the picture, and {} holds only sound",
            name_of(output)
        ),
    };
    Some(invalid(why))
}

/// `--password` where it can't lock anything: a video or audio merge, and
/// pictures alone, which go through the photo merge rather than qpdf.
fn refused_lock(
    args: &MergeArgs,
    family: Family,
    inputs: &[PathBuf],
    output: &Path,
) -> Option<ConvError> {
    if !args.lock.password {
        return None;
    }
    match family {
        Family::Video | Family::Audio => Some(invalid(format!(
            "--password locks a PDF, and {} is not one",
            name_of(output)
        ))),
        Family::Pdf if inputs.iter().all(|p| pages::is_picture(p)) => Some(ConvError {
            remediation: Some(Remediation {
                managed: None,
                manual: Some(format!(
                    "merge without --password, then: conv {} --password",
                    output.display()
                )),
            }),
            ..invalid(
                "--password locks a merge with a PDF in it; lock a merge of pictures alone \
                 once it is written"
                    .to_string(),
            )
        }),
        Family::Pdf => None,
    }
}

fn run_pdfs(cli: &Cli, args: &MergeArgs, inputs: &[PathBuf], output: &Path) -> i32 {
    let subject = json!({ "inputs": inputs, "output": output });
    let header = name_of(output);
    let qpdf = match pdf_support::resolve_qpdf(cli, !args.dry_run) {
        Ok(q) => q,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run),
    };

    let mut opener = match Opener::from_env() {
        Ok(o) => o,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run),
    };
    let planned = pdf::plan_merge(&qpdf.path, inputs, output, &mut |p, wrong| {
        opener.next(p, wrong)
    })
    .and_then(|plan| pdf_support::locked(plan, &args.lock, &header, args.dry_run));
    let plan = match planned {
        Ok(p) => p,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run),
    };
    if args.dry_run {
        pdf_support::print_dry_run(cli, &plan);
        return 0;
    }

    let start = Instant::now();
    let mut on_event = pdf_support::verbose_printer(args.verbose);
    match pdf::run(&plan, &qpdf, args.overwrite, &mut on_event) {
        Ok(o) => {
            pdf_support::print_success(cli, subject, &o, start.elapsed());
            0
        }
        Err(e) => failed(cli, args, subject, &header, e),
    }
}

/// Pictures alone are the photo merge, `conv a.jpg b.jpg out.pdf`, with a
/// page of each picture's own size: there is no PDF page to match.
fn run_pictures(cli: &Cli, args: &MergeArgs, inputs: &[PathBuf], output: &Path) -> i32 {
    let mut c = cli.clone();
    c.command = None;
    c.paths = inputs
        .iter()
        .cloned()
        .chain(std::iter::once(output.to_path_buf()))
        .collect();
    c.overwrite = args.overwrite;
    c.dry_run = args.dry_run;
    c.verbose = args.verbose;
    crate::commands::convert::run(&c)
}

fn run_pages(cli: &Cli, args: &MergeArgs, inputs: &[PathBuf], output: &Path) -> i32 {
    let subject = json!({ "inputs": inputs, "output": output });
    let header = name_of(output);
    let qpdf = match pdf_support::resolve_qpdf(cli, !args.dry_run) {
        Ok(q) => q,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run),
    };
    let mut opener = match Opener::from_env() {
        Ok(o) => o,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run),
    };
    let planned = pages::plan_merge(&qpdf.path, inputs, output, &mut |p, wrong| {
        opener.next(p, wrong)
    })
    .and_then(|mut p| {
        let lock = &args.lock;
        if let Some(lock) =
            password::lock_for(lock.password, &lock.restrict, &header, args.dry_run)?
        {
            p.set_lock(lock);
        }
        let commands = p.commands()?;
        Ok((p, commands))
    });
    let (plan, commands) = match planned {
        Ok(p) => p,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run),
    };
    let notes = plan.all_notes().unwrap_or_default();
    let warnings = plan.warnings().unwrap_or_default();
    if args.dry_run {
        if cli.json {
            let commands: Vec<Vec<String>> = commands
                .iter()
                .map(|(b, argv)| {
                    std::iter::once(b.exe_name().to_string())
                        .chain(argv.iter().cloned())
                        .collect()
                })
                .collect();
            let layouts: Vec<_> = plan
                .layouts()
                .into_iter()
                .map(|(path, contents)| json!({ "path": path, "contents": contents }))
                .collect();
            let v = json!({ "ok": true, "dry_run": true, "plans": [{
                "ok": true,
                "inputs": inputs,
                "output": output,
                "pages": plan.pages(),
                "commands": commands,
                "layouts": layouts,
                "warnings": notes,
                "notes": warnings,
            }]});
            println!("{}", serde_json::to_string_pretty(&v).unwrap());
        } else {
            let mut s = String::new();
            for (b, argv) in &commands {
                s.push_str(&render::command_line_human(b.exe_name(), argv));
                s.push('\n');
            }
            for w in &warnings {
                s.push_str(&format!("warning: {w}\n"));
            }
            for n in &notes {
                s.push_str(&format!("note: {n}\n"));
            }
            print!("{s}");
        }
        return 0;
    }
    let typst = match resolve_offering(cli, Backend::Typst, true) {
        Ok(t) => t,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, false),
    };
    let start = Instant::now();
    let progress = Progress::new(cli, args.verbose);
    let mut on_event = |e: Event| progress.event(e);
    let result = pages::run(
        &plan,
        &cli.resolver(),
        &typst,
        &qpdf,
        args.overwrite,
        &mut on_event,
    );
    progress.finish();
    match result {
        Ok(o) => {
            if cli.json {
                let mut v = render::pdf_outcome_json(&o, start.elapsed());
                v["inputs"] = json!(inputs);
                v["backends"] = json!([
                    { "backend": Backend::Typst, "version": typst.version },
                    { "backend": Backend::Qpdf, "version": o.version },
                ]);
                let envelope = json!({ "ok": true, "results": [v] });
                println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
            } else {
                pdf_support::print_success(cli, subject, &o, start.elapsed());
            }
            0
        }
        Err(e) => failed(cli, args, subject, &header, e),
    }
}

fn run_clips(cli: &Cli, args: &MergeArgs, inputs: &[PathBuf], output: &Path, to: Format) -> i32 {
    let subject = json!({ "inputs": inputs, "output": output });
    let header = name_of(output);
    let ffprobe = match resolve_offering(cli, Backend::Ffprobe, !args.dry_run) {
        Ok(p) => p,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run),
    };
    if args.max_size.is_some() {
        return run_clips_sized(cli, args, inputs, output, to, &ffprobe);
    }
    let opts = clips::Options {
        resize: args.resize.clone(),
        crf: args.crf,
        encode_crf: None,
    };
    let plan = match clips::plan_merge(&ffprobe.path, inputs, to, output, &opts) {
        Ok(p) => p,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run),
    };
    if args.dry_run {
        print_clips_dry_run(cli, &plan, None);
        return 0;
    }
    let ffmpeg = match resolve_offering(cli, Backend::Ffmpeg, true) {
        Ok(f) => f,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, false),
    };
    let start = Instant::now();
    let progress = Progress::new(cli, args.verbose);
    let mut on_event = |e: Event| progress.event(e);
    let result = clips::run(&plan, &ffmpeg, args.overwrite, &mut on_event);
    progress.finish();
    match result {
        Ok(o) => {
            print_clips_success(cli, &o, start.elapsed());
            0
        }
        Err(e) => failed(cli, args, subject, &header, e),
    }
}

/// `--max-size`: the clips are joined first, copied when they match and
/// otherwise encoded near losslessly, into a file beside the output, which
/// is then fitted under the size as `conv joined.mkv out.mp4 --max-size`
/// would fit it. The sizes can only be chosen once the joined file exists,
/// so a size too small to look good is asked about after the join.
fn run_clips_sized(
    cli: &Cli,
    args: &MergeArgs,
    inputs: &[PathBuf],
    output: &Path,
    to: Format,
    ffprobe: &ResolvedBackend,
) -> i32 {
    let subject = json!({ "inputs": inputs, "output": output });
    let header = name_of(output);
    let joined = output.with_file_name(format!(
        ".{}.convkit-joined.mkv",
        output
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    // An encode here is fed to another, so it keeps nearly everything:
    // CRF 16 rather than the usual 20.
    let opts = clips::Options {
        resize: args.resize.clone(),
        crf: None,
        encode_crf: Some(16),
    };
    let plan = match clips::plan_merge(&ffprobe.path, inputs, Format::Mkv, &joined, &opts) {
        Ok(p) => p,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run),
    };
    let max = args.max_size.clone().expect("checked by the caller");
    if args.dry_run {
        let then = format!(
            "Then fitted under {} into {}, as conv {} {} --max-size {} does; the sizes are \
             chosen once the clips are joined.",
            max.spelling,
            name_of(output),
            name_of(&joined),
            name_of(output),
            max.spelling
        );
        print_clips_dry_run(cli, &plan, Some(&then));
        return 0;
    }
    if output.exists() && !args.overwrite {
        let e = ConvError::new(
            ErrorCode::OutputExists,
            format!("{} exists; pass -y to overwrite", output.display()),
        );
        return failed(cli, args, subject, &header, e);
    }
    let ffmpeg = match resolve_offering(cli, Backend::Ffmpeg, true) {
        Ok(f) => f,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, false),
    };
    let start = Instant::now();
    let _cleanup = RemoveOnDrop(joined.clone());
    let progress = Progress::new(cli, args.verbose);
    let mut on_event = |e: Event| progress.event(e);
    let join = clips::run(&plan, &ffmpeg, true, &mut on_event);
    progress.finish();
    let join = match join {
        Ok(j) => j,
        Err(e) => return failed(cli, args, subject, &header, e),
    };

    let resolver = cli.resolver();
    let tuning = convkit_core::Tuning {
        max_size: Some(max),
        ..Default::default()
    };
    // A size too small to look good is asked about, as a conversion asks.
    let mut allow_extreme = false;
    let mut shown: Vec<String> = Vec::new();
    if let Ok(Some(sz)) = sized::preview(Format::Mkv, to, &joined, &tuning, &resolver) {
        if let (Some(warning), true) = (
            sz.warning.clone(),
            sz.choice.as_ref().is_some_and(|c| c.extreme),
        ) {
            if !cli.json {
                let typed: Vec<String> = wild::args_os()
                    .skip(1)
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                eprint!(
                    "{}",
                    render::extreme_warnings_human(
                        &[(joined.as_path(), warning.as_str(), sz.suggested.as_deref())],
                        1,
                        &typed,
                        render::stderr_styled()
                    )
                );
                shown.push(warning.clone());
            }
            let interactive = prompt::is_interactive_session();
            allow_extreme = match prompt::extreme_gate(cli.yes, cli.json, cli.quiet, interactive) {
                Gate::Proceed => true,
                Gate::Ask => prompt::ask("Convert anyway? [y/N] "),
                Gate::Refuse => false,
            };
            if !allow_extreme {
                let e = sized::confirmation_error(output, &sz);
                return pdf_support::print_failure(cli, subject, &header, &e, false);
            }
        }
    }
    let req = convkit_core::Request {
        from: Format::Mkv,
        to,
        inputs: vec![joined.clone()],
        output: output.to_path_buf(),
        overwrite: args.overwrite,
        tuning,
        allow_extreme,
    };
    let progress = Progress::new(cli, args.verbose);
    let mut on_event = |e: Event| progress.event(e);
    let sized = convkit_core::exec::run(&req, &resolver, &mut on_event);
    progress.finish();
    let mut o = match sized {
        Ok(o) => o,
        Err(e) => return failed(cli, args, subject, &header, e),
    };
    o.elapsed_ms = start.elapsed().as_millis() as u64;
    // The join's notes first: they say what went into the file.
    let mut notes = join.notes.clone();
    notes.append(&mut o.warnings);
    o.warnings = notes;
    let mut warnings = join.warnings.clone();
    warnings.append(&mut o.notes);
    o.notes = warnings;
    o.backend_output.splice(0..0, join.backend_output.clone());
    if cli.json {
        let mut v = render::outcome_json(&o);
        v["inputs"] = json!(inputs);
        let envelope = json!({ "ok": true, "results": [v] });
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
        return 0;
    }
    if !cli.quiet {
        print!(
            "{}",
            render::conversion_success_human(&o, render::stdout_styled())
        );
    }
    let shown: Vec<&str> = shown.iter().map(String::as_str).collect();
    eprint!(
        "{}",
        render::conversion_notes_human("", &o, &shown, render::stderr_styled())
    );
    0
}

/// Removes a file when dropped: the joined file `--max-size` fits, on every
/// way out.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn print_clips_dry_run(cli: &Cli, plan: &clips::ClipsPlan, then: Option<&str>) {
    let commands = plan.commands();
    let list = plan.list();
    if cli.json {
        let commands: Vec<Vec<String>> = commands
            .into_iter()
            .map(|argv| std::iter::once("ffmpeg".to_string()).chain(argv).collect())
            .collect();
        let mut notes = plan.notes.clone();
        notes.extend(then.map(str::to_string));
        let v = json!({ "ok": true, "dry_run": true, "plans": [{
            "ok": true,
            "inputs": plan.inputs,
            "output": plan.output,
            "remuxed": plan.copied,
            "duration_ms": plan.duration_ms,
            "commands": commands,
            "list": list.map(|(path, contents)| json!({ "path": path, "contents": contents })),
            "warnings": notes,
            "notes": plan.warnings,
        }]});
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
        return;
    }
    let mut s = String::new();
    if let Some((path, contents)) = list {
        s.push_str(&format!("# {} lists the files to join:\n", path.display()));
        for line in contents.lines() {
            s.push_str(&format!("#   {line}\n"));
        }
    }
    for argv in &commands {
        s.push_str(&render::command_line_human("ffmpeg", argv));
        s.push('\n');
    }
    for w in &plan.warnings {
        s.push_str(&format!("warning: {w}\n"));
    }
    for n in plan.notes.iter().map(String::as_str).chain(then) {
        s.push_str(&format!("note: {n}\n"));
    }
    print!("{s}");
}

fn print_clips_success(cli: &Cli, o: &clips::ClipsOutcome, elapsed: std::time::Duration) {
    if cli.json {
        let mut v = json!({
            "ok": true,
            "inputs": o.inputs,
            "output": o.output,
            "bytes": o.bytes,
            "duration_ms": o.duration_ms,
            "remuxed": o.copied,
            "warnings": o.notes,
            "notes": o.warnings,
            "backend_output": o.backend_output,
            "elapsed_ms": elapsed.as_millis() as u64,
            "backends": [{ "backend": Backend::Ffmpeg, "version": o.version }],
        });
        if let Some((w, h)) = o.shape.size {
            v["width"] = json!(w);
            v["height"] = json!(h);
        }
        if let Some((n, d)) = o.shape.rate {
            v["fps"] = json!(format!("{n}/{d}"));
        }
        if let Some(sr) = o.shape.sample_rate {
            v["sample_rate"] = json!(sr);
        }
        let envelope = json!({ "ok": true, "results": [v] });
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
        return;
    }
    if !cli.quiet {
        print!(
            "{}",
            render::clips_success_human(o, elapsed, render::stdout_styled())
        );
    }
    let styled = render::stderr_styled();
    for w in &o.warnings {
        eprintln!(
            "{}",
            render::paint_warning(&format!("warning  {w}"), styled)
        );
    }
}

/// A failed run, with the `try` line for an output that already exists.
fn failed(
    cli: &Cli,
    args: &MergeArgs,
    subject: serde_json::Value,
    header: &str,
    mut e: ConvError,
) -> i32 {
    if e.code == ErrorCode::OutputExists {
        e.remediation = existing_output_fix(&args.paths).map(|manual| Remediation {
            managed: None,
            manual: Some(manual),
        });
    }
    pdf_support::print_failure(cli, subject, header, &e, false)
}

/// `backend`, resolved the usual way. When it is missing and `offer` is
/// set, the same install prompt a conversion gives (`--yes` installs,
/// `--no-install`, `--json` and `-q` never prompt).
fn resolve_offering(
    cli: &Cli,
    backend: Backend,
    offer: bool,
) -> Result<ResolvedBackend, ConvError> {
    match cli.resolver().resolve(backend) {
        Ok(b) => Ok(b),
        Err(e)
            if e.code == ErrorCode::BackendMissing
                && offer
                && install_prompt::should_install(cli, backend) =>
        {
            install::install_backend(cli, backend)?;
            cli.resolver().resolve(backend)
        }
        Err(e) => Err(e),
    }
}

/// A spinner naming the backend running, for a single merge that can take
/// a while (an encode), and `-v`'s command lines and backend output. No
/// spinner under `--quiet` or `--json`, or on a console that cannot redraw
/// one in place.
struct Progress {
    spinner: Option<indicatif::ProgressBar>,
    verbose: bool,
}

impl Progress {
    fn new(cli: &Cli, verbose: bool) -> Progress {
        let spinner = (!cli.quiet && !cli.json && render::ansi_supported()).then(|| {
            let pb = indicatif::ProgressBar::new_spinner();
            pb.set_style(
                indicatif::ProgressStyle::default_spinner()
                    .template("{spinner} {msg}")
                    .expect("static template is valid"),
            );
            pb.enable_steady_tick(std::time::Duration::from_millis(100));
            pb.set_message("starting…");
            pb
        });
        Progress { spinner, verbose }
    }

    fn say(&self, line: &str) {
        match &self.spinner {
            Some(pb) => pb.suspend(|| eprintln!("{line}")),
            None => eprintln!("{line}"),
        }
    }

    fn event(&self, event: Event) {
        match event {
            Event::StepStarted { backend, .. } => {
                if let Some(pb) = &self.spinner {
                    pb.set_message(format!("running {}…", backend.exe_name()));
                }
            }
            Event::StepSpawned { program, argv, .. } if self.verbose => {
                self.say(&format!(
                    "+ {}",
                    render::command_line_human(&program.to_string_lossy(), &argv)
                ));
            }
            Event::StepReport {
                backend, report, ..
            } if self.verbose => {
                self.say(render::verbose_report_human(backend.exe_name(), &report).trim_end());
            }
            _ => {}
        }
    }

    fn finish(&self) {
        if let Some(pb) = &self.spinner {
            pb.finish_and_clear();
        }
    }
}

fn invalid(message: String) -> ConvError {
    ConvError::new(ErrorCode::InvalidInvocation, message)
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

const SHAPE: &str = "expected files and then an output, e.g. `conv merge a.pdf b.pdf out.pdf` or \
     `conv merge a.mp4 b.mp4 out.mp4`";

/// Splits the typed paths into inputs and the output, expanding globs
/// (never the output's) and folders, and checking every input goes into
/// what the output makes. Whether the inputs exist is checked by the
/// caller, before any backend is looked for.
fn split_paths(typed: &[PathBuf]) -> Result<(Vec<PathBuf>, PathBuf, Format), ConvError> {
    let positionals = input::expand_globs(typed, typed.len().saturating_sub(1));
    let [raw_inputs @ .., output] = positionals.as_slice() else {
        return Err(invalid(SHAPE.into()));
    };
    if raw_inputs.is_empty() {
        return Err(invalid(SHAPE.into()));
    }
    if output.is_dir() {
        let ext = raw_inputs
            .iter()
            .find_map(|p| Format::from_path(p))
            .and_then(|f| family_of(f).map(|_| f.ext()))
            .unwrap_or("pdf");
        return Err(invalid(format!(
            "{} is a folder; name the merged file, e.g. {}",
            output.display(),
            output.join(format!("merged.{ext}")).display()
        )));
    }
    let Some((to, family)) = Format::from_path(output).and_then(|f| Some((f, family_of(f)?)))
    else {
        let hint = match Format::from_path(output) {
            Some(Format::Gif) => "; for a GIF, merge into an mp4 and then conv merged.mp4 .gif",
            Some(Format::Avi) => "; conv reads avi but does not write it",
            _ => "",
        };
        return Err(invalid(format!(
            "the merged file must be a .pdf, a video (.mp4, .mov, .mkv, .webm) or an audio \
             file (.mp3, .m4a, .wav, .flac, .opus, .ogg), got {}{hint}",
            name_of(output)
        )));
    };

    let mut inputs = Vec::new();
    for p in raw_inputs {
        if p.is_dir() {
            let found = input::files_in(p, output, |f| family.takes_from_folder(f))?;
            if found.is_empty() {
                return Err(invalid(format!(
                    "{} has no {}",
                    p.display(),
                    family.files()
                )));
            }
            inputs.extend(found);
            continue;
        }
        match Format::from_path(p) {
            Some(f) if family.takes(f) => inputs.push(p.clone()),
            other => return Err(not_taken(p, other, family, to, typed)),
        }
    }
    Ok((inputs, output.clone(), to))
}

/// Why `p` can't go into a merge of `family`, with what would. `typed` is
/// the command's paths, for the one to try instead.
fn not_taken(
    p: &Path,
    f: Option<Format>,
    family: Family,
    to: Format,
    typed: &[PathBuf],
) -> ConvError {
    let name = name_of(p);
    let mut e = match family {
        Family::Pdf => invalid(format!(
            "a merge into a PDF takes PDFs and pictures; {name} is neither"
        )),
        Family::Video => invalid(format!(
            "a merge into {} takes videos; {name} is not one",
            to.ext()
        )),
        Family::Audio => invalid(format!(
            "a merge into {} takes audio and video files; {name} is neither",
            to.ext()
        )),
    };
    let fix = match (family, f.map(|f| f.kind())) {
        (Family::Pdf, Some(Kind::Document)) => {
            Some(format!("convert it to a PDF first: conv {name} .pdf"))
        }
        (Family::Video, Some(Kind::Audio)) => {
            let shown: Vec<String> = typed.iter().map(|p| p.display().to_string()).collect();
            let [inputs @ .., output] = shown.as_slice() else {
                return e;
            };
            let listed = if inputs.len() > 3 {
                format!("{} ... {}", inputs[0], inputs[inputs.len() - 1])
            } else {
                inputs.join(" ")
            };
            let sound = Path::new(output).with_extension("m4a");
            Some(format!(
                "to join the sound alone: conv merge {listed} {}",
                sound.display()
            ))
        }
        _ => None,
    };
    e.remediation = fix.map(|manual| Remediation {
        managed: None,
        manual: Some(manual),
    });
    e
}

/// The `try` line when the output already exists. The usual cause is a
/// glob that ended in an existing file (`conv merge *.pdf`), so it suggests
/// the same command with `merged.EXT` as the output. `-y` is already named
/// in the message itself. A typed folder may have supplied the existing
/// file as an input, so then there is no suggestion.
fn existing_output_fix(typed: &[PathBuf]) -> Option<String> {
    if typed.iter().any(|p| p.is_dir()) {
        return None;
    }
    let shown: Vec<String> = typed.iter().map(|p| p.display().to_string()).collect();
    let last = shown.last().cloned().unwrap_or_default();
    let ext = typed
        .last()
        .and_then(|p| Format::from_path(p))
        .map_or("pdf", |f| f.ext());
    let listed = if shown.len() > 4 {
        format!("{} ... {last}", shown[0])
    } else {
        shown.join(" ")
    };
    Some(format!(
        "conv merge {listed} merged.{ext} (if {last} was meant as an input)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: &[&str]) -> Vec<PathBuf> {
        v.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn the_last_path_is_the_output_and_names_what_is_made() {
        let (inputs, output, to) = split_paths(&p(&["a.pdf", "b.pdf", "out.pdf"])).unwrap();
        assert_eq!(inputs, p(&["a.pdf", "b.pdf"]));
        assert_eq!((output, to), (PathBuf::from("out.pdf"), Format::Pdf));
        let (_, _, to) = split_paths(&p(&["a.mp4", "b.mov", "c.gif", "out.mkv"])).unwrap();
        assert_eq!(to, Format::Mkv);
        let (_, _, to) = split_paths(&p(&["a.mp3", "talk.mp4", "album.m4a"])).unwrap();
        assert_eq!(to, Format::M4a);
        let (inputs, _, _) = split_paths(&p(&["scan.heic", "b.pdf", "c.png", "out.pdf"])).unwrap();
        assert_eq!(inputs.len(), 3, "pictures go in among the PDFs");
    }

    #[test]
    fn shape_errors_say_what_to_type() {
        let e = split_paths(&p(&["a.pdf"])).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert_eq!(e.message, SHAPE);
        let e = split_paths(&p(&["a.pdf", "b.pdf", "out.docx"])).unwrap_err();
        assert!(
            e.message
                .starts_with("the merged file must be a .pdf, a video"),
            "{}",
            e.message
        );
        assert!(e.message.ends_with("got out.docx"), "{}", e.message);
        let e = split_paths(&p(&["a.mp4", "b.mp4", "out.gif"])).unwrap_err();
        assert!(
            e.message.ends_with(
                "got out.gif; for a GIF, merge into an mp4 and then conv merged.mp4 .gif"
            ),
            "{}",
            e.message
        );
        let e = split_paths(&p(&["notes.docx", "b.pdf", "out.pdf"])).unwrap_err();
        assert_eq!(
            e.message,
            "a merge into a PDF takes PDFs and pictures; notes.docx is neither"
        );
        assert_eq!(
            e.remediation.unwrap().manual.unwrap(),
            "convert it to a PDF first: conv notes.docx .pdf"
        );
        let e = split_paths(&p(&["a.mp4", "song.mp3", "out.mp4"])).unwrap_err();
        assert_eq!(
            e.message,
            "a merge into mp4 takes videos; song.mp3 is not one"
        );
        assert_eq!(
            e.remediation.unwrap().manual.unwrap(),
            "to join the sound alone: conv merge a.mp4 song.mp3 out.m4a"
        );
        let e = split_paths(&p(&["a.mp4", "b.pdf", "out.mp3"])).unwrap_err();
        assert_eq!(
            e.message,
            "a merge into mp3 takes audio and video files; b.pdf is neither"
        );
    }

    #[test]
    fn a_folder_adds_what_the_output_takes_and_an_empty_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let scans = dir.path().join("scans");
        std::fs::create_dir(&scans).unwrap();
        let e = split_paths(&[scans.clone(), dir.path().join("out.pdf")]).unwrap_err();
        assert_eq!(
            e.message,
            format!("{} has no PDFs or pictures", scans.display())
        );
        for name in ["p2.pdf", "p10.jpg", "all.pdf", "notes.txt", "clip.mp4"] {
            std::fs::write(scans.join(name), b"x").unwrap();
        }
        let (inputs, _, _) = split_paths(&[scans.clone(), scans.join("all.pdf")]).unwrap();
        assert_eq!(inputs, vec![scans.join("p2.pdf"), scans.join("p10.jpg")]);
        let (inputs, _, _) = split_paths(&[scans.clone(), dir.path().join("all.mp4")]).unwrap();
        assert_eq!(inputs, vec![scans.join("clip.mp4")]);
        let e = split_paths(&[scans.clone(), dir.path().join("all.mp3")]).unwrap_err();
        assert_eq!(e.message, format!("{} has no audio files", scans.display()));
    }

    #[test]
    fn password_is_refused_where_it_locks_nothing() {
        let args = MergeArgs {
            paths: vec![],
            overwrite: false,
            dry_run: false,
            verbose: false,
            resize: None,
            crf: None,
            max_size: None,
            lock: crate::cli::LockArgs {
                password: true,
                restrict: vec![],
            },
        };
        let clips = [PathBuf::from("a.mp4"), PathBuf::from("b.mp4")];
        let e = refused_lock(&args, Family::Video, &clips, Path::new("o.mp4")).unwrap();
        assert_eq!(e.message, "--password locks a PDF, and o.mp4 is not one");
        let photos = [PathBuf::from("a.jpg"), PathBuf::from("b.png")];
        let e = refused_lock(&args, Family::Pdf, &photos, Path::new("o.pdf")).unwrap();
        assert!(e
            .message
            .starts_with("--password locks a merge with a PDF in it"));
        let mixed = [PathBuf::from("a.jpg"), PathBuf::from("b.pdf")];
        assert!(refused_lock(&args, Family::Pdf, &mixed, Path::new("o.pdf")).is_none());
    }

    #[test]
    fn knobs_are_refused_where_they_mean_nothing() {
        let args = |resize: Option<&str>, max: bool| MergeArgs {
            paths: vec![],
            overwrite: false,
            dry_run: false,
            verbose: false,
            resize: resize.map(str::to_string),
            crf: None,
            max_size: max.then(|| convkit_core::size::parse("10mb").unwrap()),
            lock: Default::default(),
        };
        let e = refused_knob(&args(Some("50%"), false), Family::Pdf, Path::new("o.pdf")).unwrap();
        assert_eq!(
            e.message,
            "--resize works on a merge of videos, not on o.pdf"
        );
        let e = refused_knob(&args(None, true), Family::Audio, Path::new("o.mp3")).unwrap();
        assert_eq!(
            e.message,
            "--max-size works on a merge of videos; o.mp3 holds only sound"
        );
        assert!(
            refused_knob(&args(Some("50%"), true), Family::Video, Path::new("o.mp4")).is_none()
        );
        assert!(refused_knob(&args(None, false), Family::Pdf, Path::new("o.pdf")).is_none());
    }

    #[test]
    fn the_existing_output_fix_suggests_a_new_name() {
        assert_eq!(
            existing_output_fix(&p(&["a.pdf", "b.pdf", "report.pdf"])).as_deref(),
            Some("conv merge a.pdf b.pdf report.pdf merged.pdf (if report.pdf was meant as an input)")
        );
        assert_eq!(
            existing_output_fix(&p(&["a.pdf", "b.pdf", "c.pdf", "d.pdf", "report.pdf"])).as_deref(),
            Some(
                "conv merge a.pdf ... report.pdf merged.pdf (if report.pdf was meant as an input)"
            )
        );
        assert_eq!(
            existing_output_fix(&p(&["a.mp4", "trip.mp4"])).as_deref(),
            Some("conv merge a.mp4 trip.mp4 merged.mp4 (if trip.mp4 was meant as an input)")
        );
    }

    #[test]
    fn a_typed_folder_gets_no_suggestion() {
        let dir = tempfile::tempdir().unwrap();
        let scans = dir.path().join("scans");
        std::fs::create_dir(&scans).unwrap();
        let typed = [scans.clone(), scans.join("all.pdf")];
        assert_eq!(existing_output_fix(&typed), None);
    }
}
