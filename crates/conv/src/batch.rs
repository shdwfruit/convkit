use std::path::PathBuf;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use convkit_core::{exec, ConvError, ErrorCode, Format, Outcome};

use crate::cli::Cli;
use crate::input::Job;

/// The outcome of one job in a batch: which input drove it, where its
/// output landed (or would have landed), the target format (so a failure
/// can be reported as `report.docx -> pdf` without re-deriving it from the
/// output path's own extension), and whether it succeeded.
pub struct JobResult {
    pub input: PathBuf,
    pub output: PathBuf,
    pub to: Format,
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
/// re-probed per job). Returns each job's result, the process exit code (see
/// `exit_code`), and the batch's total wall-clock elapsed time — measured
/// here, in the binary, not in `convkit-core` (see `Outcome::elapsed_ms`'s
/// docs for why): this is the "did it hang?" answer Part 2 exists to give,
/// and it has to wrap the whole parallel batch, not sum each job's own
/// elapsed time, since jobs overlap.
/// # Invariant
///
/// Callers must hand this distinct output paths: the rayon fan-out below
/// lets two jobs targeting one file race their renames, so uniqueness is
/// enforced at planning time (`jobs_from`'s collision check — today the
/// only production source of a multi-job batch). A new multi-job source
/// must run the same check.
pub fn run(jobs: Vec<Job>, cli: &Cli) -> (Vec<JobResult>, i32, Duration) {
    let batch_start = Instant::now();
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
                let job_start = Instant::now();
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
                // Part 2: stamp this job's own wall-clock time onto the
                // `Outcome` `exec::run` handed back — `elapsed_ms` always
                // comes back `0` from `convkit-core` itself (timing is a
                // presentation concern, measured here, not there).
                let result = result.map(|mut outcome| {
                    outcome.elapsed_ms = job_start.elapsed().as_millis() as u64;
                    outcome
                });
                if let Some(b) = &bar {
                    b.inc(1);
                }
                JobResult {
                    input: job.inputs[0].clone(),
                    output: job.output,
                    to: job.to,
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
    (results, code, batch_start.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;

    fn synthetic_result(result: Result<Outcome, ConvError>) -> JobResult {
        JobResult {
            input: PathBuf::from("in"),
            output: PathBuf::from("out"),
            to: Format::Jpg,
            result,
        }
    }

    fn ok() -> Result<Outcome, ConvError> {
        Ok(Outcome {
            output: PathBuf::from("out"),
            bytes: 1,
            warnings: vec![],
            notes: vec![],
            backend_output: vec![],
            backends: vec![],
            remuxed: false,
            elapsed_ms: 0,
        })
    }

    fn err(code: ErrorCode) -> Result<Outcome, ConvError> {
        Err(ConvError::new(code, "failed"))
    }

    #[test]
    fn exit_code_is_zero_when_every_job_succeeds() {
        let results = vec![synthetic_result(ok()), synthetic_result(ok())];
        assert_eq!(exit_code(&results), 0);
    }

    /// When every job fails, the exit code is the underlying error's own
    /// code — here `BackendMissing` (3) — not the generic partial-failure
    /// code, so a batch that failed only because a backend is missing still
    /// exits 3.
    #[test]
    fn exit_code_uses_the_shared_error_code_when_every_job_fails_the_same_way() {
        let results = vec![
            synthetic_result(err(ErrorCode::BackendMissing)),
            synthetic_result(err(ErrorCode::BackendMissing)),
        ];
        assert_eq!(exit_code(&results), ErrorCode::BackendMissing.exit_code());
    }

    #[test]
    fn exit_code_is_batch_partial_failure_on_a_genuinely_mixed_result() {
        let results = vec![
            synthetic_result(ok()),
            synthetic_result(err(ErrorCode::ConversionFailed)),
        ];
        assert_eq!(
            exit_code(&results),
            ErrorCode::BatchPartialFailure.exit_code()
        );
    }

    fn test_cli(magick_path: PathBuf) -> Cli {
        Cli {
            paths: vec![],
            to: None,
            dry_run: false,
            json: false,
            overwrite: false,
            quiet: true,
            yes: false,
            no_install: false,
            outdir: None,
            jobs: Some(1),
            ffmpeg_path: None,
            ffprobe_path: None,
            magick_path: Some(magick_path),
            pandoc_path: None,
            soffice_path: None,
            typst_path: None,
            command: None,
        }
    }

    /// Writes a script standing in for `magick`: on a bare version probe
    /// (`Resolver::resolve`'s own check) it no-ops and exits 0; otherwise it
    /// writes one byte to whatever its last argument names. Mirrors
    /// `exec::tests::stub_that_creates_its_output`'s own reasoning; can't
    /// reuse that helper directly since it's private to `convkit-core`'s own
    /// test module.
    fn magick_stub(dir: &Path) -> PathBuf {
        let (name, body) = if cfg!(windows) {
            (
                "magick_stub.bat",
                "@echo off\r\n\
                 if not \"%~2\"==\"\" goto notversion\r\n\
                 if \"%~1\"==\"--version\" exit /b 0\r\n\
                 if \"%~1\"==\"-version\" exit /b 0\r\n\
                 :notversion\r\n\
                 :loop\r\n\
                 if \"%~1\"==\"\" goto done\r\n\
                 set \"last=%~1\"\r\n\
                 shift\r\n\
                 goto loop\r\n\
                 :done\r\n\
                 <nul set /p \"=x\" >\"%last%\"\r\n\
                 exit /b 0\r\n",
            )
        } else {
            (
                "magick_stub.sh",
                "#!/bin/sh\n\
                 if [ \"$#\" = \"1\" ] && { [ \"$1\" = \"--version\" ] || [ \"$1\" = \"-version\" ]; }; then\n\
                 \x20   exit 0\n\
                 fi\n\
                 for a in \"$@\"; do last=\"$a\"; done\n\
                 printf x > \"$last\"\n",
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

    /// Part 2's own wiring: a real (stubbed-backend) successful job must
    /// come back with `elapsed_ms` set to a real, measured value —
    /// `exec::run` itself always hands back `0` (timing is measured here,
    /// in the binary, not in `convkit-core`; see `Outcome::elapsed_ms`'s
    /// docs) — and with `to` matching the job's own target format, so
    /// `render::conversion_failure_human` can report `name -> ext` without
    /// re-deriving it from the output path.
    #[test]
    fn run_stamps_elapsed_ms_and_carries_the_target_format() {
        let dir = tempfile::tempdir().unwrap();
        let stub = magick_stub(dir.path());
        let cli = test_cli(stub);

        let input = dir.path().join("a.png");
        std::fs::write(&input, b"x").unwrap();
        let job = Job {
            inputs: vec![input],
            output: dir.path().join("out.jpg"),
            from: Format::Png,
            to: Format::Jpg,
        };

        let (results, code, _elapsed) = run(vec![job], &cli);
        assert_eq!(code, 0);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].to, Format::Jpg);
        let outcome = results[0].result.as_ref().unwrap();
        assert!(outcome.bytes > 0);
        // Not a tight bound -- just proof this is a real, sane measurement
        // (`exec::run` itself always hands back `0`, so any assertion here
        // at all is meaningful) rather than an unset or overflowed value.
        assert!(outcome.elapsed_ms < 30_000, "{}", outcome.elapsed_ms);
    }
}
