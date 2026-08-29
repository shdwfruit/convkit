use convkit_core::{plan, ConvError, ErrorCode};
use serde_json::json;

use crate::batch;
use crate::cli::Cli;
use crate::input;
use crate::render;

pub fn run(cli: &Cli) -> i32 {
    let jobs = match input::plan_jobs(cli) {
        Ok(jobs) => jobs,
        Err(e) => {
            print_error(cli, &e);
            return e.code.exit_code();
        }
    };

    if cli.dry_run {
        return dry_run(&jobs, cli);
    }

    let (results, code) = batch::run(jobs, cli);
    print_results(&results, cli);
    code
}

/// Errors resolved before any job ran (bad `--to`, an unsupported pair, a
/// directory that couldn't be read) — the same top-level envelope a
/// single-job run always used, printed to stderr.
fn print_error(cli: &Cli, e: &ConvError) {
    if cli.json {
        let envelope = json!({ "ok": false, "error": e });
        eprintln!("{}", serde_json::to_string_pretty(&envelope).unwrap());
    } else {
        eprint!("{}", render::error_human(e));
    }
}

/// Builds a plan per job (never probing — `--dry-run` always shows the
/// conservative transcode) and reports every one of them, never aborting
/// early on the first failure.
///
/// A single job replicates the exact legacy single-conversion shape and
/// error routing — a build failure is a top-level error, on stderr, with
/// that error's own exit code — which also happens to be exactly what the
/// multi-job rule below produces for a batch of one, so the preview and the
/// real run it previews always agree on both shape and exit code.
///
/// Two or more jobs never abort early: every job gets its own plan attempt,
/// with the same per-job stdout/stderr split and exit-code aggregation
/// `batch::run` uses for a real execution, so a bad job among several
/// others doesn't erase the preview for the rest.
fn dry_run(jobs: &[input::Job], cli: &Cli) -> i32 {
    if let [only] = jobs {
        return match plan::build(only.from, only.to, &only.inputs, &only.output, None) {
            Ok(p) => {
                let text = if cli.json {
                    format!(
                        "{}\n",
                        serde_json::to_string_pretty(&render::plan_json(&p)).unwrap()
                    )
                } else {
                    render::plan_human(&p)
                };
                print!("{text}");
                0
            }
            Err(e) => {
                print_error(cli, &e);
                e.code.exit_code()
            }
        };
    }

    let results: Vec<_> = jobs
        .iter()
        .map(|job| plan::build(job.from, job.to, &job.inputs, &job.output, None))
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
/// stdout see nothing else. `--json` is unchanged: one array, one write, on
/// stdout, carrying both outcomes and errors — a machine consumer reads the
/// exit code for pass/fail, not which stream a line landed on.
fn print_results(results: &[batch::JobResult], cli: &Cli) {
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
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Array(arr)).unwrap()
        );
    } else {
        let mut out = String::new();
        let mut err = String::new();
        for r in results {
            match &r.result {
                Ok(o) => out.push_str(&render::outcome_human(o)),
                Err(e) => err.push_str(&render::error_human(e)),
            }
        }
        print!("{out}");
        eprint!("{err}");
    }
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
