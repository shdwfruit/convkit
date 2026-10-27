use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use convkit_core::{
    plan, probe, registry, sized, AvailableBackends, Backend, ConvError, ErrorCode, Format,
    MediaProbe, Outcome, Resolver, Tuning,
};
use serde_json::json;

use crate::batch;
use crate::cli::Cli;
use crate::clipboard::{self, Got};
use crate::commands::install;
use crate::input;
use crate::install_prompt;
use crate::output;
use crate::paste::{self, PASTED};
use crate::prompt::{self, Gate};
use crate::render;

pub fn run(cli: &Cli) -> i32 {
    if !cli.paste {
        return convert(cli, None);
    }
    match pasted(cli) {
        // The folder holding a pasted picture lives until the run is over.
        Ok(Pasted::Inputs(run, folder)) => {
            let picture = folder.as_ref().map(|_| run.paths[0].clone());
            convert(&run, picture.as_deref())
        }
        Ok(Pasted::Saved(outcome)) => {
            let copied = cli
                .copy
                .then(|| copy_outputs(std::slice::from_ref(&outcome.output)));
            if cli.json {
                let mut v = render::outcome_json(&outcome);
                v["input"] = json!(PASTED);
                let mut envelope = json!({ "ok": true, "results": [v] });
                if let Some(c) = &copied {
                    envelope["clipboard"] = c.json();
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
                    c.print(cli);
                }
            }
            0
        }
        Ok(Pasted::DryRun(path, bytes)) => {
            if cli.json {
                let v = json!({ "dry_run": true, "input": PASTED, "output": path, "bytes": bytes });
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else {
                println!("would save {PASTED} as {} ({bytes} bytes)", path.display());
            }
            0
        }
        Err(e) => {
            render::print_error(cli.json, &e);
            e.code.exit_code()
        }
    }
}

/// What `--paste` made of the clipboard.
enum Pasted {
    /// The run to do, with the clipboard's files or picture as its inputs,
    /// and the folder a pasted picture was put in.
    Inputs(Box<Cli>, Option<tempfile::TempDir>),
    /// A picture saved as it was: `conv --paste` with nothing to convert.
    Saved(Box<convkit_core::Outcome>),
    /// What `Saved` would write, under --dry-run: the path and its size.
    DryRun(PathBuf, usize),
}

fn pasted(cli: &Cli) -> Result<Pasted, ConvError> {
    let refused = |message: String| ConvError::new(ErrorCode::InvalidInvocation, message);
    let got = paste::get()?;
    let mut run = cli.clone();
    run.paste = false;
    let (format, bytes) = match got {
        Got::Files(files) => {
            let nothing_asked =
                cli.paths.is_empty() && cli.to.is_none() && !input::keeps_own_format(cli);
            if nothing_asked {
                let held = match files.as_slice() {
                    [one] => one
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    many => format!("{} files", many.len()),
                };
                let formats: Vec<Option<Format>> =
                    files.iter().map(|f| Format::from_path(f)).collect();
                return Err(say_what(&held, &formats));
            }
            run.paths = files.into_iter().chain(cli.paths.iter().cloned()).collect();
            return Ok(Pasted::Inputs(Box::new(run), None));
        }
        Got::Text(_) => {
            return Err(refused(
                "the clipboard holds text, not a picture or files".to_string(),
            ))
        }
        Got::Picture { format, bytes } => (format, bytes),
    };
    let name = format!("clipboard.{}", format.ext());
    // What was asked for: a named output, a bare `.ext`, `--to`, or nothing.
    let named = cli
        .paths
        .first()
        .filter(|p| !input::is_extension_shorthand(p));
    let target = match (named, cli.paths.first(), cli.to.as_deref()) {
        (Some(p), _, _) => Format::from_path(p),
        (None, Some(ext), _) => Format::from_path(Path::new(&format!("x{}", ext.display()))),
        (None, None, Some(to)) => Format::from_ext(to),
        (None, None, None) => None,
    };
    let plain = !input::keeps_own_format(cli) && cli.tuning() == Tuning::default();
    // A flag that keeps the format (`--max-size`, `--resize` alone, ...)
    // makes a lone input a whole job; any other knob needs a target.
    let keeps = input::keeps_own_format(cli);
    if target.is_none() && cli.paths.is_empty() && cli.to.is_none() && !keeps && !plain {
        let held = format!("a {} picture", format.ext().to_uppercase());
        return Err(say_what(&held, &[Some(format)]));
    }
    // Nothing to change: the picture is saved as it came, under the name
    // asked for or as clipboard.EXT.
    if plain && target.is_none_or(|t| t == format) {
        let path = match named {
            Some(p) => input::in_outdir(p, cli.outdir.as_deref())?,
            None => input::in_outdir(Path::new(&name), cli.outdir.as_deref())?,
        };
        if path.exists() && !cli.overwrite {
            return Err(ConvError::new(
                ErrorCode::OutputExists,
                format!("{} exists; pass -y to overwrite", path.display()),
            ));
        }
        if cli.dry_run {
            return Ok(Pasted::DryRun(path, bytes.len()));
        }
        output::ensure_dir(cli.outdir.as_deref())?;
        return Ok(Pasted::Saved(Box::new(output::write_file(&path, &bytes)?)));
    }
    let kept = paste::keep(format, &bytes)?;
    run.paths = std::iter::once(kept.path.clone())
        .chain(cli.paths.iter().cloned())
        .collect();
    Ok(Pasted::Inputs(Box::new(run), kept.folder))
}

/// The refusal when `--paste` was given nothing to make: what the
/// clipboard's `formats` can all become, and the command typed again with
/// one of them.
fn say_what(held: &str, formats: &[Option<Format>]) -> ConvError {
    let pairs = registry::all_pairs();
    let can = |from: &Option<Format>| -> Vec<Format> {
        from.map(|f| pairs.iter().filter(|p| p.0 == f).map(|p| p.1).collect())
            .unwrap_or_default()
    };
    let mut targets = formats.first().map(can).unwrap_or_default();
    for f in formats.iter().skip(1) {
        let theirs = can(f);
        targets.retain(|t| theirs.contains(t));
    }
    let Some(example) = [
        Format::Jpg,
        Format::Png,
        Format::Mp4,
        Format::Mp3,
        Format::Pdf,
    ]
    .into_iter()
    .find(|f| targets.contains(f))
    .or(targets.first().copied()) else {
        let why = if formats.len() > 1 {
            "conv can't make them all into one format"
        } else {
            "conv can't convert it"
        };
        return ConvError::new(
            ErrorCode::UnsupportedPair,
            format!("the clipboard holds {held}, and {why}"),
        );
    };
    let list: Vec<&str> = targets.iter().map(|t| t.ext()).collect();
    let typed: Vec<String> = typed_args()
        .into_iter()
        .map(|a| if a.contains(' ') { format!("'{a}'") } else { a })
        .collect();
    ConvError {
        code: ErrorCode::InvalidInvocation,
        message: format!(
            "the clipboard holds {held}, which can become {}; say which",
            list.join(", ")
        ),
        backend: None,
        remediation: Some(convkit_core::Remediation {
            managed: None,
            manual: Some(format!(
                "e.g. `conv {} .{}`",
                typed.join(" "),
                example.ext()
            )),
        }),
    }
}

/// How `--copy` went.
pub(crate) struct CopyReport {
    ok: bool,
    message: String,
}

impl CopyReport {
    pub(crate) fn json(&self) -> serde_json::Value {
        json!({ "ok": self.ok, "message": self.message })
    }

    /// On stderr: a failure even under --quiet, as the file was written
    /// but what was asked for didn't all happen.
    pub(crate) fn print(&self, cli: &Cli) {
        if !self.ok {
            let line = format!("warning  {}", self.message);
            eprintln!("{}", render::paint_warning(&line, render::stderr_styled()));
        } else if !cli.quiet {
            eprintln!("{}", self.message);
        }
    }
}

pub(crate) fn copy_outputs(outputs: &[PathBuf]) -> CopyReport {
    let put = match clipboard::put_for(outputs) {
        Ok(put) => put,
        Err(e) => {
            return CopyReport {
                ok: false,
                message: format!("couldn't copy it: {e}"),
            }
        }
    };
    match clipboard::put(&put) {
        Ok(how) => CopyReport {
            ok: true,
            message: clipboard::copied_note(&put, &how),
        },
        Err(e) => CopyReport {
            ok: false,
            message: format!("couldn't copy to the clipboard: {e}"),
        },
    }
}

/// `e` with the pasted picture's passing path, in either spelling (macOS
/// gives /var as /private/var), named as `PASTED`.
fn tidied(e: &ConvError, pasted: Option<&Path>) -> ConvError {
    let mut e = e.clone();
    if let Some(p) = pasted {
        let real = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        for spelling in [real, p.to_path_buf()] {
            e.message = e.message.replace(&spelling.display().to_string(), PASTED);
        }
    }
    e
}

/// Warnings already shown, keyed as the results will be: by `PASTED` for
/// the pasted picture.
fn renamed(shown: Vec<(PathBuf, String)>, pasted: Option<&Path>) -> Vec<(PathBuf, String)> {
    match pasted {
        Some(_) => shown
            .into_iter()
            .map(|(_, w)| (PathBuf::from(PASTED), w))
            .collect(),
        None => shown,
    }
}

/// Runs a conversion. `pasted` is the passing path of a picture `--paste`
/// took from the clipboard.
fn convert(cli: &Cli, pasted: Option<&Path>) -> i32 {
    let mut jobs = match input::plan_jobs(cli) {
        Ok(jobs) => jobs,
        Err(e) => {
            render::print_error(cli.json, &tidied(&e, pasted));
            return e.code.exit_code();
        }
    };
    // Nothing is written into a pasted picture's passing folder: what would
    // go beside it goes here, named as it would have been there.
    if let Some(folder) = pasted.and_then(Path::parent) {
        for job in &mut jobs {
            if job.output.starts_with(folder) {
                job.output = PathBuf::from(job.output.file_name().unwrap_or_default());
            }
        }
    }

    if cli.dry_run {
        return dry_run(&jobs, cli);
    }

    let (allow_extreme, mut shown) = match confirm_extreme(&jobs, cli) {
        Ok(c) => (c.allowed || cli.yes, renamed(c.shown, pasted)),
        Err(e) => {
            render::print_error(cli.json, &tidied(&e, pasted));
            return e.code.exit_code();
        }
    };

    // Kept alongside `results` so a retry below can re-run exactly the jobs
    // that failed on a missing backend — `JobResult` alone has nothing to
    // hand back to `batch::run`, only what came out of it.
    let original_jobs = jobs.clone();
    let (mut results, mut code, mut elapsed) = batch::run(jobs, cli, allow_extreme);

    // --- Part 1: offer to install a missing backend, then retry once -----
    //
    // `missing_backend` only returns a backend when every `backend_missing`
    // failure in this batch names the *same* one — a mixed batch (one job
    // missing ffmpeg, another missing magick) gets no prompt at all, since
    // installing one would silently leave the other's failure unexplained.
    // `install_prompt::should_install` is what actually enforces every hard
    // gate (no managed build, `--json`/`--quiet`, `--no-install`, a
    // non-interactive session) before ever asking a question.
    if let Some(backend) = missing_backend(&results) {
        if install_prompt::should_install(cli, backend) {
            let redo_start = Instant::now();
            match install::install_backend(cli, backend) {
                Ok(_) => {
                    let retry_indices: Vec<usize> = results
                        .iter()
                        .enumerate()
                        .filter(|(_, r)| failed_on_missing_backend(&r.result, backend))
                        .map(|(i, _)| i)
                        .collect();
                    let retry_jobs: Vec<input::Job> = retry_indices
                        .iter()
                        .map(|&i| original_jobs[i].clone())
                        .collect();
                    // An answer already given stands for the retried jobs
                    // too. Only when the first pass could not preview (it
                    // needs ffprobe, which may be exactly what was just
                    // installed) is there nothing to reuse, so ask now.
                    let allow = if allow_extreme {
                        true
                    } else {
                        match confirm_extreme(&retry_jobs, cli) {
                            Ok(c) => {
                                shown.extend(renamed(c.shown, pasted));
                                c.allowed
                            }
                            Err(e) => {
                                render::print_error(cli.json, &tidied(&e, pasted));
                                print_results(&results, cli, elapsed, &shown, None);
                                return e.code.exit_code();
                            }
                        }
                    };
                    let (retry_results, _, _) = batch::run(retry_jobs, cli, allow);
                    for (idx, new_result) in retry_indices.into_iter().zip(retry_results) {
                        results[idx] = new_result;
                    }
                    code = batch::exit_code(&results);
                    // Counts the install download plus the retry itself,
                    // deliberately excluding the time spent waiting on the
                    // user's own y/n keypress — that's think time, not
                    // work, and would make "did it hang?" unanswerable in
                    // the one place this number matters most.
                    elapsed += redo_start.elapsed();
                }
                Err(e) => {
                    // The install itself failed (network, checksum, ...).
                    // "On no ... exit with today's error ... unchanged"
                    // extends naturally to "the install didn't work
                    // either": report that failure too, but leave
                    // `results`/`code` exactly as the original
                    // `backend_missing` failure already reported them.
                    render::print_error(cli.json, &tidied(&e, pasted));
                    elapsed += redo_start.elapsed();
                }
            }
        }
    }

    if let Some(picture) = pasted {
        for r in &mut results {
            r.input = PathBuf::from(PASTED);
            if let Err(e) = &mut r.result {
                *e = tidied(e, Some(picture));
            }
        }
    }
    let outputs: Vec<PathBuf> = results
        .iter()
        .filter_map(|r| r.result.as_ref().ok().map(|o| o.output.clone()))
        .collect();
    let copied = (cli.copy && !outputs.is_empty()).then(|| copy_outputs(&outputs));
    print_results(&results, cli, elapsed, &shown, copied.as_ref());
    code
}

/// What `confirm_extreme` decided, and what it printed.
#[derive(Debug, Default)]
struct Confirmation {
    /// Extreme jobs may run. False when nothing extreme was found (a preview
    /// that fails is not an answer; the real run reports that failure
    /// itself).
    allowed: bool,
    /// The warning printed for each extreme job, by input, before it ran.
    /// Its result carries the same sentence as a note, which the human
    /// output does not print twice.
    shown: Vec<(PathBuf, String)>,
}

/// One job the question is about: its input, the core's warning, and what
/// to try instead, when there is something.
struct Asked<'a> {
    job: &'a input::Job,
    warning: String,
    suggested: Option<String>,
    /// The `--max-size` plan, whose refusal names the size to try.
    sizing: Option<sized::SizingPlan>,
}

/// Previews every job a question can be about -- an extreme `--max-size`
/// target, or a `--resize --upscale` past four times the source's pixels --
/// and, when any is, prints why and asks once for the whole batch. The two
/// never meet in one run: `--upscale` and `--max-size` exclude each other.
/// `Err`: not confirmed, so nothing may run.
fn confirm_extreme(jobs: &[input::Job], cli: &Cli) -> Result<Confirmation, ConvError> {
    let resolver = cli.resolver();
    let tuning = cli.tuning();
    // Cannot run, and `batch::run` will say so; not worth a question.
    let runnable = |job: &&input::Job| !job.output.exists() || cli.overwrite;
    let sizing = cli.max_size.is_some();
    let asked: Vec<Asked> = if sizing {
        jobs.iter()
            .filter(runnable)
            .filter_map(|job| {
                let sz = sized::preview(job.from, job.to, &job.inputs[0], &tuning, &resolver)
                    .ok()
                    .flatten()?;
                // The core words every extreme choice; a plan with nothing
                // to say has nothing to ask about.
                let warning = sz.warning.clone()?;
                sz.choice
                    .as_ref()
                    .is_some_and(|c| c.extreme)
                    .then(|| Asked {
                        job,
                        warning,
                        suggested: sz.suggested.clone(),
                        sizing: Some(sz.clone()),
                    })
            })
            .collect()
    } else if tuning.upscale {
        jobs.iter()
            .filter(runnable)
            .filter_map(|job| {
                let probed = probed_for(&resolver, job, &tuning).ok().flatten();
                let plan = plan::build_tuned(
                    job.from,
                    job.to,
                    &job.inputs,
                    &job.output,
                    probed.as_ref(),
                    None,
                    &tuning,
                )
                .ok()?;
                let e = plan.enlarged.filter(|e| e.needs_confirmation)?;
                Some(Asked {
                    job,
                    warning: e.warning,
                    suggested: None,
                    sizing: None,
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    if asked.is_empty() {
        return Ok(Confirmation::default());
    }
    let mut shown = Vec::new();
    if !cli.json {
        let args = typed_args();
        let entries: Vec<(&Path, &str, Option<&str>)> = asked
            .iter()
            .map(|a| {
                (
                    a.job.inputs[0].as_path(),
                    a.warning.as_str(),
                    a.suggested.as_deref(),
                )
            })
            .collect();
        eprint!(
            "{}",
            render::extreme_warnings_human(&entries, jobs.len(), &args, render::stderr_styled())
        );
        shown = asked
            .iter()
            .map(|a| (a.job.inputs[0].clone(), a.warning.clone()))
            .collect();
    }
    let allowed = Confirmation {
        allowed: true,
        shown,
    };
    let what = if sizing {
        "extreme compression"
    } else {
        "large upscale"
    };
    let refusal = || {
        if let ([only], 1) = (asked.as_slice(), jobs.len()) {
            return match &only.sizing {
                Some(sz) => sized::confirmation_error(&only.job.inputs[0], sz),
                None => convkit_core::upscale_confirmation_error(&only.job.inputs),
            };
        }
        ConvError::new(
            ErrorCode::ConfirmationRequired,
            format!(
                "{what} not confirmed for {} of {} files; pass --yes to convert anyway",
                asked.len(),
                jobs.len()
            ),
        )
    };
    match prompt::extreme_gate(
        cli.yes,
        cli.json,
        cli.quiet,
        prompt::is_interactive_session(),
    ) {
        Gate::Proceed => Ok(allowed),
        Gate::Refuse => Err(refusal()),
        Gate::Ask => {
            let question = if jobs.len() == 1 {
                "Convert anyway? [y/N] ".to_string()
            } else if sizing {
                format!(
                    "{} of {} conversions are extreme. Convert anyway? [y/N] ",
                    asked.len(),
                    jobs.len()
                )
            } else {
                format!(
                    "{} of {} conversions enlarge past four times their source's pixels. \
                     Convert anyway? [y/N] ",
                    asked.len(),
                    jobs.len()
                )
            };
            if prompt::ask(&question) {
                Ok(allowed)
            } else {
                Err(refusal())
            }
        }
    }
}

/// The single backend to offer installing, given this batch's results —
/// `None` when nothing failed with `backend_missing` at all, or when more
/// than one distinct backend is implicated (see `run`'s own doc comment for
/// why a mixed batch gets no prompt).
fn missing_backend(results: &[batch::JobResult]) -> Option<Backend> {
    let mut found: Option<Backend> = None;
    for r in results {
        let Err(e) = &r.result else { continue };
        if e.code != ErrorCode::BackendMissing {
            continue;
        }
        let Some(b) = e.backend else { continue };
        match found {
            None => found = Some(b),
            Some(f) if f == b => {}
            Some(_) => return None,
        }
    }
    found
}

/// Whether this job's own result is precisely "failed because `backend` was
/// missing" — the set of jobs a successful install is worth retrying.
fn failed_on_missing_backend(result: &Result<Outcome, ConvError>, backend: Backend) -> bool {
    matches!(result, Err(e) if e.code == ErrorCode::BackendMissing && e.backend == Some(backend))
}

/// Probes the input when, and only when, this pair might be satisfiable by a
/// stream copy, or a video knob needs a source to cap against — mirrors
/// `exec::run`'s own gate exactly (`registry::needs_probe_tuned`), so
/// `--dry-run` and the real run it previews always agree on whether a probe
/// happens at all. Where a probe is optional, `ffprobe` missing or the probe
/// itself failing is swallowed into `None` here exactly as it is in
/// `exec::run`: both conservatively fall back to a transcode preview rather
/// than turning "no ffprobe" into its own dry-run failure mode. Where a knob
/// can *only* be honoured with a probe (`registry::requires_probe`), that
/// failure is the preview's real answer, as it is `exec::run`'s.
///
/// An image source is also read for the notes that depend on it (a
/// jpg/png/bmp target's frame and alpha notes, and the location note), as
/// `exec::run` does, so a preview and `--json` carry the same notes as the
/// conversion.
fn probed_for(
    resolver: &Resolver,
    job: &input::Job,
    tuning: &Tuning,
) -> Result<Option<MediaProbe>, ConvError> {
    let mut probed = media_probed_for(resolver, job, tuning)?;
    if registry::notes_need_image(job.from, job.to, tuning) {
        let traits = resolver
            .resolve(Backend::Magick)
            .ok()
            .and_then(|m| probe::image_traits(&m.path, &job.inputs).ok());
        if let Some(t) = traits {
            probed.get_or_insert_with(MediaProbe::default).image = Some(t);
        }
    }
    // As `exec::run` does, so the preview refuses a password-protected
    // .docx and carries the same macro note.
    if registry::reads_office(job.from, job.to) {
        if let Some(t) = convkit_core::office::traits(&job.inputs[0]) {
            probed.get_or_insert_with(MediaProbe::default).office = Some(t);
        }
    }
    // As `exec::run` does, so the preview reads the same .ico frame.
    if registry::reads_frames(job.from, job.to) {
        let frames = resolver
            .resolve(Backend::Magick)
            .ok()
            .and_then(|m| probe::frame_sizes(&m.path, &job.inputs[0]).ok());
        if let Some(f) = frames {
            probed.get_or_insert_with(MediaProbe::default).frames = f;
        }
    }
    // As `exec::run` does, so the preview shows the import options a real
    // run uses, and the same notes.
    if registry::reads_table(job.from, job.to) {
        if let Some(t) = convkit_core::table::read(&job.inputs[0], job.from) {
            probed.get_or_insert_with(MediaProbe::default).table = Some(t);
        }
    }
    Ok(probed)
}

/// `probed_for` before the image read for notes: the media probe, or an
/// `--upscale` size read.
fn media_probed_for(
    resolver: &Resolver,
    job: &input::Job,
    tuning: &Tuning,
) -> Result<Option<MediaProbe>, ConvError> {
    // A knob that can only be honoured with a probe: a missing ffprobe or
    // a missing input is this preview's real answer, exactly as it is
    // `exec::run`'s (see `registry::requires_probe`).
    if registry::requires_probe(job.from, job.to, tuning) {
        if !job.inputs[0].is_file() {
            return Err(ConvError::new(
                ErrorCode::InputNotFound,
                format!("input not found: {}", job.inputs[0].display()),
            ));
        }
        let ffprobe = resolver.resolve(Backend::Ffprobe)?;
        return probe::run(&ffprobe.path, &job.inputs[0]).map(Some);
    }
    if let (Some(read), Some(geometry)) = (
        registry::image_read(job.from, job.to, tuning),
        registry::image_read_geometry(job.to, tuning),
    ) {
        // As `exec::run` does: an unreadable size leaves `--upscale` its
        // general warning rather than failing the preview. `probe::image`
        // reads only existing regular files.
        return Ok(resolver
            .resolve(Backend::Magick)
            .ok()
            .and_then(|m| probe::image(&m.path, &job.inputs, read, geometry).ok()));
    }
    if !registry::needs_probe_tuned(job.from, job.to, tuning) {
        return Ok(None);
    }
    // `--dry-run` is documented as inert, but ffprobe honours URLs and
    // device paths, so probing the raw positional would turn a preview of
    // `http://…/x.mkv` into a real outbound fetch. Mirror `exec::run`'s
    // own input gate: only an existing regular file is ever probed, and
    // anything else falls back to the conservative transcode preview the
    // no-probe path already produces.
    if !job.inputs[0].is_file() {
        return Ok(None);
    }
    Ok(resolver
        .resolve(Backend::Ffprobe)
        .ok()
        .and_then(|p| probe::run(&p.path, &job.inputs[0]).ok()))
}

/// Checks backend availability when, and only when, this pair has more
/// than one recipe to choose between (`registry::has_fallback`) — mirrors
/// `probed_for`'s own gate exactly, so `--dry-run` and the real run it
/// previews always agree on whether this check happens at all, and an
/// ordinary conversion (every pair but docx/odt -> pdf, today) never pays
/// for it.
fn available_for(resolver: &Resolver, job: &input::Job) -> Option<AvailableBackends> {
    if !registry::has_fallback(job.from, job.to) {
        return None;
    }
    Some(resolver.check_availability(registry::FALLBACK_BACKENDS))
}

/// Builds a plan per job and reports every one of them, never aborting early
/// on the first failure — every job, one job included, is reported the same
/// way: inside the `"plans"` array under `--json` (I2 — a consumer used to
/// have to branch on job count to find the plan), or printed in turn in
/// human mode. A bad job among several others never erases the preview for
/// the rest, the same tolerance `batch::run` gives a real execution.
///
/// C3: probes first on any pair that might remux, or that a video knob
/// needs a source to cap against (`probed_for`, gated on
/// `registry::needs_probe_tuned` exactly like `exec::run`), so the preview
/// shown here is the *exact* command a real run would use — not the
/// conservative transcode `plan::build` falls back to with no probe.
/// `plan::build` itself stays pure; the probe runs here, in the caller, and
/// its result is passed in, the same split `exec::run` already uses between
/// itself and `plan::build`.
///
/// The same holds for backend availability: a docx/odt -> pdf dry-run must
/// preview the pandoc+typst command when soffice is
/// absent, not the (unusable) soffice one — `available_for` (gated on
/// `registry::has_fallback` exactly like `exec::run`'s own check) is what
/// makes that true.
///
/// A knob that wanted a probe but did not get one (`ffprobe` missing, the
/// probe itself failing, or the input not being a real file) gets no note
/// of its own here. `video::resolve` runs above `plan::build_tuned`'s probe
/// branch, on *every* path, including this one with `probed: None`, and
/// pushes "Source frame rate could not be determined; --fps N was applied
/// as given." onto the plan's own warnings for exactly this case, on a real
/// run and not only a preview. A note here as well would print two notes
/// about one missing probe.
fn dry_run(jobs: &[input::Job], cli: &Cli) -> i32 {
    let resolver = cli.resolver();
    let tuning = cli.tuning();
    let results: Vec<_> = jobs
        .iter()
        .map(|job| {
            let probed = probed_for(&resolver, job, &tuning)?;
            let available = available_for(&resolver, job);
            let mut plan = plan::build_tuned(
                job.from,
                job.to,
                &job.inputs,
                &job.output,
                probed.as_ref(),
                available.as_ref(),
                &tuning,
            )?;
            // As `exec::run` does, so the preview names the same fix.
            convkit_core::metadata::explain_missing_ffprobe(&mut plan, &resolver);
            Ok(plan)
        })
        .collect();

    if cli.json {
        let arr: Vec<serde_json::Value> = results
            .iter()
            .map(|r| match r {
                Ok(p) => json!({ "ok": true, "plan": p }),
                Err(e) => json!({ "ok": false, "error": e }),
            })
            .collect();
        let ok = results.iter().all(|r| r.is_ok());
        let envelope = json!({ "ok": ok, "dry_run": true, "plans": arr });
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
    } else {
        for r in &results {
            match r {
                Ok(p) => print!("{}", render::plan_human(p)),
                Err(e) => eprint!("{}", render::error_human(e)),
            }
        }
    }

    exit_code_for(&results)
}

/// Reports every job's outcome: successes to stdout, per-job failures to
/// stderr — a batch that partly fails must still let a script watching only
/// stderr (`2>errors.log`) see the trouble, and let a script piping only
/// stdout see nothing else. `--json` is unchanged in spirit: one envelope,
/// one write, on stdout, carrying both outcomes and errors — a machine
/// consumer reads the exit code for pass/fail, not which stream a line
/// landed on. I2: the envelope is now always `{"ok": ..., "results": [...]}`
/// — never a bare array with no `ok` field, which used to be this command's
/// own, fourth, incompatible `--json` success shape. Part 2 adds one
/// additive key to a successful job's own object (`elapsed_ms`, via
/// `render::outcome_json`) but otherwise leaves this whole branch untouched
/// — `--json` output is unaffected by Part 2's human-mode redesign.
///
/// Human mode (Part 2): a single job gets its own compact result
/// (`render::conversion_success_human`/`conversion_failure_human`); more
/// than one gets a batch summary line instead of a wall of per-job success
/// lines, with every per-job failure line still printed in full — "keep
/// per-job failure lines, drop per-job success spam." `--quiet` silences
/// every success line (single or batch summary) but never a failure line —
/// "silences everything except errors."
///
/// `shown` is what `confirm_extreme` printed before the run: a note that
/// repeats it word for word, for the same input, is not printed again.
fn print_results(
    results: &[batch::JobResult],
    cli: &Cli,
    elapsed: Duration,
    shown: &[(PathBuf, String)],
    copied: Option<&CopyReport>,
) {
    print_each(results, cli, elapsed, shown, copied);
    if let (false, Some(c)) = (cli.json, copied) {
        c.print(cli);
    }
}

fn print_each(
    results: &[batch::JobResult],
    cli: &Cli,
    elapsed: Duration,
    shown: &[(PathBuf, String)],
    copied: Option<&CopyReport>,
) {
    if cli.json {
        let arr: Vec<serde_json::Value> = results
            .iter()
            .map(|r| match &r.result {
                Ok(o) => {
                    let mut v = render::outcome_json(o);
                    v["input"] = json!(r.input);
                    v
                }
                Err(e) => json!({ "ok": false, "input": r.input, "output": r.output, "error": e }),
            })
            .collect();
        let ok = results.iter().all(|r| r.result.is_ok());
        let mut envelope = json!({ "ok": ok, "results": arr });
        if let Some(c) = copied {
            envelope["clipboard"] = c.json();
        }
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
        return;
    }

    let styled_out = render::stdout_styled();
    let styled_err = render::stderr_styled();
    let args = typed_args();
    let shown_for = |input: &Path| -> Vec<&str> {
        shown
            .iter()
            .filter(|(i, _)| i == input)
            .map(|(_, w)| w.as_str())
            .collect()
    };

    if let [r] = results {
        match &r.result {
            Ok(o) => {
                if !cli.quiet {
                    print!("{}", render::conversion_success_human(o, styled_out));
                }
                // Backend-reported degradation goes to stderr even under
                // --quiet: "silences everything except errors" — and a
                // conversion that dropped your images is in the errors'
                // half of that bargain, exit code notwithstanding.
                eprint!(
                    "{}",
                    render::conversion_notes_human("", o, &shown_for(&r.input), styled_err)
                );
                eprint!("{}", render::sizing_hint_human("", o, &args, styled_err));
            }
            Err(e) => {
                eprint!(
                    "{}",
                    render::conversion_failure_human(&r.input, r.to.ext(), e, styled_err)
                );
            }
        }
        return;
    }

    let mut err = String::new();
    for r in results {
        match &r.result {
            Err(e) => {
                err.push_str(&render::conversion_failure_human(
                    &r.input,
                    r.to.ext(),
                    e,
                    styled_err,
                ));
            }
            Ok(o) => {
                let label = r.input.display().to_string();
                err.push_str(&render::conversion_notes_human(
                    &label,
                    o,
                    &shown_for(&r.input),
                    styled_err,
                ));
                err.push_str(&render::sizing_hint_human(&label, o, &args, styled_err));
            }
        }
    }
    eprint!("{err}");

    if !cli.quiet {
        print!(
            "{}",
            render::batch_summary_human(results, elapsed, styled_out)
        );
    }
}

/// The command line as `main` parsed it, without the program name, for the
/// "try this instead" lines. Lossy, because the line is only shown, and a byte
/// sequence that is not UTF-8 (which clap accepts as a path) would panic
/// `env::args`.
fn typed_args() -> Vec<String> {
    wild::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// The batch exit-code rule, shared in spirit with `batch::run`: 0 if every
/// job succeeded, the first failure's own error code if every job failed
/// (so an all-unsupported-pair dry-run still exits 2, not a generic 4), or
/// `BatchPartialFailure` (4) on a genuinely mixed result.
fn exit_code_for<T>(results: &[Result<T, ConvError>]) -> i32 {
    let failures = results.iter().filter(|r| r.is_err()).count();
    match (failures, results.len()) {
        (0, _) => 0,
        (f, n) if f == n => match &results[0] {
            Err(e) => e.code.exit_code(),
            Ok(_) => unreachable!("f == n == results.len() means every result is Err"),
        },
        _ => ErrorCode::BatchPartialFailure.exit_code(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_paste_with_no_target_lists_what_it_can_become() {
        let e = say_what("clip.mov", &[Some(Format::Mov)]);
        assert!(e.message.contains("which can become mp4"), "{}", e.message);
        let hint = e.remediation.unwrap().manual.unwrap();
        assert!(hint.ends_with(" .mp4`"), "{hint}");
        // Several files: only what they can all become.
        let e = say_what("2 files", &[Some(Format::Png), Some(Format::Heic)]);
        assert!(
            e.message.contains("jpg") && !e.message.contains("heic,"),
            "{}",
            e.message
        );
        // Nothing in common.
        let e = say_what("2 files", &[Some(Format::Mov), Some(Format::Docx)]);
        assert_eq!(e.code, ErrorCode::UnsupportedPair);
        assert!(e.message.contains("can't make them all into one format"));
    }
    use std::io::Write;
    use std::path::{Path, PathBuf};

    /// Writes a stub standing in for `ffprobe`. Responds to a bare
    /// `-version` probe (as `Resolver::resolve` issues on every backend it
    /// finds) with a no-op success, and to anything else — the real
    /// `-v quiet -print_format json -show_streams -show_format <input>` invocation
    /// `probe::run` issues — with a fixed, compatible-codec JSON payload on
    /// stdout. Named arbitrarily (not `ffprobe.exe`/`ffprobe`) because this
    /// is registered via `Resolver::with_override`, which — unlike the
    /// CLI's `--ffmpeg-path`-derived sibling lookup in `cli.rs` — has no
    /// filename convention to satisfy.
    fn write_ffprobe_stub(dir: &Path) -> PathBuf {
        let (name, body) = if cfg!(windows) {
            (
                "ffprobe_stub.bat",
                "@echo off\r\n\
                 if \"%~1\"==\"-version\" (\r\n\
                 \x20   echo ffprobe-stub 1.0\r\n\
                 \x20   exit /b 0\r\n\
                 )\r\n\
                 echo {\"streams\":[{\"codec_type\":\"video\",\"codec_name\":\"h264\"},{\"codec_type\":\"audio\",\"codec_name\":\"aac\"}]}\r\n\
                 exit /b 0\r\n",
            )
        } else {
            (
                "ffprobe_stub.sh",
                "#!/bin/sh\n\
                 if [ \"$1\" = \"-version\" ]; then\n\
                 \x20   echo \"ffprobe-stub 1.0\"\n\
                 \x20   exit 0\n\
                 fi\n\
                 echo '{\"streams\":[{\"codec_type\":\"video\",\"codec_name\":\"h264\"},{\"codec_type\":\"audio\",\"codec_name\":\"aac\"}]}'\n",
            )
        };
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    fn job(
        from: convkit_core::Format,
        to: convkit_core::Format,
        input: &str,
        output: &str,
    ) -> input::Job {
        input::Job {
            inputs: vec![PathBuf::from(input)],
            output: PathBuf::from(output),
            from,
            to,
        }
    }

    /// C3's core mechanism: a pair that might remux (`mkv -> mp4`) must
    /// actually invoke ffprobe and return its codecs, not silently stay
    /// `None`. The input must be a real file on disk — `probed_for` now
    /// refuses to probe anything else (see the URL test below).
    #[test]
    fn probed_for_runs_ffprobe_on_a_pair_that_might_remux() {
        let dir = tempfile::tempdir().unwrap();
        let stub = write_ffprobe_stub(dir.path());
        let mut r = Resolver::new();
        r.with_override(Backend::Ffprobe, stub);

        let input = dir.path().join("in.mkv");
        std::fs::write(&input, b"x").unwrap();
        let j = input::Job {
            inputs: vec![input],
            output: PathBuf::from("out.mp4"),
            from: convkit_core::Format::Mkv,
            to: convkit_core::Format::Mp4,
        };
        let probed = probed_for(&r, &j, &Tuning::default())
            .unwrap()
            .expect("must probe a remuxable pair");
        assert_eq!(probed.video_codec.as_deref(), Some("h264"));
        assert_eq!(probed.audio_codec(), Some("aac"));
    }

    /// The dry-run SSRF hole: ffprobe follows URLs, so a `--dry-run` of
    /// `http://…/x.mkv` used to make a real network request from a command
    /// documented as inert. Anything that is not an existing regular file
    /// must skip the probe entirely — ffprobe is never spawned at all.
    #[test]
    fn probed_for_never_probes_an_input_that_is_not_a_local_file() {
        let dir = tempfile::tempdir().unwrap();
        let stub = write_ffprobe_stub(dir.path());
        let mut r = Resolver::new();
        r.with_override(Backend::Ffprobe, stub);

        for input in ["http://192.0.2.1/x.mkv", "definitely-missing.mkv"] {
            let j = job(
                convkit_core::Format::Mkv,
                convkit_core::Format::Mp4,
                input,
                "out.mp4",
            );
            assert!(
                probed_for(&r, &j, &Tuning::default()).unwrap().is_none(),
                "{input} must not be probed"
            );
        }
    }

    /// A pair that can never remux (no container change ffmpeg would ever
    /// stream-copy) must not probe at all — mirrors `exec::run`'s own gate,
    /// so the two never disagree on whether a probe happens.
    #[test]
    fn probed_for_skips_the_probe_for_a_pair_that_can_never_remux() {
        let r = Resolver::new();
        let j = job(
            convkit_core::Format::Pdf,
            convkit_core::Format::Docx,
            "in.pdf",
            "out.docx",
        );
        assert!(probed_for(&r, &j, &Tuning::default()).unwrap().is_none());
    }

    /// A webm video knob can only be honoured with a probe
    /// (`registry::requires_probe`), so the preview, like the real run,
    /// answers with the real cause instead of a conservative transcode
    /// preview that would then refuse the flag.
    #[test]
    fn probed_for_reports_a_webm_video_knobs_missing_probe_as_the_real_error() {
        let dir = tempfile::tempdir().unwrap();
        let stub = write_ffprobe_stub(dir.path());
        let fps = Tuning {
            fps: Some("15".into()),
            ..Default::default()
        };

        // Input missing: named, not skipped.
        let mut with_stub = Resolver::new();
        with_stub.with_override(Backend::Ffprobe, stub.clone());
        let j = job(
            convkit_core::Format::Mp4,
            convkit_core::Format::Webm,
            "definitely-missing.mp4",
            "out.webm",
        );
        let e = probed_for(&with_stub, &j, &fps).unwrap_err();
        assert_eq!(e.code, ErrorCode::InputNotFound, "{}", e.message);

        // Input present, ffprobe resolvable: probed.
        let input = dir.path().join("in.mp4");
        std::fs::write(&input, b"x").unwrap();
        let j = job(
            convkit_core::Format::Mp4,
            convkit_core::Format::Webm,
            input.to_str().unwrap(),
            "out.webm",
        );
        let probed = probed_for(&with_stub, &j, &fps).unwrap().unwrap();
        assert_eq!(probed.video_codec.as_deref(), Some("h264"));

        // Input present, no ffprobe anywhere: backend_missing, naming it.
        let mut none = Resolver::new();
        none.overrides_only();
        let e = probed_for(&none, &j, &fps).unwrap_err();
        assert_eq!(e.code, ErrorCode::BackendMissing, "{}", e.message);
        assert_eq!(e.backend, Some(Backend::Ffprobe));

        // The same missing ffprobe is still swallowed where a probe is only
        // an optimisation (an untuned remux, or a tuned mp4 target).
        let j = job(
            convkit_core::Format::Mkv,
            convkit_core::Format::Mp4,
            input.to_str().unwrap(),
            "out.mp4",
        );
        assert!(probed_for(&none, &j, &fps).unwrap().is_none());
    }

    /// A `magick` stand-in that answers `-ping` with `answer`. Unix only, as
    /// the read's `-format` argument holds a newline, which Rust will not
    /// pass to a batch file.
    #[cfg(unix)]
    fn write_magick_ping_stub(dir: &Path, answer: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("magick_stub.sh");
        std::fs::write(
            &p,
            format!("#!/bin/sh\nif [ \"$1\" = \"-ping\" ]; then echo '{answer}'; fi\n"),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// A jpg target's preview reads the image for its notes, as the real
    /// run does, and a webp target reads it for the location note; a pair
    /// whose notes do not depend on it (a tiff target keeps no EXIF, and
    /// --strip-metadata takes the location out), or an input that is not a
    /// file, is not read.
    #[cfg(unix)]
    #[test]
    fn probed_for_reads_the_image_a_jpg_targets_notes_depend_on() {
        let dir = tempfile::tempdir().unwrap();
        let stub = write_magick_ping_stub(dir.path(), "PNG Blend 1");
        let mut r = Resolver::new();
        r.with_override(Backend::Magick, stub);
        let input = dir.path().join("in.png");
        std::fs::write(&input, b"x").unwrap();
        let input = input.to_str().unwrap();

        let j = job(
            convkit_core::Format::Png,
            convkit_core::Format::Jpg,
            input,
            "out.jpg",
        );
        let probed = probed_for(&r, &j, &Tuning::default())
            .unwrap()
            .expect("a jpg target reads its source");
        let traits = probed.image.expect("the read's answer");
        assert_eq!(traits.alpha, Some(true));
        assert!(!traits.multi_frame);

        let webp = job(
            convkit_core::Format::Png,
            convkit_core::Format::Webp,
            input,
            "out.webp",
        );
        assert!(probed_for(&r, &webp, &Tuning::default()).unwrap().is_some());
        let stripped = Tuning {
            strip_metadata: true,
            ..Tuning::default()
        };
        assert!(probed_for(&r, &webp, &stripped).unwrap().is_none());
        let tiff = job(
            convkit_core::Format::Png,
            convkit_core::Format::Tiff,
            input,
            "out.tiff",
        );
        assert!(probed_for(&r, &tiff, &Tuning::default()).unwrap().is_none());
        let missing = job(
            convkit_core::Format::Png,
            convkit_core::Format::Jpg,
            "definitely-missing.png",
            "out.jpg",
        );
        assert!(probed_for(&r, &missing, &Tuning::default())
            .unwrap()
            .is_none());
    }

    // --- available_for -------------------------------------------------------

    /// A minimal script that exits 0 no matter what it's invoked with
    /// (including a bare version probe, either dash convention) — stands in
    /// for "a backend that resolves successfully" without needing to model
    /// any particular backend's real behaviour, since these tests only care
    /// whether resolution itself succeeds or fails.
    fn resolvable_stub(dir: &Path, name: &str) -> PathBuf {
        let (file_name, body): (String, &str) = if cfg!(windows) {
            (format!("{name}.bat"), "@echo off\r\nexit /b 0\r\n")
        } else {
            (name.to_string(), "#!/bin/sh\nexit 0\n")
        };
        let p = dir.join(file_name);
        std::fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    /// `available_for` must actually run `check_availability` for a pair
    /// that has a fallback recipe (docx -> pdf), reporting exactly which of
    /// soffice/pandoc/typst are resolvable. Soffice is deliberately left
    /// un-overridden and `overrides_only()` is what makes it unresolvable —
    /// a public, always-available `Resolver` method (unlike `with_managed_
    /// dir`, which stays `pub(crate)` to `convkit-core`) that closes the
    /// whole candidate chain but `Source::Override` in one call. A plain
    /// nonexistent override wouldn't serve this test's purpose even setting
    /// `overrides_only` aside: since the override-authority fix (see
    /// `resolve.rs`'s `Resolver::resolve` docs), a `Source::Override`
    /// pointing at a path that doesn't exist is now a hard, immediate
    /// `InvalidInvocation` error rather than a fall-through, so it would
    /// make this test about a bad override value, not about soffice
    /// genuinely being unavailable. Leaving Soffice with *no* override at
    /// all under `overrides_only` is what makes `candidates()` empty for it
    /// deterministically — on a machine with a real `CONVKIT_SOFFICE` set
    /// (e.g. this project's own hostile-environment audit) or a real,
    /// installed LibreOffice (this project's own dev machine has one at the
    /// standard Windows install location, which its installer never adds to
    /// `PATH`), only `overrides_only` closes every one of `Source::Env`/
    /// `Source::Path`/`Source::WellKnown` off. See
    /// `Resolver::overrides_only`'s docs.
    #[test]
    fn available_for_checks_availability_for_a_pair_with_a_fallback_recipe() {
        let dir = tempfile::tempdir().unwrap();
        let pandoc_stub = resolvable_stub(dir.path(), "pandoc_stub");
        let typst_stub = resolvable_stub(dir.path(), "typst_stub");
        let mut r = Resolver::new();
        r.overrides_only();
        r.with_override(Backend::Pandoc, pandoc_stub);
        r.with_override(Backend::Typst, typst_stub);

        let j = job(
            convkit_core::Format::Docx,
            convkit_core::Format::Pdf,
            "in.docx",
            "out.pdf",
        );
        let available = available_for(&r, &j).expect("docx->pdf has a fallback recipe");
        assert!(available.has(Backend::Pandoc));
        assert!(available.has(Backend::Typst));
        assert!(!available.has(Backend::Soffice));
    }

    /// A pair with only one possible recipe must never even check
    /// availability — mirrors `probed_for_skips_the_probe_for_a_pair_that_
    /// can_never_remux`'s reasoning for the media-remux probe.
    #[test]
    fn available_for_skips_the_check_for_a_pair_with_no_fallback() {
        let r = Resolver::new();
        let j = job(
            convkit_core::Format::Mp4,
            convkit_core::Format::Gif,
            "in.mp4",
            "out.gif",
        );
        assert!(available_for(&r, &j).is_none());
    }

    // --- Part 1: missing_backend / failed_on_missing_backend ----------------

    fn ok_job_result() -> batch::JobResult {
        batch::JobResult {
            input: PathBuf::from("in.mp4"),
            output: PathBuf::from("out.gif"),
            to: convkit_core::Format::Gif,
            result: Ok(Outcome {
                output: PathBuf::from("out.gif"),
                bytes: 1,
                warnings: vec![],
                notes: vec![],
                backend_output: vec![],
                backends: vec![],
                remuxed: false,
                elapsed_ms: 1,
                sizing: None,
                enlarged: None,
                range: None,
                speed: None,
            }),
        }
    }

    fn missing_result(backend: Backend) -> batch::JobResult {
        batch::JobResult {
            input: PathBuf::from("in.mp4"),
            output: PathBuf::from("out.gif"),
            to: convkit_core::Format::Gif,
            result: Err(ConvError::backend_missing(backend)),
        }
    }

    fn other_failure_result() -> batch::JobResult {
        batch::JobResult {
            input: PathBuf::from("in.mp4"),
            output: PathBuf::from("out.gif"),
            to: convkit_core::Format::Gif,
            result: Err(ConvError::new(ErrorCode::OutputExists, "exists")),
        }
    }

    #[test]
    fn missing_backend_finds_the_sole_backend_missing_failure() {
        let results = vec![ok_job_result(), missing_result(Backend::Ffmpeg)];
        assert_eq!(missing_backend(&results), Some(Backend::Ffmpeg));
    }

    #[test]
    fn missing_backend_is_none_when_nothing_failed_that_way() {
        let results = vec![ok_job_result(), other_failure_result()];
        assert_eq!(missing_backend(&results), None);
    }

    /// A mixed batch — two different backends both missing — must offer no
    /// prompt at all, since installing one would silently leave the other's
    /// failure unexplained.
    #[test]
    fn missing_backend_is_none_when_two_different_backends_are_missing() {
        let results = vec![
            missing_result(Backend::Ffmpeg),
            missing_result(Backend::Magick),
        ];
        assert_eq!(missing_backend(&results), None);
    }

    #[test]
    fn missing_backend_tolerates_repeats_of_the_same_backend() {
        let results = vec![
            missing_result(Backend::Ffmpeg),
            missing_result(Backend::Ffmpeg),
        ];
        assert_eq!(missing_backend(&results), Some(Backend::Ffmpeg));
    }

    #[test]
    fn failed_on_missing_backend_matches_only_the_named_backend() {
        let r = missing_result(Backend::Ffmpeg);
        assert!(failed_on_missing_backend(&r.result, Backend::Ffmpeg));
        assert!(!failed_on_missing_backend(&r.result, Backend::Magick));
    }

    #[test]
    fn failed_on_missing_backend_is_false_for_a_different_error_code() {
        let r = other_failure_result();
        assert!(!failed_on_missing_backend(&r.result, Backend::Ffmpeg));
    }
}
