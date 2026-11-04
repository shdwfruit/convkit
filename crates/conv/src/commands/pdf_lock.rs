//! `conv report.pdf --password` and `conv report.pdf --unlock`: a PDF kept
//! as a PDF, locked with a password or written again without one.
//!
//! These are flags on a file changed in place, like `--strip-metadata`,
//! and follow the same naming: one PDF becomes `report-locked.pdf` (or
//! `-unlocked`), a named output is written as named, `-o DIR` keeps the
//! name, and a `--to pdf` batch writes into a `locked` folder beside its
//! files. qpdf does the work, so nothing goes through the conversion
//! engine.

use std::path::{Path, PathBuf};
use std::time::Instant;

use convkit_core::pdf::{self, PdfOutcome, PdfPlan, Rewrite};
use convkit_core::{ConvError, ErrorCode, Format, ResolvedBackend};
use serde_json::json;

use crate::cli::Cli;
use crate::clipboard::Got;
use crate::commands::convert::copy_outputs;
use crate::commands::pdf_support;
use crate::input::{self, collision_key, expand_globs, is_extension_shorthand};
use crate::password::{self, Opener};
use crate::{paste, render};

/// Whether this run locks or unlocks PDFs.
pub fn wanted(cli: &Cli) -> bool {
    cli.password || cli.unlock
}

/// The words for each way.
struct Way {
    how: Rewrite,
    flag: &'static str,
    verb: &'static str,
    done: &'static str,
}

fn way(cli: &Cli) -> Way {
    if cli.unlock {
        Way {
            how: Rewrite::Unlock,
            flag: "--unlock",
            verb: "unlock",
            done: "unlocked",
        }
    } else {
        Way {
            how: Rewrite::Lock,
            flag: "--password",
            verb: "lock",
            done: "locked",
        }
    }
}

/// One PDF and where it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Job {
    input: PathBuf,
    output: PathBuf,
}

fn invalid(message: String) -> ConvError {
    ConvError::new(ErrorCode::InvalidInvocation, message)
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// `report.pdf` + `locked` -> `report-locked.pdf`, keeping the extension's
/// case.
fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| "pdf".to_string());
    path.with_file_name(format!("{stem}-{suffix}.{ext}"))
}

/// The conversion flags this has no use for, refused rather than ignored.
fn refuse_other_flags(cli: &Cli, way: &Way) -> Result<(), ConvError> {
    if let Some(turn) = cli.rotate {
        let file = cli
            .paths
            .first()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "FILE.pdf".to_string());
        let lock = if cli.password { " --password" } else { "" };
        return Err(ConvError {
            remediation: Some(convkit_core::Remediation {
                managed: None,
                manual: Some(format!(
                    "conv pages {file} --rotate {}{lock}",
                    super::pages::turn_word(turn)
                )),
            }),
            ..invalid(format!(
                "{} doesn't combine with --rotate; conv pages turns a PDF's pages",
                way.flag
            ))
        });
    }
    if !cli.tuning().is_empty() {
        return Err(invalid(format!(
            "{} doesn't combine with --resize, --quality or the other conversion flags",
            way.flag
        )));
    }
    match cli.to.as_deref() {
        Some(to) if !to.trim_start_matches('.').eq_ignore_ascii_case("pdf") => Err(invalid(
            format!("{} writes a PDF; use --to pdf, or leave --to out", way.flag),
        )),
        _ => Ok(()),
    }
}

/// A path this run can take as an input.
fn check_pdf(p: &Path, way: &Way) -> Result<(), ConvError> {
    if Format::from_path(p) == Some(Format::Pdf) {
        return Ok(());
    }
    let e = invalid(format!(
        "{} {}s PDFs, and {} is not one",
        way.flag,
        way.verb,
        name_of(p)
    ));
    // Anything else is locked by putting it in a zip.
    if way.flag != "--password" {
        return Err(e);
    }
    Err(ConvError {
        remediation: Some(convkit_core::Remediation {
            managed: None,
            manual: Some(format!(
                "to lock it in a zip instead: conv {} .zip --password",
                p.display()
            )),
        }),
        ..e
    })
}

/// Where each input goes, by the rules of a file changed in place: see the
/// module docs.
fn plan_paths(
    paths: &[PathBuf],
    batch: bool,
    outdir: Option<&Path>,
    way: &Way,
    password_flag: bool,
) -> Result<Vec<Job>, ConvError> {
    if paths.is_empty() {
        return Err(invalid(format!(
            "expected a PDF, e.g. `conv report.pdf {}`",
            way.flag
        )));
    }
    let lone = |input: &Path| -> Result<Job, ConvError> {
        check_pdf(input, way)?;
        let output = match outdir {
            Some(dir) => dir.join(input.file_name().unwrap_or_default()),
            None => input.to_path_buf(),
        };
        let output = if collision_key(&output) == collision_key(input) {
            suffixed(&output, way.done)
        } else {
            output
        };
        Ok(Job {
            input: input.to_path_buf(),
            output,
        })
    };
    if !batch {
        let globbable = paths.len().max(2) - 1;
        let positionals = expand_globs(paths, globbable);
        if paths.len() == 1 && positionals.len() > 1 {
            return Err(invalid(format!(
                "{} matched {} files; add --to pdf to {} each one",
                paths[0].display(),
                positionals.len(),
                way.verb
            )));
        }
        return match positionals.as_slice() {
            [one] if one.is_dir() => Err(invalid(format!(
                "{} is a directory; add --to pdf to {} the PDFs in it",
                one.display(),
                way.verb
            ))),
            [one] => Ok(vec![lone(one)?]),
            [input, out] if is_extension_shorthand(out) => {
                if !out.to_string_lossy().eq_ignore_ascii_case(".pdf") {
                    return Err(invalid(format!(
                        "{} writes a PDF; {} is not one",
                        way.flag,
                        out.display()
                    )));
                }
                Ok(vec![lone(input)?])
            }
            [input, out] => {
                check_pdf(input, way)?;
                if Format::from_path(out) != Some(Format::Pdf) {
                    if password_flag && !out.exists() {
                        return Err(password::typed_after_the_flag(&out.display().to_string()));
                    }
                    return Err(invalid(format!(
                        "{} writes a PDF; {} is not one",
                        way.flag,
                        name_of(out)
                    )));
                }
                let output = input::in_outdir(out, outdir)?;
                if collision_key(&output) == collision_key(input) {
                    return Err(invalid(format!(
                        "output is the input: {}; name a different output",
                        output.display()
                    )));
                }
                // What `conv *.pdf --password` gives with two PDFs here: the
                // second is not meant as the output, so it is never written
                // over, even with -y.
                if output.exists() {
                    return Err(invalid(format!(
                        "{} and {} are both pdf files; add --to pdf to {} each one, or \
                         remove {} to write a {} copy there",
                        input.display(),
                        output.display(),
                        way.verb,
                        output.display(),
                        way.done
                    )));
                }
                Ok(vec![Job {
                    input: input.clone(),
                    output,
                }])
            }
            _ => Err(invalid(format!(
                "{} without --to takes one PDF, or a PDF and its output; add --to pdf to {} \
                 each file",
                way.flag, way.verb
            ))),
        };
    }

    let mut inputs = Vec::new();
    for p in expand_globs(paths, paths.len()) {
        if p.is_dir() {
            let ending = format!("-{}", way.done);
            let found: Vec<PathBuf> = input::files_in(&p, &p, |f| f == Format::Pdf)?
                .into_iter()
                .filter(|f| {
                    !f.file_stem()
                        .is_some_and(|s| s.to_string_lossy().to_lowercase().ends_with(&ending))
                })
                .collect();
            if found.is_empty() {
                return Err(invalid(format!("{} has no PDF files", p.display())));
            }
            inputs.extend(found);
        } else {
            check_pdf(&p, way)?;
            inputs.push(p);
        }
    }
    let jobs: Vec<Job> = inputs
        .into_iter()
        .map(|input| {
            let name = input.file_name().unwrap_or_default().to_os_string();
            let output = match outdir {
                Some(dir) => dir.join(&name),
                None => match input.parent() {
                    Some(dir) => dir.join(way.done).join(&name),
                    None => Path::new(way.done).join(&name),
                },
            };
            Job { input, output }
        })
        .collect();
    let input_keys: Vec<PathBuf> = jobs.iter().map(|j| collision_key(&j.input)).collect();
    for (i, job) in jobs.iter().enumerate() {
        let key = collision_key(&job.output);
        if let Some(prev) = jobs[..i].iter().find(|j| collision_key(&j.output) == key) {
            return Err(invalid(format!(
                "outputs collide: {} and {} both produce {}",
                prev.input.display(),
                job.input.display(),
                job.output.display()
            )));
        }
        if input_keys.contains(&key) {
            return Err(invalid(format!(
                "outputs collide: {} would write {}, which is also an input",
                job.input.display(),
                job.output.display()
            )));
        }
    }
    Ok(jobs)
}

/// The paths this run takes: the clipboard's files under `--paste`, then
/// what was typed.
fn typed_or_pasted(cli: &Cli) -> Result<Vec<PathBuf>, ConvError> {
    if !cli.paste {
        return Ok(cli.paths.clone());
    }
    match paste::get()? {
        Got::Files(files) => Ok(files.into_iter().chain(cli.paths.iter().cloned()).collect()),
        Got::Picture { .. } => Err(invalid(
            "the clipboard holds a picture, not a PDF".to_string(),
        )),
        Got::Text(_) => Err(invalid("the clipboard holds text, not a PDF".to_string())),
    }
}

/// What became of one file: planned or written, skipped because a batch
/// had nothing to do to it, or failed.
enum Fate<T> {
    Done(T),
    Skipped(String),
    Failed(ConvError),
}

impl<T> Fate<T> {
    fn error(&self) -> Option<&ConvError> {
        match self {
            Fate::Failed(e) => Some(e),
            _ => None,
        }
    }
}

pub fn run(cli: &Cli) -> i32 {
    let way = way(cli);
    let batch = cli.to.is_some();
    let planned = refuse_other_flags(cli, &way)
        .and_then(|()| typed_or_pasted(cli))
        .and_then(|paths| plan_paths(&paths, batch, cli.outdir.as_deref(), &way, cli.password));
    let jobs = match planned {
        Ok(jobs) => jobs,
        Err(e) => {
            render::print_error(cli.json, &e);
            return e.code.exit_code();
        }
    };
    let subject = |job: &Job| json!({ "input": job.input, "output": job.output });
    // Check the inputs before offering to install qpdf.
    if let Some(missing) = jobs.iter().find(|j| !j.input.is_file()) {
        let e = ConvError::new(
            ErrorCode::InputNotFound,
            format!("input not found: {}", missing.input.display()),
        );
        let name = name_of(&missing.input);
        return pdf_support::print_failure(cli, subject(missing), &name, &e, cli.dry_run);
    }
    let qpdf = match pdf_support::resolve_qpdf(cli, !cli.dry_run) {
        Ok(q) => q,
        Err(e) => {
            let name = name_of(&jobs[0].input);
            return pdf_support::print_failure(cli, subject(&jobs[0]), &name, &e, cli.dry_run);
        }
    };

    // Every locked input is opened, and asked for, before the new password.
    let mut opener = match Opener::from_env() {
        Ok(o) => o,
        Err(e) => {
            render::print_error(cli.json, &e);
            return e.code.exit_code();
        }
    };
    let mut plans: Vec<(Job, Fate<PdfPlan>)> = jobs
        .into_iter()
        .map(|job| {
            let opened = pdf::open(&qpdf.path, &job.input, &mut |p, w| opener.next(p, w));
            let fate = match opened {
                // A folder of statements unlocks the locked ones; the rest
                // need nothing.
                Ok(info) if batch && way.how == Rewrite::Unlock && !info.encrypted => {
                    Fate::Skipped(format!(
                        "{} has no password or restrictions, so it was left as it is.",
                        name_of(&job.input)
                    ))
                }
                Ok(info) => match pdf::plan::rewrite_plan(&info, &job.output, way.how) {
                    Ok(plan) => Fate::Done(plan),
                    Err(e) => Fate::Failed(e),
                },
                Err(e) => Fate::Failed(e),
            };
            (job, fate)
        })
        .collect();
    if plans.iter().any(|(_, f)| matches!(f, Fate::Done(_))) {
        let what = match plans.as_slice() {
            [(job, _)] => name_of(&job.output),
            _ => format!("the {} PDFs", way.done),
        };
        match password::lock_for(cli.password, &cli.restrict, &what, cli.dry_run) {
            Ok(Some(lock)) => {
                for (_, fate) in plans.iter_mut() {
                    if let Fate::Done(plan) = fate {
                        plan.set_lock(lock.clone());
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                render::print_error(cli.json, &e);
                return e.code.exit_code();
            }
        }
    }

    if cli.dry_run {
        return dry_run(cli, &plans, &subject);
    }
    let start = Instant::now();
    let done: Vec<(Job, Fate<PdfOutcome>)> = plans
        .into_iter()
        .map(|(job, fate)| {
            let fate = match fate {
                Fate::Done(plan) => match write(cli, &qpdf, &plan) {
                    Ok(o) => Fate::Done(o),
                    Err(e) => Fate::Failed(e),
                },
                Fate::Skipped(why) => Fate::Skipped(why),
                Fate::Failed(e) => Fate::Failed(e),
            };
            (job, fate)
        })
        .collect();
    report(cli, &way, batch, done, start, &subject)
}

/// Runs one plan, making its folder first.
fn write(cli: &Cli, qpdf: &ResolvedBackend, plan: &PdfPlan) -> Result<PdfOutcome, ConvError> {
    if let Some(dir) = plan.outputs[0]
        .path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
    {
        std::fs::create_dir_all(dir).map_err(|e| {
            invalid(format!(
                "cannot create output directory {}: {e}",
                dir.display()
            ))
        })?;
    }
    let mut on_event = pdf_support::verbose_printer(cli.verbose);
    pdf::run(plan, qpdf, cli.overwrite, &mut on_event)
}

/// One JSON element: `done` for a planned or written file, with the file's
/// `input` and `output` added.
fn element<T>(
    job: &Job,
    fate: &Fate<T>,
    done: impl Fn(&T) -> serde_json::Value,
    subject: &dyn Fn(&Job) -> serde_json::Value,
) -> serde_json::Value {
    let mut v = match fate {
        Fate::Done(t) => done(t),
        Fate::Skipped(why) => json!({ "ok": true, "skipped": true, "warnings": [why] }),
        Fate::Failed(e) => json!({ "ok": false, "error": e }),
    };
    for (k, val) in subject(job).as_object().into_iter().flatten() {
        v[k] = val.clone();
    }
    v
}

fn dry_run(
    cli: &Cli,
    plans: &[(Job, Fate<PdfPlan>)],
    subject: &dyn Fn(&Job) -> serde_json::Value,
) -> i32 {
    let code = exit_code(plans.iter().map(|(_, f)| f.error()));
    if cli.json {
        let elements: Vec<serde_json::Value> = plans
            .iter()
            .map(|(job, fate)| element(job, fate, render::pdf_plan_json, subject))
            .collect();
        let v = json!({ "ok": code == 0, "dry_run": true, "plans": elements });
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
        return code;
    }
    for (job, fate) in plans {
        match fate {
            Fate::Done(plan) => print!("{}", render::pdf_plan_human(plan)),
            Fate::Skipped(why) => println!("note: {why}"),
            Fate::Failed(e) => eprint!(
                "{}",
                render::failure_human(&name_of(&job.input), e, render::stderr_styled())
            ),
        }
    }
    code
}

/// 0 when everything worked, the one error's code when everything failed,
/// and the partial-failure code otherwise, as a conversion batch does. A
/// skipped file is not a failure.
fn exit_code<'a>(errors: impl Iterator<Item = Option<&'a ConvError>>) -> i32 {
    let errors: Vec<Option<&ConvError>> = errors.collect();
    let failed: Vec<&ConvError> = errors.iter().flatten().copied().collect();
    match (failed.len(), errors.len()) {
        (0, _) => 0,
        (f, n) if f == n => failed[0].code.exit_code(),
        _ => ErrorCode::BatchPartialFailure.exit_code(),
    }
}

fn report(
    cli: &Cli,
    way: &Way,
    batch: bool,
    done: Vec<(Job, Fate<PdfOutcome>)>,
    start: Instant,
    subject: &dyn Fn(&Job) -> serde_json::Value,
) -> i32 {
    let elapsed = start.elapsed();
    let written: Vec<PathBuf> = done
        .iter()
        .filter_map(|(_, f)| match f {
            Fate::Done(o) => Some(o),
            _ => None,
        })
        .flat_map(|o| o.outputs.iter().map(|w| w.path.clone()))
        .collect();
    let code = exit_code(done.iter().map(|(_, f)| f.error()));
    // One file outside a batch reads like a conversion. It is never
    // skipped: with nothing to do, it is refused instead.
    if let ([(job, fate)], false) = (done.as_slice(), batch) {
        match fate {
            Fate::Done(o) => pdf_support::print_success(cli, subject(job), o, elapsed),
            Fate::Skipped(_) => {}
            Fate::Failed(e) => {
                pdf_support::print_failure(cli, subject(job), &name_of(&job.input), e, false);
            }
        }
        copy(cli, &written);
        return code;
    }

    if cli.json {
        let results: Vec<serde_json::Value> = done
            .iter()
            .map(|(job, fate)| {
                element(job, fate, |o| render::pdf_outcome_json(o, elapsed), subject)
            })
            .collect();
        let mut envelope = json!({ "ok": code == 0, "results": results });
        if cli.copy && !written.is_empty() {
            envelope["clipboard"] = copy_outputs(&written).json();
        }
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
        return code;
    }
    let mut counts = (0, 0, 0);
    let mut notes = Vec::new();
    for (job, fate) in &done {
        match fate {
            Fate::Done(o) => {
                counts.0 += 1;
                eprint!("{}", render::pdf_warnings_human(o, render::stderr_styled()));
                let name = name_of(&job.input);
                notes.extend(o.notes.iter().map(|n| format!("{name}: {n}")));
            }
            Fate::Skipped(why) => {
                counts.1 += 1;
                notes.push(why.clone());
            }
            Fate::Failed(e) => {
                if e.code == ErrorCode::OutputExists {
                    counts.1 += 1;
                } else {
                    counts.2 += 1;
                }
                eprint!(
                    "{}",
                    render::failure_human(&name_of(&job.input), e, render::stderr_styled())
                );
            }
        }
    }
    if !cli.quiet {
        print!(
            "{}",
            render::pdf_batch_summary_human(
                way.done,
                counts,
                elapsed,
                written.first().map(PathBuf::as_path),
                render::stdout_styled(),
            )
        );
        for line in render::notes_human(&notes, render::stdout_styled()).lines() {
            println!("  {line}");
        }
    }
    copy(cli, &written);
    code
}

/// `--copy`, outside `--json` (where it goes in the envelope).
fn copy(cli: &Cli, written: &[PathBuf]) {
    if cli.copy && !written.is_empty() && !cli.json {
        copy_outputs(written).print(cli);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock() -> Way {
        Way {
            how: Rewrite::Lock,
            flag: "--password",
            verb: "lock",
            done: "locked",
        }
    }

    fn unlock() -> Way {
        Way {
            how: Rewrite::Unlock,
            flag: "--unlock",
            verb: "unlock",
            done: "unlocked",
        }
    }

    fn v(p: &[&str]) -> Vec<PathBuf> {
        p.iter().map(PathBuf::from).collect()
    }

    fn outputs(jobs: &[Job]) -> Vec<PathBuf> {
        jobs.iter().map(|j| j.output.clone()).collect()
    }

    #[test]
    fn a_lone_pdf_gets_the_suffix_and_keeps_its_name_in_another_folder() {
        let jobs = plan_paths(&v(&["docs/report.pdf"]), false, None, &lock(), true).unwrap();
        assert_eq!(outputs(&jobs), v(&["docs/report-locked.pdf"]));
        let jobs = plan_paths(&v(&["Report.PDF"]), false, None, &unlock(), false).unwrap();
        assert_eq!(outputs(&jobs), v(&["Report-unlocked.PDF"]));
        let jobs = plan_paths(
            &v(&["report.pdf"]),
            false,
            Some(Path::new("out")),
            &lock(),
            true,
        )
        .unwrap();
        assert_eq!(outputs(&jobs), vec![Path::new("out").join("report.pdf")]);
        let jobs = plan_paths(&v(&["report.pdf", ".pdf"]), false, None, &lock(), true).unwrap();
        assert_eq!(outputs(&jobs), v(&["report-locked.pdf"]));
    }

    #[test]
    fn a_named_output_is_written_as_named() {
        let jobs = plan_paths(
            &v(&["report.pdf", "secret.pdf"]),
            false,
            None,
            &lock(),
            true,
        )
        .unwrap();
        assert_eq!(outputs(&jobs), v(&["secret.pdf"]));
        let e = plan_paths(
            &v(&["report.pdf", "report.pdf"]),
            false,
            None,
            &lock(),
            true,
        )
        .unwrap_err();
        assert_eq!(
            e.message,
            "output is the input: report.pdf; name a different output"
        );
    }

    #[test]
    fn an_existing_second_pdf_is_never_taken_for_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.pdf"), dir.path().join("b.pdf"));
        std::fs::write(&a, b"%PDF").unwrap();
        std::fs::write(&b, b"%PDF").unwrap();
        let e = plan_paths(&[a, b.clone()], false, None, &lock(), true).unwrap_err();
        assert!(
            e.message
                .contains("are both pdf files; add --to pdf to lock each one"),
            "{}",
            e.message
        );
    }

    #[test]
    fn what_is_not_a_pdf_is_refused_and_a_typed_password_called_out() {
        let e = plan_paths(&v(&["photo.jpg"]), false, None, &lock(), true).unwrap_err();
        assert_eq!(e.message, "--password locks PDFs, and photo.jpg is not one");
        let e = plan_paths(&v(&["report.pdf", "hunter2"]), false, None, &lock(), true).unwrap_err();
        assert!(
            e.message.starts_with("--password takes no value"),
            "{}",
            e.message
        );
        let e = plan_paths(
            &v(&["report.pdf", "out.docx"]),
            false,
            None,
            &unlock(),
            false,
        )
        .unwrap_err();
        assert_eq!(e.message, "--unlock writes a PDF; out.docx is not one");
        let e = plan_paths(
            &v(&["a.pdf", "b.pdf", "c.pdf"]),
            false,
            None,
            &unlock(),
            false,
        )
        .unwrap_err();
        assert!(
            e.message.ends_with("add --to pdf to unlock each file"),
            "{}",
            e.message
        );
    }

    #[test]
    fn a_batch_writes_into_a_folder_named_for_what_was_done() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs");
        std::fs::create_dir(&docs).unwrap();
        for name in ["b10.pdf", "b2.pdf", "notes.txt", "old-locked.pdf"] {
            std::fs::write(docs.join(name), b"x").unwrap();
        }
        let jobs = plan_paths(std::slice::from_ref(&docs), true, None, &lock(), true).unwrap();
        assert_eq!(
            outputs(&jobs),
            vec![
                docs.join("locked").join("b2.pdf"),
                docs.join("locked").join("b10.pdf")
            ]
        );
        let out = dir.path().join("out");
        let jobs =
            plan_paths(std::slice::from_ref(&docs), true, Some(&out), &lock(), true).unwrap();
        assert_eq!(jobs[0].output, out.join("b2.pdf"));
        let e = plan_paths(std::slice::from_ref(&docs), false, None, &lock(), true).unwrap_err();
        assert!(e
            .message
            .ends_with("is a directory; add --to pdf to lock the PDFs in it"));
    }

    #[test]
    fn a_batch_whose_outputs_collide_is_refused() {
        let e = plan_paths(
            &v(&["a/r.pdf", "b/r.pdf"]),
            true,
            Some(Path::new("out")),
            &lock(),
            true,
        )
        .unwrap_err();
        assert!(e.message.starts_with("outputs collide"), "{}", e.message);
    }

    #[test]
    fn exit_codes_follow_a_batchs() {
        let e = ConvError::new(ErrorCode::PasswordProtected, "x");
        assert_eq!(exit_code([None, None].into_iter()), 0);
        assert_eq!(exit_code([Some(&e)].into_iter()), 1);
        assert_eq!(exit_code([None, Some(&e)].into_iter()), 4);
    }
}
