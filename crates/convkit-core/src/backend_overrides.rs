//! Turns the small set of optional `--<backend>-path` overrides every
//! convkit binary exposes into a configured [`Resolver`], in one place.
//!
//! This used to be two independent copies -- `conv`'s own `Cli::resolver()`
//! and `convkit-diff`'s `BackendPaths::resolver()` -- linked only by a code
//! comment noting that the second one echoed the first, so nothing actually
//! detected drift between them. It didn't stay in sync: `conv`'s copy of
//! the ffprobe-sibling inference below broke on Linux and macOS (a test
//! built its input from a Windows-only path literal) and CI caught it,
//! while `convkit-diff`'s copy had zero test coverage and was never
//! checked. `BackendOverrides` is now the only implementation; both crates
//! build one from their own CLI flags and call [`BackendOverrides::resolver`].
//!
//! This is resolution *policy* -- which override maps to which [`Backend`],
//! and the one piece of inference among them (a bare `--ffmpeg-path`
//! implies a sibling `ffprobe` in the same directory) -- so it lives here
//! next to [`Resolver`] itself, not in either binary crate.

use std::path::PathBuf;

use crate::{Backend, Resolver};

/// The optional `--<backend>-path` overrides a caller's CLI exposes,
/// carried as plain data so any crate's own flag struct can build one from
/// its own fields without needing to share that struct's shape. A caller
/// that doesn't expose every flag (`convkit-diff` has no
/// `--pandoc-path`/`--soffice-path`/`--typst-path` at all) just leaves the
/// rest `None` via `Default`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackendOverrides {
    pub ffmpeg: Option<PathBuf>,
    /// Takes precedence over inferring a sibling from `ffmpeg`, when both
    /// are set -- see `resolver`'s docs.
    pub ffprobe: Option<PathBuf>,
    pub magick: Option<PathBuf>,
    pub pandoc: Option<PathBuf>,
    pub soffice: Option<PathBuf>,
    pub typst: Option<PathBuf>,
}

impl BackendOverrides {
    /// Builds a [`Resolver`] with `with_override` applied for every
    /// override set here, plus one inference: when `ffmpeg` is set but
    /// `ffprobe` is not, the sibling `ffprobe`/`ffprobe.exe` in `ffmpeg`'s
    /// own directory is used as the `Ffprobe` override too, since ffprobe
    /// ships beside ffmpeg in every distribution convkit supports. An
    /// explicit `ffprobe` always wins over that inference -- it is checked
    /// first, and the inference branch only runs in its absence -- so
    /// pinning `--ffprobe-path` at a deliberately-missing file (to force
    /// the no-probe transcode path, for instance) is never silently
    /// overridden just because `--ffmpeg-path` also happens to be set.
    ///
    /// Every override recorded here is later *authoritative*, not a hint:
    /// `Resolver::resolve` treats a `Source::Override` candidate that isn't
    /// a real file as a hard, immediate error rather than falling through
    /// to `PATH`/well-known locations (see `Resolver::resolve`'s docs).
    /// This method itself does no filesystem I/O and never fails -- it
    /// only records paths -- so that error only surfaces once something
    /// actually resolves a backend through the `Resolver` this returns.
    ///
    /// Sibling inference walks whatever `Path::parent()` returns, so it
    /// works the same way on every platform: it isn't a Windows-only
    /// concept, only the resulting filename (`ffprobe.exe` vs. `ffprobe`)
    /// is platform-specific.
    pub fn resolver(&self) -> Resolver {
        let mut r = Resolver::new();
        for (path, backend) in [
            (&self.ffmpeg, Backend::Ffmpeg),
            (&self.magick, Backend::Magick),
            (&self.pandoc, Backend::Pandoc),
            (&self.soffice, Backend::Soffice),
            (&self.typst, Backend::Typst),
        ] {
            if let Some(p) = path {
                r.with_override(backend, p.clone());
            }
        }
        if let Some(p) = &self.ffprobe {
            r.with_override(Backend::Ffprobe, p.clone());
        } else if let Some(p) = &self.ffmpeg {
            if let Some(dir) = p.parent() {
                let filename = if cfg!(windows) {
                    format!("{}.exe", Backend::Ffprobe.exe_name())
                } else {
                    Backend::Ffprobe.exe_name().to_string()
                };
                r.with_override(Backend::Ffprobe, dir.join(filename));
            }
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Source;

    /// A path builder that stays meaningful on every platform: a
    /// Windows-style literal has no parent at all on Unix (backslash is
    /// just an ordinary filename character there), so a test that always
    /// used one would pass vacuously on Linux/macOS without ever exercising
    /// `Path::parent()`-based inference. This is exactly the class of bug
    /// commit fe460a2 fixed in the old `conv`-only copy of these tests;
    /// building every cross-platform test input this way is what keeps it
    /// fixed here.
    fn ffmpeg_bin_dir_path(filename: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\tools\ffmpeg\bin\{filename}"))
        } else {
            PathBuf::from(format!("/tools/ffmpeg/bin/{filename}"))
        }
    }

    /// With no overrides at all, every backend's candidates are exactly
    /// what a bare `Resolver::new()` produces -- `BackendOverrides::default()`
    /// must add nothing on its own.
    #[test]
    fn no_overrides_behaves_exactly_like_a_bare_resolver() {
        let built = BackendOverrides::default().resolver();
        let bare = Resolver::new();
        for backend in [
            Backend::Ffmpeg,
            Backend::Ffprobe,
            Backend::Magick,
            Backend::Pandoc,
            Backend::Soffice,
            Backend::Typst,
        ] {
            assert_eq!(
                built.candidates(backend),
                bare.candidates(backend),
                "{backend:?}: default BackendOverrides must not add a candidate"
            );
        }
    }

    /// Each of the five directly-overridable backends maps to its own
    /// `Backend` variant, and only that one -- e.g. `magick` must never
    /// accidentally land on `Backend::Pandoc`.
    #[test]
    fn each_override_lands_on_exactly_its_own_backend() {
        for (set, backend) in [
            (
                BackendOverrides {
                    ffmpeg: Some(PathBuf::from("/o/ffmpeg")),
                    ..Default::default()
                },
                Backend::Ffmpeg,
            ),
            (
                BackendOverrides {
                    magick: Some(PathBuf::from("/o/magick")),
                    ..Default::default()
                },
                Backend::Magick,
            ),
            (
                BackendOverrides {
                    pandoc: Some(PathBuf::from("/o/pandoc")),
                    ..Default::default()
                },
                Backend::Pandoc,
            ),
            (
                BackendOverrides {
                    soffice: Some(PathBuf::from("/o/soffice")),
                    ..Default::default()
                },
                Backend::Soffice,
            ),
            (
                BackendOverrides {
                    typst: Some(PathBuf::from("/o/typst")),
                    ..Default::default()
                },
                Backend::Typst,
            ),
        ] {
            let r = set.resolver();
            let candidates = r.candidates(backend);
            assert_eq!(
                candidates.first().map(|(_, s)| *s),
                Some(Source::Override),
                "{backend:?}: expected its own override to be the top candidate: {candidates:?}"
            );
            for other in [
                Backend::Ffmpeg,
                Backend::Ffprobe,
                Backend::Magick,
                Backend::Pandoc,
                Backend::Soffice,
                Backend::Typst,
            ] {
                if other == backend {
                    continue;
                }
                // Ffmpeg is the one deliberate exception: its own override
                // also implies an inferred Ffprobe override, by design (see
                // `explicit_ffprobe_wins_over_the_ffmpeg_sibling_inference`
                // and `ffmpeg_alone_still_infers_the_sibling_ffprobe` for
                // that behaviour in full) -- not a leak this loop should
                // flag.
                if backend == Backend::Ffmpeg && other == Backend::Ffprobe {
                    continue;
                }
                let other_candidates = r.candidates(other);
                assert!(
                    !other_candidates
                        .first()
                        .is_some_and(|(_, s)| *s == Source::Override),
                    "{backend:?}'s override must not leak onto {other:?}: {other_candidates:?}"
                );
            }
        }
    }

    /// An explicit `ffprobe` must take precedence over the sibling
    /// `ffmpeg` otherwise infers from its own directory -- pinning
    /// `ffprobe` at a nonexistent file to force the no-probe transcode
    /// path must not be silently overridden by the ffmpeg-directory
    /// inference just because `ffmpeg` also happens to be set.
    #[test]
    fn explicit_ffprobe_wins_over_the_ffmpeg_sibling_inference() {
        let ffmpeg = ffmpeg_bin_dir_path(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        let ffprobe = PathBuf::from(if cfg!(windows) {
            r"C:\elsewhere\my-ffprobe.exe"
        } else {
            "/elsewhere/my-ffprobe"
        });
        let overrides = BackendOverrides {
            ffmpeg: Some(ffmpeg),
            ffprobe: Some(ffprobe.clone()),
            ..Default::default()
        };
        let r = overrides.resolver();
        let candidates = r.candidates(Backend::Ffprobe);
        assert_eq!(
            candidates.first(),
            Some(&(ffprobe, Source::Override)),
            "{candidates:?}"
        );
    }

    /// With no explicit `ffprobe`, `ffmpeg` alone must still infer the
    /// sibling in the same directory. Sibling inference is not a Windows
    /// concept: it walks whatever `Path::parent()` returns, so this must
    /// hold on every platform -- see `ffmpeg_bin_dir_path`'s docs on why
    /// the input is built platform-appropriately rather than with an
    /// unconditional Windows-style literal.
    #[test]
    fn ffmpeg_alone_still_infers_the_sibling_ffprobe() {
        let ffmpeg = ffmpeg_bin_dir_path(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        let overrides = BackendOverrides {
            ffmpeg: Some(ffmpeg),
            ..Default::default()
        };
        let r = overrides.resolver();
        let candidates = r.candidates(Backend::Ffprobe);
        let expected_probe = ffmpeg_bin_dir_path(if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        });
        assert_eq!(
            candidates.first(),
            Some(&(expected_probe, Source::Override)),
            "{candidates:?}"
        );
    }

    /// `ffprobe` alone (no `ffmpeg` at all) still overrides ffprobe, and
    /// must never accidentally also override ffmpeg itself.
    #[test]
    fn ffprobe_alone_overrides_only_ffprobe() {
        let ffprobe = PathBuf::from(if cfg!(windows) {
            r"C:\elsewhere\my-ffprobe.exe"
        } else {
            "/elsewhere/my-ffprobe"
        });
        let overrides = BackendOverrides {
            ffprobe: Some(ffprobe.clone()),
            ..Default::default()
        };
        let r = overrides.resolver();
        assert_eq!(
            r.candidates(Backend::Ffprobe).first(),
            Some(&(ffprobe, Source::Override))
        );
        let ffmpeg_candidates = r.candidates(Backend::Ffmpeg);
        assert!(
            !ffmpeg_candidates
                .first()
                .is_some_and(|(_, source)| *source == Source::Override),
            "ffprobe alone must not also override ffmpeg: {ffmpeg_candidates:?}"
        );
    }

    /// Every override recorded here is authoritative once something
    /// actually resolves through the `Resolver` this builds: a nonexistent
    /// path errors immediately rather than falling through to `PATH`/
    /// well-known locations, and the ffprobe-sibling inference is no
    /// exception -- an inferred path that doesn't exist must error exactly
    /// the same way an explicit one would (see `Resolver::resolve`'s
    /// override-authority docs and commit e8acf8a).
    #[test]
    fn a_nonexistent_override_errors_instead_of_falling_through() {
        let bogus = PathBuf::from(if cfg!(windows) {
            r"C:\definitely\not\here.exe"
        } else {
            "/definitely/not/here"
        });
        let overrides = BackendOverrides {
            magick: Some(bogus.clone()),
            ..Default::default()
        };
        let r = overrides.resolver();
        let err = r.resolve(Backend::Magick).unwrap_err();
        assert_eq!(err.code, crate::ErrorCode::InvalidInvocation);
        assert_eq!(err.code.exit_code(), 2);
    }

    /// Same as above, but through the inferred ffprobe sibling rather than
    /// an explicit `ffprobe` override -- the inference must produce a real
    /// `Source::Override` candidate, subject to the same authority, not a
    /// silently-skipped one.
    #[test]
    fn an_inferred_ffprobe_sibling_that_does_not_exist_also_errors() {
        let ffmpeg = ffmpeg_bin_dir_path(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        let overrides = BackendOverrides {
            ffmpeg: Some(ffmpeg),
            ..Default::default()
        };
        let r = overrides.resolver();
        let err = r.resolve(Backend::Ffprobe).unwrap_err();
        assert_eq!(err.code, crate::ErrorCode::InvalidInvocation);
        assert_eq!(err.code.exit_code(), 2);
    }
}
