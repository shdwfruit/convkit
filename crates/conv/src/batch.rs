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

/// Runs every job, in parallel, on a shared `Resolver` (built once so its
/// per-backend resolution cache is shared across the whole batch rather than
/// re-probed per job). Returns each job's result alongside the process exit
/// code: 0 if every job succeeded, the underlying error's code if every job
/// failed (so a batch that failed only because a backend is missing still
/// exits 3), or `BatchPartialFailure` (4) on a mixed result.
pub fn run(jobs: Vec<Job>, cli: &Cli) -> (Vec<JobResult>, i32) {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cli.jobs.unwrap_or_else(num_cpus_or_one))
        .build()
        .expect("thread pool");

    let bar = (!cli.quiet && !cli.json && jobs.len() > 1)
        .then(|| indicatif::ProgressBar::new(jobs.len() as u64));
    let resolver = cli.resolver();

    let results: Vec<JobResult> = pool.install(|| {
        jobs.into_par_iter()
            .map(|job| {
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
                    };
                    exec::run(&req, &resolver, &mut |_| {})
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

    let failures = results.iter().filter(|r| r.result.is_err()).count();
    let code = match (failures, results.len()) {
        (0, _) => 0,
        (f, n) if f == n => results[0].result.as_ref().unwrap_err().code.exit_code(),
        _ => ErrorCode::BatchPartialFailure.exit_code(),
    };
    (results, code)
}
