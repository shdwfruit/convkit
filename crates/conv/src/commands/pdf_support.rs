//! What `conv merge`, `conv split`, `conv pages` and a PDF's `--password`
//! and `--unlock` share: finding qpdf (offering to install it), the lock
//! `--password` asks for, `-v` output, and printing a plan, a result or a
//! failure.

use std::time::Duration;

use convkit_core::pdf::{PdfOutcome, PdfPlan};
use convkit_core::{ConvError, Event};
use serde_json::json;

use crate::cli::{Cli, LockArgs};
use crate::password;
use crate::render;

/// `plan`, locked as `--password` asks, once every input is open. `what`
/// names the output in the question: `out.pdf`, `the split files`.
pub(crate) fn locked(
    mut plan: PdfPlan,
    lock: &LockArgs,
    what: &str,
    dry_run: bool,
) -> Result<PdfPlan, ConvError> {
    if let Some(lock) = password::lock_for(lock.password, &lock.restrict, what, dry_run)? {
        plan.set_lock(lock);
    }
    Ok(plan)
}

/// `-v`: each qpdf command line as it is spawned, then qpdf's output, on
/// stderr.
pub(crate) fn verbose_printer(verbose: bool) -> impl FnMut(Event) {
    move |event| match event {
        Event::StepSpawned { program, argv, .. } if verbose => {
            eprintln!(
                "+ {}",
                render::command_line_human(&program.to_string_lossy(), &argv)
            );
        }
        Event::StepReport {
            backend, report, ..
        } if verbose => {
            eprintln!(
                "{}",
                render::verbose_report_human(backend.exe_name(), &report).trim_end()
            );
        }
        _ => {}
    }
}

pub(crate) fn print_dry_run(cli: &Cli, plan: &PdfPlan) {
    if cli.json {
        let v = json!({ "ok": true, "dry_run": true, "plans": [render::pdf_plan_json(plan)] });
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        print!("{}", render::pdf_plan_human(plan));
    }
}

fn with_subject(mut v: serde_json::Value, subject: &serde_json::Value) -> serde_json::Value {
    if let (Some(obj), Some(extra)) = (v.as_object_mut(), subject.as_object()) {
        for (k, val) in extra {
            obj.insert(k.clone(), val.clone());
        }
    }
    v
}

/// A finished run. `subject` is `{"inputs", "output"}` for a merge and
/// `{"input"}` for a split, added to the JSON element.
pub(crate) fn print_success(
    cli: &Cli,
    subject: serde_json::Value,
    o: &PdfOutcome,
    elapsed: Duration,
) {
    if cli.json {
        let v = with_subject(render::pdf_outcome_json(o, elapsed), &subject);
        let envelope = json!({ "ok": true, "results": [v] });
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
        return;
    }
    if !cli.quiet {
        print!(
            "{}",
            render::pdf_success_human(o, elapsed, render::stdout_styled())
        );
    }
    eprint!("{}", render::pdf_warnings_human(o, render::stderr_styled()));
}

/// A failure once the inputs are known: a `FAIL <header>` block on stderr,
/// or a JSON element on stdout (in `plans` for a dry run, `results`
/// otherwise). Returns the exit code.
pub(crate) fn print_failure(
    cli: &Cli,
    subject: serde_json::Value,
    header: &str,
    e: &ConvError,
    dry_run: bool,
) -> i32 {
    if cli.json {
        let v = with_subject(json!({ "ok": false, "error": e }), &subject);
        let envelope = if dry_run {
            json!({ "ok": false, "dry_run": true, "plans": [v] })
        } else {
            json!({ "ok": false, "results": [v] })
        };
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
    } else {
        eprint!(
            "{}",
            render::failure_human(header, e, render::stderr_styled())
        );
    }
    e.code.exit_code()
}
