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

use convkit_core::{Backend, Resolver};

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
    pub fn resolver(&self) -> Resolver {
        let mut r = Resolver::new();
        if let Some(p) = &self.magick_path {
            r.with_override(Backend::Magick, p.clone());
        }
        if let Some(p) = &self.ffmpeg_path {
            r.with_override(Backend::Ffmpeg, p.clone());
        }
        if let Some(p) = &self.ffprobe_path {
            r.with_override(Backend::Ffprobe, p.clone());
        } else if let Some(p) = &self.ffmpeg_path {
            // Mirrors conv's own Cli::resolver(): ffprobe ships beside
            // ffmpeg, so an explicit --ffmpeg-path with no separate
            // --ffprobe-path still finds ffprobe in the same directory
            // rather than falling through to PATH/env for it alone.
            if let Some(dir) = p.parent() {
                let probe = dir.join(if cfg!(windows) {
                    "ffprobe.exe"
                } else {
                    "ffprobe"
                });
                r.with_override(Backend::Ffprobe, probe);
            }
        }
        r
    }
}
