use convkit_core::{plan, probe, registry, Backend, ConvError, ErrorCode, MediaProbe, Resolver};
use serde_json::json;

use crate::batch;
use crate::cli::Cli;
use crate::input;
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

    let (results, code) = batch::run(jobs, cli);
    print_results(&results, cli);
    code
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
    resolver
        .resolve(Backend::Ffprobe)
        .ok()
        .and_then(|p| probe::run(&p.path, &job.inputs[0]).ok())
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
fn dry_run(jobs: &[input::Job], cli: &Cli) -> i32 {
    let resolver = cli.resolver();
    let results: Vec<_> = jobs
        .iter()
        .map(|job| {
            let probed = probed_for(&resolver, job);
            plan::build(job.from, job.to, &job.inputs, &job.output, probed.as_ref())
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
/// own, fourth, incompatible `--json` success shape.
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
        let ok = results.iter().all(|r| r.result.is_ok());
        let envelope = json!({ "ok": ok, "results": arr });
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
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
    /// `None`.
    #[test]
    fn probed_for_runs_ffprobe_on_a_pair_that_might_remux() {
        let dir = tempfile::tempdir().unwrap();
        let stub = write_ffprobe_stub(dir.path());
        let mut r = Resolver::new();
        r.with_override(Backend::Ffprobe, stub);

        let j = job(
            convkit_core::Format::Mkv,
            convkit_core::Format::Mp4,
            "in.mkv",
            "out.mp4",
        );
        let probed = probed_for(&r, &j).expect("must probe a remuxable pair");
        assert_eq!(probed.video_codec.as_deref(), Some("h264"));
        assert_eq!(probed.audio_codec.as_deref(), Some("aac"));
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
}
