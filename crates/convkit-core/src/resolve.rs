use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::error::{ConvError, Result};
use crate::Backend;

/// Uniquifies the soffice version-probe's isolated profile directory across
/// however many probes happen to run inside one process (in practice at
/// most one per distinct `Resolver`, thanks to the cache below, but this
/// costs nothing and removes any doubt).
static VERSION_PROBE_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Override,
    Env,
    Managed,
    Path,
    WellKnown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBackend {
    pub backend: Backend,
    pub path: PathBuf,
    pub version: String,
    pub source: Source,
}

#[derive(Debug, Default)]
pub struct Resolver {
    overrides: HashMap<Backend, PathBuf>,
    /// Successful resolutions, keyed by backend. `Mutex` rather than
    /// `RefCell` because a `Resolver` is expected to be shared across
    /// threads in Task 12's rayon batch mode — plain interior mutability
    /// would make `Resolver` `!Sync` and fail to compile there.
    cache: Mutex<HashMap<Backend, ResolvedBackend>>,
}

impl Resolver {
    pub fn new() -> Self {
        Resolver::default()
    }

    pub fn with_override(&mut self, backend: Backend, path: PathBuf) {
        self.overrides.insert(backend, path);
    }

    /// Where `conv install` places managed binaries.
    pub fn managed_dir() -> PathBuf {
        #[cfg(windows)]
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        #[cfg(not(windows))]
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));

        base.unwrap_or_else(std::env::temp_dir)
            .join("convkit")
            .join("bin")
    }

    /// Environment variable consulted for this backend, e.g. `CONVKIT_FFMPEG`.
    fn env_var(backend: Backend) -> String {
        format!("CONVKIT_{}", backend.exe_name().to_ascii_uppercase())
    }

    /// Platform install locations checked last, for tools that commonly are not
    /// on PATH. LibreOffice on Windows and macOS is the main reason this exists.
    fn well_known(backend: Backend) -> Vec<PathBuf> {
        match (backend, cfg!(windows), cfg!(target_os = "macos")) {
            (Backend::Soffice, true, _) => vec![
                PathBuf::from(r"C:\Program Files\LibreOffice\program\soffice.exe"),
                PathBuf::from(r"C:\Program Files (x86)\LibreOffice\program\soffice.exe"),
            ],
            (Backend::Soffice, _, true) => {
                vec![PathBuf::from(
                    "/Applications/LibreOffice.app/Contents/MacOS/soffice",
                )]
            }
            _ => Vec::new(),
        }
    }

    /// Ordered candidates. Exposed for testing the documented precedence.
    pub fn candidates(&self, backend: Backend) -> Vec<(PathBuf, Source)> {
        let exe = backend.exe_name();
        let mut out = Vec::new();

        if let Some(p) = self.overrides.get(&backend) {
            out.push((p.clone(), Source::Override));
        }
        if let Some(p) = std::env::var_os(Self::env_var(backend)) {
            out.push((PathBuf::from(p), Source::Env));
        }
        let managed = Self::managed_dir().join(if cfg!(windows) {
            format!("{exe}.exe")
        } else {
            exe.to_string()
        });
        out.push((managed, Source::Managed));
        if let Ok(p) = which::which(exe) {
            out.push((p, Source::Path));
        }
        out.extend(
            Self::well_known(backend)
                .into_iter()
                .map(|p| (p, Source::WellKnown)),
        );
        out
    }

    /// Resolves a backend, caching the result so each backend is probed —
    /// meaning: its candidates walked and, on a hit, its version probe spawned
    /// — at most once per `Resolver` (in practice, once per process, since
    /// one `Resolver` is expected to live for a whole `conv` invocation).
    /// Only successes are cached; a missing backend is cheap to re-check
    /// (no subprocess involved) and re-checking costs nothing.
    pub fn resolve(&self, backend: Backend) -> Result<ResolvedBackend> {
        if let Some(cached) = self.cache.lock().unwrap().get(&backend) {
            return Ok(cached.clone());
        }
        for (path, source) in self.candidates(backend) {
            if !path.is_file() {
                continue;
            }
            let version = Self::version_of(backend, &path).unwrap_or_else(|| "unknown".into());
            let resolved = ResolvedBackend {
                backend,
                path,
                version,
                source,
            };
            self.cache.lock().unwrap().insert(backend, resolved.clone());
            return Ok(resolved);
        }
        Err(ConvError::backend_missing(backend))
    }

    /// A short version token extracted from the first line of the
    /// backend's version banner (see `extract_version_token`), not the
    /// whole line. Never fails the resolve — an unreadable version is
    /// reported as "unknown".
    ///
    /// Two things a naive `--version` probe gets wrong, discovered by
    /// `conv doctor` reporting a real, working ffmpeg as "unknown":
    /// `--version` is not a real ffmpeg (or ffprobe, or ImageMagick
    /// `magick`) option at all — they take the single-dash `-version` —
    /// and ffmpeg's version banner is written to stderr regardless of which
    /// flag is used, leaving stdout empty. pandoc and soffice use the
    /// conventional double-dash `--version` and do print it to stdout, so
    /// this only falls back to stderr when stdout came back empty rather
    /// than always preferring one stream.
    ///
    /// For `soffice` specifically, this is itself a soffice invocation, so
    /// it gets its own isolated `-env:UserInstallation` profile just like a
    /// real conversion does — the constraint that every soffice invocation
    /// gets its own profile has no carve-out for version probes, and without
    /// this a probe could collide with an already-running LibreOffice. The
    /// probe's profile directory is removed afterward on a best-effort
    /// basis; nothing downstream depends on it surviving.
    fn version_of(backend: Backend, path: &Path) -> Option<String> {
        let flag = match backend {
            Backend::Ffmpeg | Backend::Ffprobe | Backend::Magick => "-version",
            Backend::Pandoc | Backend::Soffice => "--version",
        };
        let mut cmd = Command::new(path);
        cmd.arg(flag);

        let profile = (backend == Backend::Soffice).then(|| {
            std::env::temp_dir().join(format!(
                "convkit-lo-version-probe-{}-{}",
                std::process::id(),
                VERSION_PROBE_COUNTER.fetch_add(1, Ordering::Relaxed)
            ))
        });
        if let Some(profile) = &profile {
            if let Ok(url) = crate::exec::user_installation_url(profile) {
                cmd.arg(format!("-env:UserInstallation={url}"));
            }
        }

        let out = cmd.output().ok()?;
        if let Some(profile) = &profile {
            let _ = std::fs::remove_dir_all(profile);
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let text = if stdout.trim().is_empty() {
            String::from_utf8_lossy(&out.stderr).into_owned()
        } else {
            stdout.into_owned()
        };
        let first_line = text.lines().next()?;
        extract_version_token(first_line).map(str::to_string)
    }
}

/// Picks the first whitespace-separated token on `line` that contains a
/// digit, rather than returning the whole banner line. A naive whole-line
/// return blew out `doctor`'s tabular version column — ffmpeg's own first
/// line alone runs to roughly 90 characters — so this extracts a short,
/// version-ish token instead. Works across every backend's banner shape
/// without per-backend parsing, since each one puts a short version-like
/// token near the front of its first line:
///   "ffmpeg version 9.0-full_build-www.gyan.dev Copyright (c) ..." -> "9.0-full_build-www.gyan.dev"
///   "Version: ImageMagick 7.1.1-29 Q16-HDRI x64 ..."                -> "7.1.1-29"
///   "pandoc 3.1.11"                                                 -> "3.1.11"
///   "LibreOffice 7.6.4.1"                                           -> "7.6.4.1"
/// Returns `None` when no token on the line contains a digit at all, so
/// the caller falls back to "unknown" rather than printing garbage.
fn extract_version_token(line: &str) -> Option<&str> {
    line.split_whitespace()
        .find(|tok| tok.chars().any(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn an_override_wins_over_everything_else() {
        let mut r = Resolver::new();
        let fake = PathBuf::from("/nowhere/custom-ffmpeg");
        r.with_override(Backend::Ffmpeg, fake.clone());
        let got = r.candidates(Backend::Ffmpeg);
        assert_eq!(got[0].0, fake);
        assert_eq!(got[0].1, Source::Override);
    }

    #[test]
    fn candidate_order_matches_the_spec() {
        let r = Resolver::new();
        let sources: Vec<Source> = r
            .candidates(Backend::Ffmpeg)
            .into_iter()
            .map(|c| c.1)
            .collect();
        let expected = [
            Source::Env,
            Source::Managed,
            Source::Path,
            Source::WellKnown,
        ];
        let filtered: Vec<Source> = sources
            .into_iter()
            .filter(|s| expected.contains(s))
            .collect();
        let mut seen_order: Vec<Source> = Vec::new();
        for s in filtered {
            if seen_order.last() != Some(&s) {
                seen_order.push(s);
            }
        }
        for w in seen_order.windows(2) {
            let a = expected.iter().position(|e| *e == w[0]).unwrap();
            let b = expected.iter().position(|e| *e == w[1]).unwrap();
            assert!(a < b, "candidate sources out of order: {seen_order:?}");
        }
    }

    #[test]
    fn a_missing_backend_produces_a_remediable_error() {
        let mut r = Resolver::new();
        r.with_override(Backend::Pandoc, PathBuf::from("/definitely/not/here"));
        let e = r.resolve(Backend::Pandoc).unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::BackendMissing);
        let rem = e.remediation.expect("must carry remediation");
        assert_eq!(rem.managed.as_deref(), Some("conv install pandoc"));
    }

    #[test]
    fn libreoffice_is_never_offered_as_a_managed_install() {
        let mut r = Resolver::new();
        r.with_override(Backend::Soffice, PathBuf::from("/definitely/not/here"));
        let e = r.resolve(Backend::Soffice).unwrap_err();
        assert_eq!(e.remediation.unwrap().managed, None);
    }

    // --- Controller review round 3: version_of used the wrong flag and
    // ignored stderr, so a real ffmpeg was reported as "unknown" -------------

    /// Writes a script that echoes its *first* argument, with a `-9`
    /// suffix, to stdout and exits 0, so a test can observe exactly which
    /// version flag `version_of` passed. Must be the first argument, not
    /// the last: for `soffice`, `version_of` appends
    /// `-env:UserInstallation=...` *after* the version flag, so the flag is
    /// not reliably the last argument, only the first. The `-9` suffix
    /// matters because `version_of` now extracts a digit-bearing token from
    /// the banner line rather than returning it whole (see
    /// `extract_version_token`); a bare `-version`/`--version` has no digit
    /// in it, so without the suffix this stub would make every case report
    /// "unknown" regardless of which flag was passed.
    fn stub_that_echoes_first_arg(dir: &Path) -> PathBuf {
        let (name, body) = if cfg!(windows) {
            ("echo_first.bat", "@echo off\r\necho %~1-9\r\nexit /b 0\r\n")
        } else {
            ("echo_first.sh", "#!/bin/sh\necho \"$1-9\"\n")
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

    /// Writes a script that prints fixed text to *stderr only* and exits 0 —
    /// mirrors a real ffmpeg build, whose `--version`/`-version` banner never
    /// touches stdout at all.
    fn stub_that_prints_version_to_stderr_only(dir: &Path) -> PathBuf {
        let (name, body) = if cfg!(windows) {
            (
                "stderr_version.bat",
                "@echo off\r\n\
                 echo banner-9.0 1>&2\r\n\
                 exit /b 0\r\n",
            )
        } else {
            (
                "stderr_version.sh",
                "#!/bin/sh\n\
                 echo \"banner-9.0\" >&2\n",
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

    /// `--version` isn't a real ffmpeg (or ffprobe, or ImageMagick `magick`)
    /// option; all three take the single-dash `-version`.
    #[test]
    fn version_probe_uses_single_dash_for_ffmpeg_ffprobe_and_magick() {
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_that_echoes_first_arg(dir.path());
        for backend in [Backend::Ffmpeg, Backend::Ffprobe, Backend::Magick] {
            let mut r = Resolver::new();
            r.with_override(backend, stub.clone());
            let resolved = r.resolve(backend).unwrap();
            assert_eq!(
                resolved.version, "-version-9",
                "{backend:?} must be probed with -version"
            );
        }
    }

    /// pandoc and soffice use the conventional double-dash `--version`.
    #[test]
    fn version_probe_uses_double_dash_for_pandoc_and_soffice() {
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_that_echoes_first_arg(dir.path());
        for backend in [Backend::Pandoc, Backend::Soffice] {
            let mut r = Resolver::new();
            r.with_override(backend, stub.clone());
            let resolved = r.resolve(backend).unwrap();
            assert_eq!(
                resolved.version, "--version-9",
                "{backend:?} must be probed with --version"
            );
        }
    }

    /// A real ffmpeg build writes its `-version` banner to stderr, leaving
    /// stdout empty; `version_of` must fall back to stderr rather than
    /// reporting "unknown" for a backend that is actually installed.
    #[test]
    fn version_falls_back_to_stderr_when_stdout_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_that_prints_version_to_stderr_only(dir.path());
        let mut r = Resolver::new();
        r.with_override(Backend::Ffmpeg, stub);
        let resolved = r.resolve(Backend::Ffmpeg).unwrap();
        assert_eq!(resolved.version, "banner-9.0");
    }

    // --- Controller review round 5: extract a version token, not the
    // whole banner line, so doctor's tabular column can't be blown out ----

    #[test]
    fn extracts_the_version_token_from_a_real_ffmpeg_banner() {
        let line = "ffmpeg version 9.0-full_build-www.gyan.dev Copyright (c) 2000-2026 the FFmpeg developers";
        assert_eq!(
            extract_version_token(line),
            Some("9.0-full_build-www.gyan.dev")
        );
    }

    #[test]
    fn extracts_the_version_token_from_a_real_imagemagick_banner() {
        let line = "Version: ImageMagick 7.1.1-29 Q16-HDRI x64 20231001 https://imagemagick.org";
        assert_eq!(extract_version_token(line), Some("7.1.1-29"));
    }

    #[test]
    fn extracts_the_version_token_from_a_real_pandoc_banner() {
        assert_eq!(extract_version_token("pandoc 3.1.11"), Some("3.1.11"));
    }

    #[test]
    fn extracts_the_version_token_from_a_real_soffice_banner() {
        assert_eq!(
            extract_version_token("LibreOffice 7.6.4.1"),
            Some("7.6.4.1")
        );
    }

    /// No token on the line contains a digit at all: `version_of` must fall
    /// back to "unknown" rather than picking some arbitrary non-version
    /// word or panicking.
    #[test]
    fn no_digit_bearing_token_yields_none() {
        assert_eq!(
            extract_version_token("no digits anywhere on this line"),
            None
        );
    }
}
