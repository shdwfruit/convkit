use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::Serialize;

use crate::error::{ConvError, ErrorCode, Result};
use crate::{plan, probe, registry, Backend, Format, OutputMode, Resolver};

#[derive(Debug, Clone)]
pub struct Request {
    pub from: Format,
    pub to: Format,
    pub inputs: Vec<PathBuf>,
    pub output: PathBuf,
    /// I5: refuse-by-default is a product invariant (spec §8), and it must
    /// hold for every caller of `convkit-core`, not just the `conv` binary.
    /// `batch.rs` already checks this before ever building a `Request`, but
    /// that check lived *only* in the binary — `exec::run`'s own final
    /// `std::fs::rename` would silently clobber an existing destination on
    /// both Unix and Windows if a caller skipped it, and the planned v1.1
    /// `conv mcp` frontend consumes `convkit-core` directly and would have
    /// inherited that silent overwrite. `false` refuses; `true` permits it,
    /// matching `-y/--overwrite`.
    pub overwrite: bool,
}

#[derive(Debug, Clone)]
pub enum Event {
    StepStarted {
        index: usize,
        total: usize,
        backend: Backend,
    },
    StepFinished {
        index: usize,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub output: PathBuf,
    pub bytes: u64,
    pub warnings: Vec<String>,
    pub backends: Vec<(Backend, String)>,
    pub remuxed: bool,
    /// Wall-clock time this conversion took, in whole milliseconds. Always
    /// `0` coming out of `exec::run` itself: per the same split that keeps
    /// `convkit-core` free of printing and prompting (Part 1's invariant),
    /// timing-for-display is a presentation concern too, so it is measured
    /// by the caller -- the `conv` binary's `batch::run`, which wraps this
    /// call in an `Instant` and overwrites this field on the `Outcome` it
    /// gets back -- never by `exec::run` itself. Left as a plain field
    /// (rather than, say, an `Option`) so a direct `convkit-core` consumer
    /// that never sets it still gets a well-formed, honestly-zero value
    /// instead of an absent one.
    pub elapsed_ms: u64,
}

/// Uniquifies each conversion's scratch directory alongside the process id,
/// so two conversions racing inside one process (Task 12's rayon batch mode)
/// never land on the same scratch path.
static SCRATCH_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Creates a private scratch directory inside `dest_dir`, named
/// `.convkit-<pid>-<counter>`.
///
/// Every intermediate step file, the LibreOffice profile, and the
/// temp-named final output all live here instead of directly in the user's
/// destination directory. This is load-bearing, not tidiness: `soffice`
/// only ever gets `--outdir <scratch>`, never the user's real directory, so
/// it can neither overwrite nor be confused with a pre-existing file there
/// (e.g. converting `report.docx` into a directory that already holds an
/// older `report.pdf`), and two conversions writing into the same
/// destination directory can never collide on the same landing name.
fn make_scratch_dir(dest_dir: &Path) -> Result<PathBuf> {
    if !dest_dir.is_dir() {
        return Err(ConvError::new(
            ErrorCode::ConversionFailed,
            format!("output directory does not exist: {}", dest_dir.display()),
        ));
    }
    let pid = std::process::id();
    let n = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = dest_dir.join(format!(".convkit-{pid}-{n}"));
    std::fs::create_dir(&path).map_err(|e| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!(
                "cannot create scratch directory in {}: {e}",
                dest_dir.display()
            ),
        )
    })?;
    Ok(path)
}

/// Owns a conversion's scratch directory and removes it (recursively) on
/// drop, unconditionally. By the time `run` returns — success or error —
/// the real output has already been renamed out of the scratch directory if
/// it was ever going to exist at all, so everything the guard finds still
/// inside is genuinely disposable: intermediates, a populated LibreOffice
/// profile, or (on a failure partway through) a temp-named output that
/// never made it out.
///
/// This is what makes cleanup cover every return path — backend resolution
/// failure, spawn failure, a rename failure, even a panic — without needing
/// an explicit `cleanup()` call at each one.
struct ScratchGuard(PathBuf);

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs a conversion plan end to end: resolves each step's backend, spawns
/// it, verifies it actually produced output, and atomically renames the
/// result into place.
///
/// # Design note
///
/// `--dry-run` (Task 10) calls `plan::build` directly with the user's real
/// output path. This function instead builds a plan targeting a temp path
/// inside a private scratch directory and renames the final result out on
/// success — the two therefore render the same flags with different output
/// paths, which is intended: the rename is what makes Ctrl-C safe. A
/// missing or zero-byte result is always a failure regardless of exit code,
/// since `soffice` returns 0 on failure.
pub fn run(req: &Request, resolver: &Resolver, on_event: &mut dyn FnMut(Event)) -> Result<Outcome> {
    if req.inputs.is_empty() {
        return Err(ConvError::new(
            ErrorCode::InputNotFound,
            "no input files were given",
        ));
    }
    for input in &req.inputs {
        if !input.is_file() {
            return Err(ConvError::new(
                ErrorCode::InputNotFound,
                format!("input not found: {}", input.display()),
            ));
        }
    }

    // I5: enforced here, not only by the CLI's own fast-path check in
    // `batch.rs`, so refuse-by-default (spec §8) holds for every caller of
    // this crate — including a future `conv mcp` frontend that would
    // otherwise inherit a silent overwrite. Checked early, before any work
    // (probing, a scratch directory, an actual conversion) is done, so a
    // refusal is cheap rather than discarding a completed transcode.
    if req.output.exists() && !req.overwrite {
        return Err(ConvError::new(
            ErrorCode::OutputExists,
            format!("{} exists; pass -y to overwrite", req.output.display()),
        ));
    }

    // Probe only when a stream copy is even possible for this pair.
    let probed = if registry::needs_probe(req.from, req.to) {
        resolver
            .resolve(Backend::Ffprobe)
            .ok()
            .and_then(|p| probe::run(&p.path, &req.inputs[0]).ok())
    } else {
        None
    };

    // Likewise: only check backend availability when this pair actually has
    // more than one recipe to choose between (today: docx/odt -> pdf). An
    // ordinary conversion never pays for the extra resolve() calls this
    // would otherwise cost.
    let available = if registry::has_fallback(req.from, req.to) {
        Some(resolver.check_availability(registry::FALLBACK_BACKENDS))
    } else {
        None
    };

    let dest_dir = req
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let scratch = make_scratch_dir(dest_dir)?;
    // Lives for the rest of this function; its `Drop` removes `scratch`
    // (and everything left inside it) on every exit path.
    let _guard = ScratchGuard(scratch.clone());

    let final_name = req.output.file_name().ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("output path has no file name: {}", req.output.display()),
        )
    })?;
    let temp_final = scratch.join(final_name);

    let built = plan::build(
        req.from,
        req.to,
        &req.inputs,
        &temp_final,
        probed.as_ref(),
        available.as_ref(),
    )?;
    let first_step = built
        .steps
        .first()
        .ok_or_else(|| ConvError::new(ErrorCode::ConversionFailed, "recipe has no steps"))?;
    let remuxed = first_step.argv.windows(2).any(|w| w == ["-c", "copy"]);

    let total = built.steps.len();
    let mut backends = Vec::new();
    // Tracks the stem of whatever this step's actual input file is: the
    // request's first input for step 0, the previous step's located output
    // for every step after. `soffice` names its OutDir result after this.
    let mut current_input_stem: OsString = req.inputs[0]
        .file_stem()
        .map(|s| s.to_os_string())
        .unwrap_or_default();

    for (i, step) in built.steps.iter().enumerate() {
        on_event(Event::StepStarted {
            index: i,
            total,
            backend: step.backend,
        });
        let resolved = resolver.resolve(step.backend)?;

        let mut cmd = Command::new(&resolved.path);

        // Constraint: every soffice invocation gets its own isolated
        // profile, and now it lives inside the scratch directory too — one
        // `remove_dir_all` on `scratch` cleans up the profile along with
        // everything else, populated or not.
        if step.backend == Backend::Soffice {
            let profile = scratch.join(format!("lo-profile-{i}"));
            let url = user_installation_url(&profile)?;
            // `plan::build` already inserted `plan::USER_INSTALLATION_
            // PLACEHOLDER` as this step's first argv element specifically
            // so `--dry-run` shows this flag at all (I1) — it just can't
            // know the real per-run scratch profile path at plan time.
            // Substitute the real, isolated URL in for that placeholder
            // here, rather than prepending a second copy, so the argv this
            // process actually receives and the argv `--dry-run` printed
            // differ only in this one token's value, never in count or
            // order.
            debug_assert_eq!(
                step.argv.first().map(String::as_str),
                Some(plan::USER_INSTALLATION_PLACEHOLDER),
                "plan::build must always emit the placeholder as a Soffice \
                 step's first argv element"
            );
            cmd.arg(format!("-env:UserInstallation={url}"));
            let rest = substitute_backend_paths(&step.argv[1..], resolver)?;
            cmd.args(&rest);
        } else {
            let argv = substitute_backend_paths(&step.argv, resolver)?;
            cmd.args(&argv);
        }

        let out = cmd.output().map_err(|e| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("failed to run {}: {e}", resolved.path.display()),
            )
        })?;

        // `step.output` is the path exec must end up with for this step. It
        // is never derived from argv: `soffice` recipes' argv ends with the
        // *input* path, not the output.
        let declared = step.output.clone();

        let produced = match step.output_mode {
            OutputMode::Path => declared.clone(),
            OutputMode::OutDir => {
                let dir = declared
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                let want_ext = declared.extension().and_then(|e| e.to_str()).unwrap_or("");
                // A lookup failure (nothing matching found) falls back to
                // `declared`, which is guaranteed not to exist yet, so the
                // is_non_empty check below uniformly reports it as "produced
                // no output" without a separate error path.
                locate_outdir_result(&current_input_stem, want_ext, dir)
                    .unwrap_or_else(|_| declared.clone())
            }
        };

        if !out.status.success() || !is_non_empty(&produced) {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // `.rev().take(3)` alone selects the *last* three lines but
            // leaves them in reverse (bottom-up) order; `.rev()` again
            // after `.take(3)` restores the original top-down reading
            // order without changing which three lines were picked.
            let tail: String = stderr
                .lines()
                .rev()
                .take(3)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ConvError {
                code: ErrorCode::ConversionFailed,
                message: if is_non_empty(&produced) {
                    format!("{} failed: {tail}", step.backend.exe_name())
                } else {
                    // soffice exits 0 on failure, so this branch is load-bearing.
                    format!("{} produced no output. {tail}", step.backend.exe_name())
                },
                backend: Some(step.backend),
                remediation: None,
            });
        }

        if produced != declared {
            std::fs::rename(&produced, &declared).map_err(io_err)?;
        }
        current_input_stem = declared
            .file_stem()
            .map(|s| s.to_os_string())
            .unwrap_or_default();
        backends.push((step.backend, resolved.version));
        on_event(Event::StepFinished { index: i });
    }

    let bytes = std::fs::metadata(&temp_final).map_err(io_err)?.len();
    std::fs::rename(&temp_final, &req.output).map_err(io_err)?;

    Ok(Outcome {
        output: req.output.clone(),
        bytes,
        warnings: built.warnings,
        backends,
        remuxed,
        elapsed_ms: 0,
    })
}

fn is_non_empty(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false)
}

/// Every backend `Arg::BackendPath` could possibly name. Small and fixed —
/// unlike the soffice `-env:UserInstallation` placeholder (a single fixed
/// *position*, argv[0] of a `Soffice` step), a `BackendPath` placeholder can
/// appear anywhere in a step's argv and for any backend, so substitution
/// here is a token-by-token scan against this list rather than a positional
/// swap. Mirrors the local `BACKENDS` list `conv`'s own `doctor` command
/// already keeps for the same "every backend, enumerated by hand" need.
const KNOWN_BACKENDS: &[Backend] = &[
    Backend::Ffmpeg,
    Backend::Ffprobe,
    Backend::Magick,
    Backend::Soffice,
    Backend::Pandoc,
    Backend::Typst,
];

/// Substitutes the real, resolved absolute path for every
/// `Backend::path_placeholder()` token in `argv`, resolving each named
/// backend the first time its placeholder is seen (`Resolver::resolve`
/// caches, so a backend that's both substituted here and separately
/// resolved as the step's own backend is never probed twice). A step whose
/// recipe needs a backend this way that turns out to be missing surfaces
/// the same `backend_missing` error naming that backend that any other
/// resolution failure would.
fn substitute_backend_paths(argv: &[String], resolver: &Resolver) -> Result<Vec<OsString>> {
    let mut out = Vec::with_capacity(argv.len());
    for tok in argv {
        let named = KNOWN_BACKENDS.iter().find(|b| *tok == b.path_placeholder());
        match named {
            Some(&backend) => out.push(resolver.resolve(backend)?.path.into_os_string()),
            None => out.push(OsString::from(tok)),
        }
    }
    Ok(out)
}

/// In OutDir mode the backend names the file `<input-stem>.<ext>` itself.
/// Match on the input's stem plus the wanted extension — not just "the
/// newest file with this extension" — so a directory that already holds an
/// unrelated file of the same type is never mistaken for our result. Mtime
/// only breaks a genuine tie between multiple matches. Only regular files
/// are considered: `read_dir` yields directories too, and on some platforms
/// a directory's reported length is nonzero, which would otherwise let
/// `is_non_empty` wave a directory through as if it were the output.
fn locate_outdir_result(input_stem: &OsStr, want_ext: &str, dir: &Path) -> Result<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).map_err(io_err)?.flatten() {
        let p = entry.path();
        if p.file_stem() != Some(input_stem) {
            continue;
        }
        if p.extension().and_then(|e| e.to_str()) != Some(want_ext) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(mtime) = meta.modified() else { continue };
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, p));
        }
    }
    best.map(|(_, p)| p).ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!(
                "backend wrote no {}.{want_ext} file into {}",
                input_stem.to_string_lossy(),
                dir.display()
            ),
        )
    })
}

fn io_err(e: std::io::Error) -> ConvError {
    ConvError::new(ErrorCode::ConversionFailed, e.to_string())
}

/// Percent-encodes every byte outside the RFC 3986 "unreserved" set
/// (`A-Za-z0-9-_.~`), except `/` and `:`, which stay literal — they are the
/// path and drive-letter separators this URL still needs to parse. Encoding
/// per byte (not per `char`) keeps this correct for non-ASCII UTF-8.
fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Builds a well-formed, percent-encoded `file://` URL for
/// `-env:UserInstallation`. The profile directory need not exist yet —
/// LibreOffice creates it — so this uses `std::path::absolute`, which never
/// touches the filesystem.
///
/// Two failure modes a naive `format!("file://{}", path.display())` has on
/// Windows: wrong slash count and backslash separators
/// (`file://C:\Users\...`, which LibreOffice rejects or silently ignores),
/// and an unescaped space — a genuinely common case, since a Windows
/// username with a space in it (e.g. `C:\Users\Test User`) is entirely
/// ordinary, and a raw space in the URL is equally malformed and equally
/// silently ignored, losing exactly the profile isolation this flag exists
/// to provide. Both are Windows-only failures Linux CI never catches, on
/// the one backend that can never be auto-installed.
///
/// `pub(crate)` because `resolve.rs`'s soffice version probe needs the same
/// well-formed-URL logic — it is itself a soffice invocation and gets the
/// same isolated-profile treatment as a real conversion.
pub(crate) fn user_installation_url(profile: &Path) -> Result<String> {
    let abs = std::path::absolute(profile).map_err(io_err)?;
    #[cfg(windows)]
    let path_part = format!("/{}", abs.to_string_lossy().replace('\\', "/"));
    #[cfg(not(windows))]
    let path_part = abs.to_string_lossy().into_owned();
    Ok(format!("file://{}", percent_encode_path(&path_part)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Writes a tiny script that copies argv's last element into existence,
    /// with one exception: invoked with exactly one argument that is
    /// `--version` or `-version`, it does nothing and exits 0 — mirroring
    /// what a real backend's version flag does. `resolve()` calls
    /// `--version`/`-version` on whatever path it resolves, including these
    /// stubs when a test overrides a backend with one, so without this
    /// exception the version probe alone would make even a "do nothing"
    /// invocation write a file named `-version` (this is exactly how the
    /// stray `crates/convkit-core/-version` file from the first round of
    /// this task was found — see the amended task-8-report.md).
    ///
    /// The brief's original Windows stub only shifted through argv and never
    /// wrote anything — verified by hand against `cmd.exe` on this machine
    /// (see task-8-report.md). Fixed here: track the last argument across
    /// the shift loop, then write exactly one byte to it with no trailing
    /// newline (`<nul set /p "=x"` is the standard cmd.exe trick for that)
    /// and exit 0 explicitly, since `set /p` reading from `nul` otherwise
    /// leaves `%errorlevel%` at 1 and would make a successful stub look like
    /// a failed backend.
    fn stub_that_creates_its_output(dir: &Path) -> PathBuf {
        let (name, body) = if cfg!(windows) {
            (
                "stub.bat",
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
                "stub.sh",
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

    /// Writes a script that does nothing and exits 0, regardless of argv —
    /// simulates a backend that silently fails to produce output despite a
    /// clean exit code.
    fn stub_that_writes_nothing(dir: &Path) -> PathBuf {
        let p = if cfg!(windows) {
            let p = dir.join("noop.bat");
            std::fs::write(&p, "@echo off\r\n").unwrap();
            p
        } else {
            let p = dir.join("noop.sh");
            std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
            p
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    /// Writes a script that prints four numbered lines to stderr, in order,
    /// and exits non-zero without writing any output file — for testing
    /// that the failure message's stderr tail reads top-down, not
    /// bottom-up (the minor stderr-order fix).
    fn stub_that_fails_with_ordered_multiline_stderr(dir: &Path) -> PathBuf {
        let (name, body) = if cfg!(windows) {
            (
                "multiline_stderr.bat",
                "@echo off\r\n\
                 echo line-one 1>&2\r\n\
                 echo line-two 1>&2\r\n\
                 echo line-three 1>&2\r\n\
                 echo line-four 1>&2\r\n\
                 exit /b 1\r\n",
            )
        } else {
            (
                "multiline_stderr.sh",
                "#!/bin/sh\n\
                 echo line-one >&2\n\
                 echo line-two >&2\n\
                 echo line-three >&2\n\
                 echo line-four >&2\n\
                 exit 1\n",
            )
        };
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    /// Writes a script simulating `soffice --headless --convert-to <ext>
    /// --outdir <dir> <input>`: it names its output `<input-stem>.pdf` in
    /// whatever directory follows `--outdir`, ignoring every other
    /// argument (including the `-env:UserInstallation=...` exec injects
    /// first). Like `stub_that_creates_its_output`, it no-ops on a bare
    /// version probe.
    fn outdir_stub_that_writes_pdf(dir: &Path) -> PathBuf {
        let (name, body) = if cfg!(windows) {
            (
                "outdir_stub.bat",
                "@echo off\r\n\
                 if not \"%~2\"==\"\" goto notversion\r\n\
                 if \"%~1\"==\"--version\" exit /b 0\r\n\
                 if \"%~1\"==\"-version\" exit /b 0\r\n\
                 :notversion\r\n\
                 set \"outdir=\"\r\n\
                 :loop\r\n\
                 if \"%~1\"==\"\" goto done\r\n\
                 if \"%~1\"==\"--outdir\" goto capture_outdir\r\n\
                 set \"last=%~1\"\r\n\
                 shift\r\n\
                 goto loop\r\n\
                 :capture_outdir\r\n\
                 shift\r\n\
                 set \"outdir=%~1\"\r\n\
                 shift\r\n\
                 goto loop\r\n\
                 :done\r\n\
                 for %%F in (\"%last%\") do set \"stem=%%~nF\"\r\n\
                 <nul set /p \"=x\" >\"%outdir%\\%stem%.pdf\"\r\n\
                 exit /b 0\r\n",
            )
        } else {
            (
                "outdir_stub.sh",
                "#!/bin/sh\n\
                 if [ \"$#\" = \"1\" ] && { [ \"$1\" = \"--version\" ] || [ \"$1\" = \"-version\" ]; }; then\n\
                 \x20   exit 0\n\
                 fi\n\
                 outdir=\"\"\n\
                 last=\"\"\n\
                 prev=\"\"\n\
                 for a in \"$@\"; do\n\
                 \x20   if [ \"$prev\" = \"--outdir\" ]; then outdir=\"$a\"; fi\n\
                 \x20   last=\"$a\"\n\
                 \x20   prev=\"$a\"\n\
                 done\n\
                 stem=$(basename \"$last\")\n\
                 stem=\"${stem%.*}\"\n\
                 printf x > \"$outdir/$stem.pdf\"\n",
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

    /// Like `outdir_stub_that_writes_pdf`, but also records every argv
    /// token it received, one per line and in order, to `record_path` — so
    /// a test can inspect exactly what this process was invoked with,
    /// including the token at position 0 that `exec::run` is supposed to
    /// substitute the real `-env:UserInstallation=<url>` into (I1).
    fn outdir_stub_that_records_argv_and_writes_pdf(dir: &Path, record_path: &Path) -> PathBuf {
        let record = record_path.display();
        let (name, body) = if cfg!(windows) {
            (
                "outdir_record_stub.bat",
                format!(
                    "@echo off\r\n\
                     if not \"%~2\"==\"\" goto notversion\r\n\
                     if \"%~1\"==\"--version\" exit /b 0\r\n\
                     if \"%~1\"==\"-version\" exit /b 0\r\n\
                     :notversion\r\n\
                     set \"outdir=\"\r\n\
                     type nul > \"{record}\"\r\n\
                     :loop\r\n\
                     if \"%~1\"==\"\" goto done\r\n\
                     echo %~1>>\"{record}\"\r\n\
                     if \"%~1\"==\"--outdir\" goto capture_outdir\r\n\
                     set \"last=%~1\"\r\n\
                     shift\r\n\
                     goto loop\r\n\
                     :capture_outdir\r\n\
                     shift\r\n\
                     echo %~1>>\"{record}\"\r\n\
                     set \"outdir=%~1\"\r\n\
                     shift\r\n\
                     goto loop\r\n\
                     :done\r\n\
                     for %%F in (\"%last%\") do set \"stem=%%~nF\"\r\n\
                     <nul set /p \"=x\" >\"%outdir%\\%stem%.pdf\"\r\n\
                     exit /b 0\r\n"
                ),
            )
        } else {
            (
                "outdir_record_stub.sh",
                format!(
                    "#!/bin/sh\n\
                     if [ \"$#\" = \"1\" ] && {{ [ \"$1\" = \"--version\" ] || [ \"$1\" = \"-version\" ]; }}; then\n\
                     \x20   exit 0\n\
                     fi\n\
                     outdir=\"\"\n\
                     last=\"\"\n\
                     prev=\"\"\n\
                     : > \"{record}\"\n\
                     for a in \"$@\"; do\n\
                     \x20   echo \"$a\" >> \"{record}\"\n\
                     \x20   if [ \"$prev\" = \"--outdir\" ]; then outdir=\"$a\"; fi\n\
                     \x20   last=\"$a\"\n\
                     \x20   prev=\"$a\"\n\
                     done\n\
                     stem=$(basename \"$last\")\n\
                     stem=\"${{stem%.*}}\"\n\
                     printf x > \"$outdir/$stem.pdf\"\n"
                ),
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

    /// Every `.convkit-<pid>-<n>` scratch directory (if any) currently
    /// sitting directly inside `dir`.
    fn scratch_dirs_in(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".convkit-"))
            .collect()
    }

    /// Minor fix: the stderr tail must read top-down (natural reading
    /// order), not bottom-up. `.lines().rev().take(3)` alone correctly
    /// selects the *last* three lines but leaves them reversed; a second
    /// `.rev()` after `.take(3)` was missing.
    #[test]
    fn the_stderr_tail_reads_in_natural_top_to_bottom_order() {
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_that_fails_with_ordered_multiline_stderr(dir.path());
        let mut r = Resolver::new();
        r.with_override(Backend::Magick, stub);

        let input = dir.path().join("a.png");
        std::fs::write(&input, b"x").unwrap();
        let req = Request {
            from: Format::Png,
            to: Format::Jpg,
            inputs: vec![input],
            output: dir.path().join("out.jpg"),
            overwrite: false,
        };

        let e = run(&req, &r, &mut |_| {}).unwrap_err();
        // The stub writes four lines; only the last three are kept, and
        // they must appear in the order they were written: two, three, four.
        let two_at = e.message.find("line-two").unwrap_or_else(|| {
            panic!("stderr tail missing line-two: {}", e.message);
        });
        let three_at = e.message.find("line-three").unwrap_or_else(|| {
            panic!("stderr tail missing line-three: {}", e.message);
        });
        let four_at = e.message.find("line-four").unwrap_or_else(|| {
            panic!("stderr tail missing line-four: {}", e.message);
        });
        assert!(
            two_at < three_at && three_at < four_at,
            "stderr tail is out of order: {}",
            e.message
        );
        assert!(
            !e.message.contains("line-one"),
            "only the last three lines should be kept: {}",
            e.message
        );
    }

    #[test]
    fn a_backend_that_writes_nothing_is_a_failure_even_on_exit_zero() {
        let dir = tempfile::tempdir().unwrap();
        let noop = stub_that_writes_nothing(dir.path());
        let mut r = Resolver::new();
        r.with_override(Backend::Magick, noop);

        let input = dir.path().join("a.png");
        std::fs::write(&input, b"x").unwrap();
        let req = Request {
            from: Format::Png,
            to: Format::Jpg,
            inputs: vec![input],
            output: dir.path().join("out.jpg"),
            overwrite: false,
        };
        let e = run(&req, &r, &mut |_| {}).unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::ConversionFailed);
        assert!(e.message.contains("produced no output"), "{}", e.message);
    }

    #[test]
    fn a_successful_step_renames_into_place_and_reports_size() {
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_that_creates_its_output(dir.path());
        let mut r = Resolver::new();
        r.with_override(Backend::Magick, stub);

        let input = dir.path().join("a.png");
        std::fs::write(&input, b"x").unwrap();
        let output = dir.path().join("out.jpg");
        let req = Request {
            from: Format::Png,
            to: Format::Jpg,
            inputs: vec![input],
            output: output.clone(),
            overwrite: false,
        };

        let outcome = run(&req, &r, &mut |_| {}).unwrap();
        assert!(output.is_file(), "output must exist after rename");
        assert_eq!(outcome.bytes, 1);
        assert_eq!(outcome.output, output);
    }

    // --- I5: overwrite refusal must be enforced by core itself, not only
    // by the CLI's own fast-path check in batch.rs -------------------------

    /// The exact gap: `batch.rs`'s check is bypassed entirely here — this
    /// calls `exec::run` directly, the way a future `conv mcp` frontend
    /// consuming `convkit-core` directly would — and `run` must still
    /// refuse to clobber a pre-existing output when `Request::overwrite` is
    /// `false`, before ever touching the backend or the real destination
    /// file.
    #[test]
    fn run_refuses_to_clobber_an_existing_output_when_overwrite_is_false() {
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_that_creates_its_output(dir.path());
        let mut r = Resolver::new();
        r.with_override(Backend::Magick, stub);

        let input = dir.path().join("a.png");
        std::fs::write(&input, b"fresh input").unwrap();
        let output = dir.path().join("out.jpg");
        std::fs::write(&output, b"pre-existing output, must survive").unwrap();

        let req = Request {
            from: Format::Png,
            to: Format::Jpg,
            inputs: vec![input],
            output: output.clone(),
            overwrite: false,
        };

        let e = run(&req, &r, &mut |_| {}).unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::OutputExists);
        assert_eq!(
            std::fs::read(&output).unwrap(),
            b"pre-existing output, must survive",
            "the pre-existing output must be left untouched"
        );
    }

    /// The counterpart: `Request::overwrite: true` must still let a real
    /// run replace an existing output — this isn't a blanket refusal, only
    /// a default one.
    #[test]
    fn run_permits_clobbering_an_existing_output_when_overwrite_is_true() {
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_that_creates_its_output(dir.path());
        let mut r = Resolver::new();
        r.with_override(Backend::Magick, stub);

        let input = dir.path().join("a.png");
        std::fs::write(&input, b"fresh input").unwrap();
        let output = dir.path().join("out.jpg");
        std::fs::write(&output, b"stale output").unwrap();

        let req = Request {
            from: Format::Png,
            to: Format::Jpg,
            inputs: vec![input],
            output: output.clone(),
            overwrite: true,
        };

        run(&req, &r, &mut |_| {}).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"x");
    }

    #[test]
    fn no_temp_files_survive_a_successful_run() {
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_that_creates_its_output(dir.path());
        let mut r = Resolver::new();
        r.with_override(Backend::Magick, stub);
        let input = dir.path().join("a.png");
        std::fs::write(&input, b"x").unwrap();
        let req = Request {
            from: Format::Png,
            to: Format::Jpg,
            inputs: vec![input],
            output: dir.path().join("out.jpg"),
            overwrite: false,
        };
        run(&req, &r, &mut |_| {}).unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("convkit-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "left temp files behind: {leftovers:?}"
        );
    }

    // --- Controller review round 2: the data-loss bug and its guards ------

    /// CRITICAL fix + REQUIRED TEST. Before the scratch-directory fix,
    /// `soffice` was handed the user's real destination directory as
    /// `--outdir` scratch space, so `locate_outdir_result` could not tell
    /// "the backend just wrote this" from "this was already here": a
    /// pre-existing `report.pdf` sitting next to a fresh `report.docx`
    /// conversion could get overwritten by soffice and then renamed away as
    /// if it were the real result, silently destroying the user's file.
    ///
    /// This drives `run()` through a full OutDir recipe (docx → pdf) with a
    /// decoy `report.pdf` already in the destination directory sharing the
    /// input's stem — precisely the scenario above — and asserts the decoy
    /// is untouched, the real output is correct, and no scratch directory
    /// survives.
    #[test]
    fn outdir_recipe_never_touches_a_decoy_already_in_the_destination_directory() {
        let dir = tempfile::tempdir().unwrap();
        let stub = outdir_stub_that_writes_pdf(dir.path());
        let mut r = Resolver::new();
        r.with_override(Backend::Soffice, stub);

        let input = dir.path().join("report.docx");
        std::fs::write(&input, b"docx-bytes").unwrap();

        let decoy = dir.path().join("report.pdf");
        std::fs::write(&decoy, b"do not touch me").unwrap();

        let output = dir.path().join("final.pdf");
        let req = Request {
            from: Format::Docx,
            to: Format::Pdf,
            inputs: vec![input],
            output: output.clone(),
            overwrite: false,
        };

        let outcome = run(&req, &r, &mut |_| {}).unwrap();

        assert_eq!(
            std::fs::read(&decoy).unwrap(),
            b"do not touch me",
            "the pre-existing decoy must be untouched"
        );
        assert!(output.is_file(), "the real output must exist");
        assert_eq!(std::fs::read(&output).unwrap(), b"x");
        assert_eq!(outcome.output, output);
        assert_eq!(outcome.bytes, 1);

        let leftovers = scratch_dirs_in(dir.path());
        assert!(
            leftovers.is_empty(),
            "left scratch directories behind: {leftovers:?}"
        );
    }

    /// REQUIRED TEST, second half: a failure partway through a multi-step
    /// recipe must still leave no scratch directory behind, including
    /// whatever an earlier, successful step already wrote into it. Drives
    /// `md → pdf` (pandoc succeeds and writes the intermediate docx into
    /// scratch; soffice then silently fails) and checks the whole scratch
    /// directory — intermediate included — is gone.
    #[test]
    fn a_mid_recipe_failure_leaves_no_scratch_directory_behind() {
        let dir = tempfile::tempdir().unwrap();
        let pandoc_stub = stub_that_creates_its_output(dir.path());
        let soffice_stub = stub_that_writes_nothing(dir.path());

        let mut r = Resolver::new();
        r.with_override(Backend::Pandoc, pandoc_stub);
        r.with_override(Backend::Soffice, soffice_stub);

        let input = dir.path().join("a.md");
        std::fs::write(&input, b"# hi").unwrap();
        let req = Request {
            from: Format::Md,
            to: Format::Pdf,
            inputs: vec![input],
            output: dir.path().join("out.pdf"),
            overwrite: false,
        };

        let e = run(&req, &r, &mut |_| {}).unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::ConversionFailed);

        let leftovers = scratch_dirs_in(dir.path());
        assert!(
            leftovers.is_empty(),
            "left scratch directories behind: {leftovers:?}"
        );
    }

    /// IMPORTANT 1: the `ScratchGuard` `Drop` impl, not two explicit
    /// `cleanup()` call sites, is what makes cleanup unconditional. Proven
    /// directly against a *populated* directory (mirroring a real
    /// LibreOffice profile's structure), not an empty one.
    #[test]
    fn scratch_guard_removes_a_populated_directory_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join(".convkit-test-scratch");
        std::fs::create_dir_all(scratch.join("user/config")).unwrap();
        std::fs::write(scratch.join("user/config/registrymodifications.xcu"), b"x").unwrap();
        assert!(scratch.is_dir());

        {
            let _guard = ScratchGuard(scratch.clone());
        }

        assert!(
            !scratch.exists(),
            "scratch directory must be removed recursively on drop"
        );
    }

    /// IMPORTANT 4: a same-named *directory* must never be mistaken for the
    /// backend's output — on some platforms a directory's reported length
    /// is nonzero, which would otherwise let `is_non_empty` wave it through.
    #[test]
    fn locate_outdir_result_ignores_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("report.pdf")).unwrap();

        let e = locate_outdir_result(OsStr::new("report"), "pdf", dir.path()).unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::ConversionFailed);
    }

    /// Amendment 4 / IMPORTANT 2: `-env:UserInstallation` must be a
    /// well-formed, percent-encoded `file://` URL. A naive
    /// `format!("file://{}", path.display())` on Windows produces
    /// `file://C:\Users\...` — wrong slash count, backslash separators —
    /// which LibreOffice rejects or silently ignores. Uses an
    /// already-absolute input so the test never depends on
    /// `std::env::current_dir()`.
    #[cfg(windows)]
    #[test]
    fn user_installation_url_is_a_well_formed_file_url_on_windows() {
        let profile = PathBuf::from(r"C:\Users\test\AppData\Local\Temp\convkit-lo-profile-0");
        let url = user_installation_url(&profile).unwrap();
        assert_eq!(
            url,
            "file:///C:/Users/test/AppData/Local/Temp/convkit-lo-profile-0"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn user_installation_url_is_a_well_formed_file_url_on_unix() {
        let profile = PathBuf::from("/tmp/convkit-lo-profile-0");
        let url = user_installation_url(&profile).unwrap();
        assert_eq!(url, "file:///tmp/convkit-lo-profile-0");
    }

    /// IMPORTANT 2: a raw space is just as malformed as a backslash, and a
    /// Windows username with a space in it (e.g. `C:\Users\Test User`) is
    /// entirely ordinary — this is a synthetic path, not this machine's own
    /// account name, since this repo publishes under the handle
    /// `shdwfruit` and a contributor's real name has no reason to appear in
    /// library source; a synthetic space proves the same regression.
    #[cfg(windows)]
    #[test]
    fn user_installation_url_percent_encodes_a_space_in_the_path() {
        let profile = PathBuf::from(r"C:\Users\Test User\AppData\Local\Temp\convkit-lo-profile-0");
        let url = user_installation_url(&profile).unwrap();
        assert_eq!(
            url,
            "file:///C:/Users/Test%20User/AppData/Local/Temp/convkit-lo-profile-0"
        );
        assert!(!url.contains(' '), "{url}");
    }

    #[cfg(not(windows))]
    #[test]
    fn user_installation_url_percent_encodes_a_space_in_the_path() {
        let profile = PathBuf::from("/tmp/test user/convkit-lo-profile-0");
        let url = user_installation_url(&profile).unwrap();
        assert_eq!(url, "file:///tmp/test%20user/convkit-lo-profile-0");
        assert!(!url.contains(' '), "{url}");
    }

    /// Amendment 3: `locate_outdir_result` must match on the input's stem,
    /// not just grab the newest file with a matching extension — otherwise
    /// converting into a directory that already holds an unrelated file can
    /// pick the wrong one. `unrelated.pdf` is written strictly after
    /// `report.pdf` so a "just pick the newest" implementation would return
    /// the wrong file; stem-matching must still return `report.pdf`.
    #[test]
    fn locate_outdir_result_matches_the_input_stem_not_just_the_newest_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("report.pdf"), b"correct").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(dir.path().join("unrelated.pdf"), b"decoy").unwrap();

        let found = locate_outdir_result(OsStr::new("report"), "pdf", dir.path()).unwrap();
        assert_eq!(found, dir.path().join("report.pdf"));
    }

    /// I1: the process actually invoked must receive the real, per-run,
    /// percent-encoded `-env:UserInstallation=file://...` URL as its first
    /// argument — never the literal placeholder text `plan::build` prints
    /// for `--dry-run` — proving `run()`'s substitution (not a second,
    /// separately-prepended flag) is what reaches the backend.
    #[test]
    fn the_real_soffice_invocation_substitutes_the_real_url_for_the_dry_run_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("argv-record.txt");
        let stub = outdir_stub_that_records_argv_and_writes_pdf(dir.path(), &record);
        let mut r = Resolver::new();
        r.with_override(Backend::Soffice, stub);

        let input = dir.path().join("report.docx");
        std::fs::write(&input, b"docx-bytes").unwrap();
        let req = Request {
            from: Format::Docx,
            to: Format::Pdf,
            inputs: vec![input],
            output: dir.path().join("out.pdf"),
            overwrite: false,
        };

        run(&req, &r, &mut |_| {}).unwrap();

        let recorded = std::fs::read_to_string(&record).unwrap();
        let first_token = recorded.lines().next().unwrap();
        assert!(
            first_token.starts_with("-env:UserInstallation=file://"),
            "{first_token:?}"
        );
        assert_ne!(
            first_token,
            crate::plan::USER_INSTALLATION_PLACEHOLDER,
            "the real process must never see the dry-run placeholder text"
        );
    }

    // --- Task 2: Arg::BackendPath substitution for the pandoc+typst
    // docx/odt -> pdf fallback -----------------------------------------------

    /// Writes a script standing in for pandoc's role in the fallback
    /// recipe: records every argv token it received (i.e. *after*
    /// `run()`'s `Arg::BackendPath` substitution) to `record_path`, one per
    /// line and in order, then writes its output at whatever path follows
    /// `-o` -- mirroring `outdir_stub_that_records_argv_and_writes_pdf`'s
    /// role for the unrelated soffice/`-env:UserInstallation` mechanism.
    /// No-ops on a bare version probe, same reasoning as every other stub
    /// here.
    fn pandoc_stub_that_records_argv_and_writes_output(dir: &Path, record_path: &Path) -> PathBuf {
        let record = record_path.display();
        let (name, body) = if cfg!(windows) {
            (
                "pandoc_record_stub.bat",
                format!(
                    "@echo off\r\n\
                     if not \"%~2\"==\"\" goto notversion\r\n\
                     if \"%~1\"==\"--version\" exit /b 0\r\n\
                     if \"%~1\"==\"-version\" exit /b 0\r\n\
                     :notversion\r\n\
                     set \"outfile=\"\r\n\
                     type nul > \"{record}\"\r\n\
                     :loop\r\n\
                     if \"%~1\"==\"\" goto done\r\n\
                     echo %~1>>\"{record}\"\r\n\
                     if \"%~1\"==\"-o\" goto capture_out\r\n\
                     shift\r\n\
                     goto loop\r\n\
                     :capture_out\r\n\
                     shift\r\n\
                     echo %~1>>\"{record}\"\r\n\
                     set \"outfile=%~1\"\r\n\
                     shift\r\n\
                     goto loop\r\n\
                     :done\r\n\
                     <nul set /p \"=x\" >\"%outfile%\"\r\n\
                     exit /b 0\r\n"
                ),
            )
        } else {
            (
                "pandoc_record_stub.sh",
                format!(
                    "#!/bin/sh\n\
                     if [ \"$#\" = \"1\" ] && {{ [ \"$1\" = \"--version\" ] || [ \"$1\" = \"-version\" ]; }}; then\n\
                     \x20   exit 0\n\
                     fi\n\
                     outfile=\"\"\n\
                     prev=\"\"\n\
                     : > \"{record}\"\n\
                     for a in \"$@\"; do\n\
                     \x20   echo \"$a\" >> \"{record}\"\n\
                     \x20   if [ \"$prev\" = \"-o\" ]; then outfile=\"$a\"; fi\n\
                     \x20   prev=\"$a\"\n\
                     done\n\
                     printf x > \"$outfile\"\n"
                ),
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

    /// When soffice is unavailable but pandoc and typst both resolve,
    /// `run()` must select the pandoc+typst fallback recipe and substitute
    /// the real, resolved typst path for `Arg::BackendPath`'s placeholder
    /// -- the process actually invoked must never see the literal
    /// placeholder text `plan::build` prints for `--dry-run`, the same
    /// proof `the_real_soffice_invocation_substitutes_the_real_url_for_
    /// the_dry_run_placeholder` already requires for the unrelated
    /// `-env:UserInstallation` mechanism.
    ///
    /// Soffice is deliberately left un-overridden here, relying on the real
    /// host genuinely having no resolvable soffice -- true of this
    /// project's own bare `cargo test --workspace` CI job (see
    /// `.github/workflows/ci.yml`: "Runs on a bare runner with NO
    /// conversion backends installed") and of every machine this task was
    /// developed and verified against. `with_managed_dir` isolates the one
    /// candidate that could otherwise leak a real installed pandoc/typst in
    /// from this machine's own managed install directory.
    #[test]
    fn fallback_recipe_substitutes_the_real_typst_path_and_never_touches_soffice() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("pandoc-argv-record.txt");
        let pandoc_stub = pandoc_stub_that_records_argv_and_writes_output(dir.path(), &record);
        // Never actually invoked with real arguments in this test (only
        // ever resolved and its path substituted in), so the same
        // do-nothing-and-exit-0 stub used elsewhere stands in for it —
        // a real, spawnable script rather than an inert marker file, which
        // matters because `Resolver::resolve`'s version probe does try to
        // run it.
        let typst_stub = stub_that_writes_nothing(dir.path());

        let mut r = Resolver::new();
        r.with_managed_dir(tempfile::tempdir().unwrap().path().to_path_buf());
        r.with_override(Backend::Pandoc, pandoc_stub);
        r.with_override(Backend::Typst, typst_stub.clone());

        let input = dir.path().join("report.docx");
        std::fs::write(&input, b"docx-bytes").unwrap();
        let req = Request {
            from: Format::Docx,
            to: Format::Pdf,
            inputs: vec![input],
            output: dir.path().join("out.pdf"),
            overwrite: false,
        };

        let outcome = run(&req, &r, &mut |_| {}).unwrap();
        assert_eq!(
            outcome.backends[0].0,
            Backend::Pandoc,
            "must have selected the pandoc+typst fallback, not soffice: {:?}",
            outcome.backends
        );

        let recorded: Vec<String> = std::fs::read_to_string(&record)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        let engine_idx = recorded
            .iter()
            .position(|t| t == "--pdf-engine")
            .unwrap_or_else(|| panic!("argv must carry --pdf-engine: {recorded:?}"));
        let engine_value = &recorded[engine_idx + 1];
        assert_eq!(
            PathBuf::from(engine_value),
            typst_stub,
            "the real resolved typst path must be substituted, not left as a placeholder"
        );
        assert_ne!(
            *engine_value,
            Backend::Typst.path_placeholder(),
            "the real process must never see the dry-run placeholder text"
        );
    }

    /// When soffice is absent and only one of pandoc/typst is available
    /// (here: pandoc, not typst), `plan::select`'s safety net must *not*
    /// choose the fallback recipe -- it requires *both* pandoc and typst --
    /// so `run()` falls through to the canonical soffice recipe and
    /// surfaces the ordinary `backend_missing` naming soffice, not a
    /// confusing one naming typst (a backend the recipe it actually chose
    /// never even mentions). This is the end-to-end proof of the same rule
    /// `plan::tests::selection_falls_back_to_soffice_when_neither_route_is_
    /// fully_available` checks at the pure planning layer. `with_managed_dir`
    /// isolates the Managed candidate so a real pandoc/typst installed on
    /// this machine (e.g. via `conv install`) can't leak in and change which
    /// backends this test's `Resolver` sees as available.
    #[test]
    fn only_pandoc_available_still_reports_backend_missing_naming_soffice() {
        let dir = tempfile::tempdir().unwrap();
        let pandoc_stub = stub_that_creates_its_output(dir.path());
        let mut r = Resolver::new();
        r.with_managed_dir(tempfile::tempdir().unwrap().path().to_path_buf());
        r.with_override(Backend::Pandoc, pandoc_stub);
        r.with_override(Backend::Typst, PathBuf::from("/definitely/not/here"));
        r.with_override(
            Backend::Soffice,
            PathBuf::from("/definitely/not/here/either"),
        );

        let input = dir.path().join("report.docx");
        std::fs::write(&input, b"docx-bytes").unwrap();
        let req = Request {
            from: Format::Docx,
            to: Format::Pdf,
            inputs: vec![input],
            output: dir.path().join("out.pdf"),
            overwrite: false,
        };

        let e = run(&req, &r, &mut |_| {}).unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::BackendMissing);
        assert_eq!(
            e.backend,
            Some(Backend::Soffice),
            "with typst unavailable, the fallback must never be chosen, so the \
             error must name soffice -- the canonical recipe's own backend --  \
             not typst"
        );
    }

    /// The general `Arg::BackendPath` substitution mechanism itself: when a
    /// step's argv names a backend that cannot be resolved, `run()` must
    /// surface the ordinary `backend_missing` error naming that backend.
    /// Exercised directly against `substitute_backend_paths` rather than
    /// through the full `docx -> pdf` selection pipeline, because that
    /// pipeline's own safety net (see the test above) means a real
    /// `docx -> pdf` conversion can never reach this state: if `typst` were
    /// truly unresolvable, `plan::select` would never have chosen the
    /// pandoc+typst recipe that needs it in the first place.
    #[test]
    fn substitute_backend_paths_reports_backend_missing_naming_the_absent_backend() {
        let argv = vec![
            "in.docx".to_string(),
            "--pdf-engine".to_string(),
            Backend::Typst.path_placeholder(),
            "-o".to_string(),
            "out.pdf".to_string(),
        ];
        let mut r = Resolver::new();
        r.with_managed_dir(tempfile::tempdir().unwrap().path().to_path_buf());
        r.with_override(Backend::Typst, PathBuf::from("/definitely/not/here"));

        let e = substitute_backend_paths(&argv, &r).unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::BackendMissing);
        assert_eq!(e.backend, Some(Backend::Typst));
    }
}
