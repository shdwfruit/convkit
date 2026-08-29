use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{ConvError, Result};
use crate::Backend;

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

    pub fn resolve(&self, backend: Backend) -> Result<ResolvedBackend> {
        for (path, source) in self.candidates(backend) {
            if !path.is_file() {
                continue;
            }
            let version = Self::version_of(backend, &path).unwrap_or_else(|| "unknown".into());
            return Ok(ResolvedBackend {
                backend,
                path,
                version,
                source,
            });
        }
        Err(ConvError::backend_missing(backend))
    }

    /// First line of `<exe> --version`, trimmed. Never fails the resolve —
    /// an unreadable version is reported as "unknown".
    fn version_of(backend: Backend, path: &Path) -> Option<String> {
        let flag = match backend {
            Backend::Magick => "-version",
            _ => "--version",
        };
        let out = Command::new(path).arg(flag).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines().next().map(|l| l.trim().to_string())
    }
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
}
