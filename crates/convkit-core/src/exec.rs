use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

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

#[derive(Debug, Clone)]
pub struct Outcome {
    pub output: PathBuf,
    pub bytes: u64,
    pub warnings: Vec<String>,
    pub backends: Vec<(Backend, String)>,
    pub remuxed: bool,
}

/// Runs a conversion plan end to end: resolves each step's backend, spawns
/// it, verifies it actually produced output, and atomically renames the
/// result into place.
///
/// # Design note
///
/// `--dry-run` (Task 10) calls `plan::build` directly with the user's real
/// output path. This function instead builds a plan targeting a temp path
/// in the destination directory and renames on success — the two therefore
/// render the same flags with different output paths, which is intended:
/// the rename is what makes Ctrl-C safe. A missing or zero-byte result is
/// always a failure regardless of exit code, since `soffice` returns 0 on
/// failure.
pub fn run(req: &Request, resolver: &Resolver, on_event: &mut dyn FnMut(Event)) -> Result<Outcome> {
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

    // Execute against a temp path in the destination directory, so a partial
    // result is never visible under the real name.
    let temp_final = temp_sibling(&req.output);
    let built = plan::build(req.from, req.to, &req.inputs, &temp_final, probed.as_ref())?;
    let remuxed = built.steps[0].argv.windows(2).any(|w| w == ["-c", "copy"]);

    let total = built.steps.len();
    let mut backends = Vec::new();
    let mut intermediates: Vec<PathBuf> = Vec::new();
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

        // Constraint: every soffice invocation gets its own isolated profile
        // so concurrent runs never collide.
        let mut soffice_profile: Option<PathBuf> = None;
        if step.backend == Backend::Soffice {
            let profile = temp_final.with_extension(format!("convkit-lo-profile-{i}"));
            let url = user_installation_url(&profile)?;
            cmd.arg(format!("-env:UserInstallation={url}"));
            soffice_profile = Some(profile);
        }
        cmd.args(&step.argv);

        let out = cmd.output().map_err(|e| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("failed to run {}: {e}", resolved.path.display()),
            )
        })?;

        // Push the profile onto `intermediates` unconditionally (success or
        // failure) so `cleanup` always removes it.
        if let Some(profile) = soffice_profile {
            intermediates.push(profile);
        }

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
            cleanup(&intermediates);
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
        if i + 1 < total {
            intermediates.push(declared.clone());
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
    cleanup(&intermediates);

    Ok(Outcome {
        output: req.output.clone(),
        bytes,
        warnings: built.warnings,
        backends,
        remuxed,
    })
}

fn temp_sibling(output: &Path) -> PathBuf {
    let pid = std::process::id();
    output.with_extension(format!(
        "convkit-{pid}.{}",
        output.extension().and_then(|e| e.to_str()).unwrap_or("tmp")
    ))
}

fn is_non_empty(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false)
}

/// In OutDir mode the backend names the file `<input-stem>.<ext>` itself.
/// Match on the input's stem plus the wanted extension — not just "the
/// newest file with this extension" — so a directory that already holds an
/// unrelated file of the same type is never mistaken for our result. Mtime
/// only breaks a genuine tie between multiple matches.
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

/// Removes every path exec created outside the final output: intermediate
/// step files and (recursively — it is a directory LibreOffice populates,
/// not a file) each soffice profile directory.
fn cleanup(paths: &[PathBuf]) {
    for p in paths {
        let _ = if p.is_dir() {
            std::fs::remove_dir_all(p)
        } else {
            std::fs::remove_file(p)
        };
    }
}

fn io_err(e: std::io::Error) -> ConvError {
    ConvError::new(ErrorCode::ConversionFailed, e.to_string())
}

/// Builds a well-formed `file://` URL for `-env:UserInstallation`. The
/// profile directory need not exist yet — LibreOffice creates it — so this
/// uses `std::path::absolute`, which never touches the filesystem.
///
/// A naive `format!("file://{}", path.display())` on Windows renders as
/// `file://C:\Users\...`: wrong slash count and backslash separators, which
/// LibreOffice rejects or silently ignores, losing the profile isolation
/// this flag exists to provide. Linux CI never catches this — it is a
/// Windows-only failure on the one backend that can never be auto-installed.
fn user_installation_url(profile: &Path) -> Result<String> {
    let abs = std::path::absolute(profile).map_err(io_err)?;
    #[cfg(windows)]
    {
        Ok(format!(
            "file:///{}",
            abs.to_string_lossy().replace('\\', "/")
        ))
    }
    #[cfg(not(windows))]
    {
        Ok(format!("file://{}", abs.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Writes a tiny script that copies argv's last element into existence.
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
                "@echo off\r\n:loop\r\nif \"%~1\"==\"\" goto done\r\nset \"last=%~1\"\r\nshift\r\ngoto loop\r\n:done\r\n<nul set /p \"=x\" >\"%last%\"\r\nexit /b 0\r\n",
            )
        } else {
            (
                "stub.sh",
                "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\nprintf x > \"$last\"\n",
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

    #[test]
    fn a_backend_that_writes_nothing_is_a_failure_even_on_exit_zero() {
        let dir = tempfile::tempdir().unwrap();
        // The brief's version called `std::os::unix::fs::PermissionsExt`
        // inside the runtime `else` branch of `if cfg!(windows)`, so both
        // branches were compiled on every platform and this failed to build
        // on Windows. Fixed the same way `stub_that_creates_its_output`
        // already does it below: pick the path/content at runtime, apply
        // the unix-only permission bit behind a `#[cfg(unix)]` block.
        let noop = if cfg!(windows) {
            let p = dir.path().join("noop.bat");
            std::fs::write(&p, "@echo off\r\n").unwrap();
            p
        } else {
            let p = dir.path().join("noop.sh");
            std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
            p
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&noop, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
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

    // --- Controller amendments beyond the brief -----------------------------

    /// Amendment 4: `-env:UserInstallation` must be a well-formed `file://`
    /// URL. A naive `format!("file://{}", path.display())` on Windows
    /// produces `file://C:\Users\...` — wrong slash count, backslash
    /// separators — which LibreOffice rejects or silently ignores. Uses an
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

    /// Amendment 6: the LibreOffice profile directory is a directory
    /// LibreOffice populates with its own files, not a single file. Confirm
    /// `cleanup` actually removes it recursively rather than failing (or
    /// silently no-op'ing) on a non-empty directory.
    #[test]
    fn cleanup_removes_a_populated_directory_recursively() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("convkit-lo-profile-0");
        std::fs::create_dir_all(profile.join("user/config")).unwrap();
        std::fs::write(profile.join("user/config/registrymodifications.xcu"), b"x").unwrap();
        assert!(profile.is_dir());

        cleanup(std::slice::from_ref(&profile));

        assert!(
            !profile.exists(),
            "profile directory must be removed recursively"
        );
    }
}
