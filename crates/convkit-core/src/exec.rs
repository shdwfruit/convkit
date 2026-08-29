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

    // Probe only when a stream copy is even possible for this pair.
    let probed = if registry::needs_probe(req.from, req.to) {
        resolver
            .resolve(Backend::Ffprobe)
            .ok()
            .and_then(|p| probe::run(&p.path, &req.inputs[0]).ok())
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

    let built = plan::build(req.from, req.to, &req.inputs, &temp_final, probed.as_ref())?;
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
            cmd.arg(format!("-env:UserInstallation={url}"));
        }
        cmd.args(&step.argv);

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
            let tail: String = stderr.lines().rev().take(3).collect::<Vec<_>>().join("; ");
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
    })
}

fn is_non_empty(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false)
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
/// and an unescaped space (this machine's own home directory is
/// `C:\Users\Rick Xie` — a raw space in the URL is equally malformed and
/// equally silently ignored, losing exactly the profile isolation this flag
/// exists to provide). Both are Windows-only failures Linux CI never
/// catches, on the one backend that can never be auto-installed.
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
        };

        let outcome = run(&req, &r, &mut |_| {}).unwrap();
        assert!(output.is_file(), "output must exist after rename");
        assert_eq!(outcome.bytes, 1);
        assert_eq!(outcome.output, output);
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

    /// IMPORTANT 2: a raw space is just as malformed as a backslash, and
    /// this machine's own home directory (`C:\Users\Rick Xie`) has one — so
    /// this uses that exact name rather than a synthetic example.
    #[cfg(windows)]
    #[test]
    fn user_installation_url_percent_encodes_a_space_in_the_path() {
        let profile = PathBuf::from(r"C:\Users\Rick Xie\AppData\Local\Temp\convkit-lo-profile-0");
        let url = user_installation_url(&profile).unwrap();
        assert_eq!(
            url,
            "file:///C:/Users/Rick%20Xie/AppData/Local/Temp/convkit-lo-profile-0"
        );
        assert!(!url.contains(' '), "{url}");
    }

    #[cfg(not(windows))]
    #[test]
    fn user_installation_url_percent_encodes_a_space_in_the_path() {
        let profile = PathBuf::from("/tmp/rick xie/convkit-lo-profile-0");
        let url = user_installation_url(&profile).unwrap();
        assert_eq!(url, "file:///tmp/rick%20xie/convkit-lo-profile-0");
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
}
