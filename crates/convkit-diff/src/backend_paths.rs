//! Builds the `convkit_core::Resolver` this whole crate runs conversions
//! and inspections through. Per the brief: "Resolve them via convkit-core's
//! Resolver rather than assuming PATH" -- every `magick`/`ffmpeg`/`ffprobe`
//! invocation in this crate goes through a `ResolvedBackend` from this
//! resolver, never a bare `Command::new("magick")`.
//!
//! `Resolver::resolve` already checks the `CONVKIT_MAGICK`/`CONVKIT_FFMPEG`/
//! `CONVKIT_FFPROBE` environment variables (`Source::Env`) before falling
//! back to `PATH`, so on a machine where ImageMagick isn't on `PATH` at all
//! -- true of this project's own dev machine, where ImageMagick 7 is
//! installed but not added to `PATH` -- setting `CONVKIT_MAGICK` is enough;
//! no override is required. The explicit `--magick-path`/`--ffmpeg-path`/
//! `--ffprobe-path` flags below exist for the same reason `conv`'s own CLI
//! carries the equivalent flags: a caller that wants to pin an exact binary
//! without touching its environment.

use std::path::PathBuf;

use convkit_core::{BackendOverrides, Resolver};

#[derive(Debug, Clone, Default, clap::Args)]
pub struct BackendPaths {
    /// Use this ImageMagick binary instead of the resolved one.
    #[arg(long, global = true, value_name = "PATH")]
    pub magick_path: Option<PathBuf>,
    /// Use this ffmpeg binary instead of the resolved one.
    #[arg(long, global = true, value_name = "PATH")]
    pub ffmpeg_path: Option<PathBuf>,
    /// Use this ffprobe binary instead of the resolved one.
    #[arg(long, global = true, value_name = "PATH")]
    pub ffprobe_path: Option<PathBuf>,
}

impl BackendPaths {
    /// The override-application and ffprobe-sibling-inference logic itself
    /// lives in `convkit_core::BackendOverrides` -- shared with `conv`'s own
    /// `Cli::resolver()`, rather than a second copy of it here. This crate
    /// exposes only three of the six overridable backends as flags (no
    /// `--pandoc-path`/`--soffice-path`/`--typst-path`), so the other three
    /// `BackendOverrides` fields are simply left at their `None` default.
    pub fn resolver(&self) -> Resolver {
        BackendOverrides {
            ffmpeg: self.ffmpeg_path.clone(),
            ffprobe: self.ffprobe_path.clone(),
            magick: self.magick_path.clone(),
            ..Default::default()
        }
        .resolver()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use convkit_core::{Backend, Source};

    /// The narrow thing left to prove here, once the override-application
    /// and ffprobe-sibling-inference precedence itself is tested thoroughly
    /// in `convkit-core`: that `BackendPaths::resolver()` maps each of its
    /// three flags onto the matching `BackendOverrides` field, and that the
    /// three backends this crate has no flag for at all (pandoc, soffice,
    /// typst) are never accidentally overridden.
    #[test]
    fn resolver_maps_every_flag_to_its_own_backend_override() {
        let paths = BackendPaths {
            magick_path: Some(PathBuf::from("/o/magick")),
            ffmpeg_path: Some(PathBuf::from("/o/ffmpeg")),
            ffprobe_path: Some(PathBuf::from("/o/ffprobe")),
        };
        let r = paths.resolver();
        for (backend, expected) in [
            (Backend::Magick, "/o/magick"),
            (Backend::Ffmpeg, "/o/ffmpeg"),
            (Backend::Ffprobe, "/o/ffprobe"),
        ] {
            assert_eq!(
                r.candidates(backend).first().map(|(p, _)| p.as_path()),
                Some(Path::new(expected)),
                "{backend:?}: wrong override made it through BackendPaths::resolver()"
            );
        }
        for backend in [Backend::Pandoc, Backend::Soffice, Backend::Typst] {
            let candidates = r.candidates(backend);
            assert!(
                !candidates
                    .first()
                    .is_some_and(|(_, s)| *s == Source::Override),
                "{backend:?}: this crate has no flag for it, so it must never be overridden: \
                 {candidates:?}"
            );
        }
    }
}
