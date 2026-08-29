use std::path::PathBuf;

use rayon::prelude::*;

use convkit_core::{exec, ConvError, ErrorCode, Outcome};

use crate::cli::Cli;
use crate::input::Job;

/// The outcome of one job in a batch: which input drove it, where its
/// output landed (or would have landed), and whether it succeeded.
pub struct JobResult {
    pub input: PathBuf,
    pub output: PathBuf,
    pub result: Result<Outcome, ConvError>,
}

fn num_cpus_or_one() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// A spinner labelled with whichever backend is currently running, shown
/// only for a single-job run (I8) — `bar` below already covers the
/// multi-job case, one tick per *completed* job, but that leaves a lone
/// conversion with no progress indication at all. A `md -> pdf` (pandoc
/// plus a LibreOffice cold start, several seconds) used to show nothing
/// until it either finished or failed. Suppressed under `--quiet` and
/// `--json`, exactly like `bar`.
fn single_job_spinner(cli: &Cli, job_count: usize) -> Option<indicatif::ProgressBar> {
    if cli.quiet || cli.json || job_count != 1 {
        return None;
    }
    let pb = indicatif::ProgressBar::new_spinner();
    pb.set_style(
        indicatif::ProgressStyle::default_spinner()
            .template("{spinner} {msg}")
            .expect("static template is valid"),
    );
    pb.enable_steady_tick(std::time::Duration::from_millis(100));
    pb.set_message("starting…");
    Some(pb)
}

/// The batch exit-code rule: 0 if every job succeeded, the underlying
/// error's own code if every job failed (so a batch that failed only
/// because a backend is missing still exits 3), or `BatchPartialFailure`
/// (4) on a mixed result. Factored out of `run` so `commands/convert.rs`'s
/// install-and-retry prompt (Part 1) can recompute this after splicing
/// retried results back into an already-reported batch, without
/// reimplementing the same rule a second time.
pub fn exit_code(results: &[JobResult]) -> i32 {
    let failures = results.iter().filter(|r| r.result.is_err()).count();
    match (failures, results.len()) {
        (0, _) => 0,
        (f, n) if f == n => results[0].result.as_ref().unwrap_err().code.exit_code(),
        _ => ErrorCode::BatchPartialFailure.exit_code(),
    }
}

/// Runs every job, in parallel, on a shared `Resolver` (built once so its
/// per-backend resolution cache is shared across the whole batch rather than
/// re-probed per job). Returns each job's result alongside the process exit
/// code (see `exit_code`).
pub fn run(jobs: Vec<Job>, cli: &Cli) -> (Vec<JobResult>, i32) {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cli.jobs.unwrap_or_else(num_cpus_or_one))
        .build()
        .expect("thread pool");

    let bar = (!cli.quiet && !cli.json && jobs.len() > 1)
        .then(|| indicatif::ProgressBar::new(jobs.len() as u64));
    let spinner = single_job_spinner(cli, jobs.len());
    let resolver = cli.resolver();

    let results: Vec<JobResult> = pool.install(|| {
        jobs.into_par_iter()
            .map(|job| {
                // I5: `exec::run` now enforces this same refusal itself
                // (`Request::overwrite`), so this is a fast path — skipping
                // backend resolution and a scratch directory entirely for
                // the common case — not the only place it's checked.
                let result = if job.output.exists() && !cli.overwrite {
                    Err(ConvError::new(
                        ErrorCode::OutputExists,
                        format!("{} exists; pass -y to overwrite", job.output.display()),
                    ))
                } else {
                    let req = exec::Request {
                        from: job.from,
                        to: job.to,
                        inputs: job.inputs.clone(),
                        output: job.output.clone(),
                        overwrite: cli.overwrite,
                    };
                    // I8: the `Event` channel used to be threaded all the
                    // way through with a no-op consumer everywhere —
                    // `exec::run`'s signature promised progress reporting
                    // that nothing ever rendered. This is the one real
                    // consumer: labels `spinner` with the currently
                    // running backend (and which step, for a multi-step
                    // recipe like `md -> pdf`) as each one starts.
                    let mut on_event = |e: exec::Event| {
                        let Some(pb) = &spinner else { return };
                        if let exec::Event::StepStarted {
                            backend,
                            index,
                            total,
                        } = e
                        {
                            let name = backend.exe_name();
                            if total > 1 {
                                pb.set_message(format!(
                                    "running {name} (step {}/{total})…",
                                    index + 1
                                ));
                            } else {
                                pb.set_message(format!("running {name}…"));
                            }
                        }
                    };
                    exec::run(&req, &resolver, &mut on_event)
                };
                if let Some(b) = &bar {
                    b.inc(1);
                }
                JobResult {
                    input: job.inputs[0].clone(),
                    output: job.output,
                    result,
                }
            })
            .collect()
    });
    if let Some(b) = bar {
        b.finish_and_clear();
    }
    if let Some(pb) = spinner {
        pb.finish_and_clear();
    }

    let code = exit_code(&results);
    (results, code)
}
