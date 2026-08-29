use convkit_core::{plan, ConvError};
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
        return match dry_run_output(&jobs, cli) {
            Ok(text) => {
                print!("{text}");
                0
            }
            Err(e) => {
                print_error(cli, &e);
                e.code.exit_code()
            }
        };
    }

    let (results, code) = batch::run(jobs, cli);
    print!("{}", render_results(&results, cli));
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
/// conservative transcode) and renders them. A single job keeps the exact
/// envelope shape single-file conversion has always used (`"plan": {...}`);
/// two or more jobs switch to a `"plans"` array so existing tooling that
/// reads the single-job shape is unaffected.
fn dry_run_output(jobs: &[input::Job], cli: &Cli) -> Result<String, ConvError> {
    let mut plans = Vec::with_capacity(jobs.len());
    for job in jobs {
        plans.push(plan::build(
            job.from,
            job.to,
            &job.inputs,
            &job.output,
            None,
        )?);
    }
    Ok(if cli.json {
        let value = if let [only] = plans.as_slice() {
            render::plan_json(only)
        } else {
            json!({ "ok": true, "dry_run": true, "plans": plans })
        };
        format!("{}\n", serde_json::to_string_pretty(&value).unwrap())
    } else {
        plans
            .iter()
            .map(render::plan_human)
            .collect::<Vec<_>>()
            .join("")
    })
}

/// Reports every job's outcome. Unlike a top-level error, a per-job failure
/// does not abort the run — other jobs may have succeeded — so both
/// successes and failures land in the same report (stdout in human mode,
/// one JSON array in `--json` mode); the process exit code, not which
/// stream a line landed on, is what signals overall pass/fail here.
fn render_results(results: &[batch::JobResult], cli: &Cli) -> String {
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
        format!(
            "{}\n",
            serde_json::to_string_pretty(&serde_json::Value::Array(arr)).unwrap()
        )
    } else {
        results
            .iter()
            .map(|r| match &r.result {
                Ok(o) => render::outcome_human(o),
                Err(e) => format!("{}: {}", r.output.display(), render::error_human(e)),
            })
            .collect::<Vec<_>>()
            .join("")
    }
}
