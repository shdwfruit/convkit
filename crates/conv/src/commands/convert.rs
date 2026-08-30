use std::time::{Duration, Instant};

use convkit_core::{
    plan, probe, registry, AvailableBackends, Backend, ConvError, ErrorCode, MediaProbe, Outcome,
    Resolver,
};
use serde_json::json;

use crate::batch;
use crate::cli::Cli;
use crate::commands::install;
use crate::input;
use crate::install_prompt;
use crate::render;

pub fn run(cli: &Cli) -> i32 {
    let jobs = match input::plan_jobs(cli) {
        Ok(jobs) => jobs,
        Err(e) => {
            render::print_error(cli.json, &e);
            return e.code.exit_code();
        }
    };

    if cli.dry_run {
        return dry_run(&jobs, cli);
    }

    // Kept alongside `results` so a retry below can re-run exactly the jobs
    // that failed on a missing backend — `JobResult` alone has nothing to
    // hand back to `batch::run`, only what came out of it.
    let original_jobs = jobs.clone();
    let (mut results, mut code, mut elapsed) = batch::run(jobs, cli);

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
                    let (retry_results, _, _) = batch::run(retry_jobs, cli);
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
                    render::print_error(cli.json, &e);
                    elapsed += redo_start.elapsed();
                }
            }
        }
    }

    print_results(&results, cli, elapsed);
    code
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
/// stream copy — mirrors `exec::run`'s own gate exactly (`registry::
/// needs_probe`), so `--dry-run` and the real run it previews always agree
/// on whether a probe happens at all. `ffprobe` missing, or the probe itself
/// failing, is swallowed into `None` here exactly as it is in `exec::run`:
/// both conservatively fall back to a transcode preview rather than turning
/// "no ffprobe" into its own dry-run failure mode.
fn probed_for(resolver: &Resolver, job: &input::Job) -> Option<MediaProbe> {
    if !registry::needs_probe(job.from, job.to) {
        return None;
    }
    // `--dry-run` is documented as inert, but ffprobe honours URLs and
    // device paths, so probing the raw positional would turn a preview of
    // `http://…/x.mkv` into a real outbound fetch. Mirror `exec::run`'s
    // own input gate: only an existing regular file is ever probed, and
    // anything else falls back to the conservative transcode preview the
    // no-probe path already produces.
    if !job.inputs[0].is_file() {
        return None;
    }
    resolver
        .resolve(Backend::Ffprobe)
        .ok()
        .and_then(|p| probe::run(&p.path, &job.inputs[0]).ok())
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
/// C3: probes first on any pair that might remux (`probed_for`, gated on
/// `registry::needs_probe` exactly like `exec::run`), so the preview shown
/// here is the *exact* command a real run would use — not the conservative
/// transcode `plan::build` falls back to with no probe. `plan::build`
/// itself stays pure; the probe runs here, in the caller, and its result is
/// passed in, the same split `exec::run` already uses between itself and
/// `plan::build`.
///
/// Task 2 applies the identical lesson to backend availability: a docx/odt
/// -> pdf dry-run must preview the pandoc+typst command when soffice is
/// absent, not the (unusable) soffice one — `available_for` (gated on
/// `registry::has_fallback` exactly like `exec::run`'s own check) is what
/// makes that true.
fn dry_run(jobs: &[input::Job], cli: &Cli) -> i32 {
    let resolver = cli.resolver();
    let results: Vec<_> = jobs
        .iter()
        .map(|job| {
            let probed = probed_for(&resolver, job);
            let available = available_for(&resolver, job);
            plan::build(
                job.from,
                job.to,
                &job.inputs,
                &job.output,
                probed.as_ref(),
                available.as_ref(),
            )
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
fn print_results(results: &[batch::JobResult], cli: &Cli, elapsed: Duration) {
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
        let envelope = json!({ "ok": ok, "results": arr });
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
        return;
    }

    let styled_out = render::stdout_styled();
    let styled_err = render::stderr_styled();

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
                eprint!("{}", render::conversion_notes_human("", o, styled_err));
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
                err.push_str(&render::conversion_notes_human(&label, o, styled_err));
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
    use std::io::Write;
    use std::path::{Path, PathBuf};

    /// Writes a stub standing in for `ffprobe`. Responds to a bare
    /// `-version` probe (as `Resolver::resolve` issues on every backend it
    /// finds) with a no-op success, and to anything else — the real
    /// `-v quiet -print_format json -show_streams <input>` invocation
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
        let probed = probed_for(&r, &j).expect("must probe a remuxable pair");
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
            assert!(probed_for(&r, &j).is_none(), "{input} must not be probed");
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
        assert!(probed_for(&r, &j).is_none());
    }

    // --- Task 2: available_for -----------------------------------------------

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
