use std::path::PathBuf;

use clap::{Parser, Subcommand};
use convkit_core::{Backend, Resolver};

#[derive(Parser, Debug)]
#[command(
    name = "conv",
    version,
    about = "One command for everyday file conversion, offline"
)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Cli {
    /// Input paths, then optionally an output path or a bare `.ext`.
    pub paths: Vec<PathBuf>,

    /// Target format for batch conversion, e.g. `--to jpg`.
    #[arg(long)]
    pub to: Option<String>,

    /// Print the backend command instead of running it.
    ///
    /// Not `global`: this only means something for the implicit conversion
    /// path (no subcommand), so it must not show up in `conv doctor --help`,
    /// `conv install --help`, etc.
    #[arg(long)]
    pub dry_run: bool,

    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,

    /// Overwrite existing outputs.
    ///
    /// Not `global` -- see `dry_run`'s doc comment; the same reasoning
    /// applies to every conversion-only flag below it.
    #[arg(short = 'y', long)]
    pub overwrite: bool,

    /// Suppress progress output.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Assume yes when prompted to install a missing backend — for a script
    /// that wants the install-then-retry behaviour without a TTY to answer
    /// the interactive prompt. Contradicts `--no-install`, which asks the
    /// opposite question ("never install"): passing both is a usage error.
    #[arg(long, global = true, conflicts_with = "no_install")]
    pub yes: bool,

    /// Never offer to install a missing backend — always fail with the
    /// structured `backend_missing` error, even in an interactive session
    /// that could otherwise be prompted.
    #[arg(long, global = true)]
    pub no_install: bool,

    /// Write outputs into this directory.
    ///
    /// Not `global` -- see `dry_run`'s doc comment.
    #[arg(short = 'o', long)]
    pub outdir: Option<PathBuf>,

    /// Parallel jobs in batch mode. Defaults to the core count.
    ///
    /// Not `global` -- see `dry_run`'s doc comment.
    #[arg(short = 'j', long)]
    pub jobs: Option<usize>,

    /// Use this ffmpeg binary instead of the resolved one.
    #[arg(long, global = true, value_name = "PATH")]
    pub ffmpeg_path: Option<PathBuf>,
    /// Use this ffprobe binary instead of the resolved one.
    #[arg(long, global = true, value_name = "PATH")]
    pub ffprobe_path: Option<PathBuf>,
    /// Use this ImageMagick binary instead of the resolved one.
    #[arg(long, global = true, value_name = "PATH")]
    pub magick_path: Option<PathBuf>,
    /// Use this pandoc binary instead of the resolved one.
    #[arg(long, global = true, value_name = "PATH")]
    pub pandoc_path: Option<PathBuf>,
    /// Use this soffice (LibreOffice) binary instead of the resolved one.
    #[arg(long, global = true, value_name = "PATH")]
    pub soffice_path: Option<PathBuf>,
    /// Use this typst binary instead of the resolved one.
    #[arg(long, global = true, value_name = "PATH")]
    pub typst_path: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Report which backends are installed and how to install the rest.
    Doctor,
    /// Download and verify a managed backend.
    Install { backend: String },
    /// List every supported conversion.
    Capabilities,
    /// Update managed backends to the versions this convkit pins.
    #[command(long_about = "\
Brings managed backends (ffmpeg, ffprobe, pandoc, typst) in line with the \
exact versions THIS BUILD of convkit has pinned and verified -- not the \
latest versions available upstream. Every managed backend is installed \
from a pinned URL with a verified SHA-256 checksum; chasing latest \
upstream would mean fetching unverified binaries, which is exactly what \
the pinning exists to prevent.

The consequence: updating conv itself is what advances the pins. A newer \
convkit ships a newer manifest, and `conv update` then brings your \
backends in line with it.

This never replaces the conv binary itself. Self-replacement is a \
platform-specific security surface, and today there is nothing published \
to fetch anyway -- so instead this detects how conv was installed and \
prints the exact command to upgrade it, alongside the version currently \
running.

Unmanaged backends (magick/ImageMagick, soffice/LibreOffice) are only \
ever reported -- installed version, and the package-manager command that \
would update them -- never touched; convkit never runs a package manager \
on your behalf.

Updated backends take effect on your very next run: conv resolves each \
one by its path every time, so nothing needs a shell restart. Only a \
PATH change would need a new terminal, and `conv update` never touches \
PATH.

Use --check in a script or a scheduled job: it reports what's stale, \
changes nothing, and exits non-zero if anything is.")]
    Update {
        /// Report what's stale without installing or changing anything;
        /// exits with a non-zero status if any managed backend doesn't
        /// match its pinned version.
        #[arg(long)]
        check: bool,
    },
}

impl Cli {
    pub fn resolver(&self) -> Resolver {
        let mut r = Resolver::new();
        for (path, backend) in [
            (&self.ffmpeg_path, Backend::Ffmpeg),
            (&self.magick_path, Backend::Magick),
            (&self.pandoc_path, Backend::Pandoc),
            (&self.soffice_path, Backend::Soffice),
            (&self.typst_path, Backend::Typst),
        ] {
            if let Some(p) = path {
                r.with_override(backend, p.clone());
            }
        }
        if let Some(p) = &self.ffprobe_path {
            r.with_override(Backend::Ffprobe, p.clone());
        } else if let Some(p) = &self.ffmpeg_path {
            // ffprobe ships beside ffmpeg; honour the same override
            // directory -- but only when the caller didn't pin ffprobe
            // explicitly. An explicit --ffprobe-path must win over this
            // inference, not the other way around.
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

#[cfg(test)]
mod tests {
    use super::*;
    use convkit_core::Source;

    fn cli(ffmpeg_path: Option<PathBuf>, ffprobe_path: Option<PathBuf>) -> Cli {
        Cli {
            paths: vec![],
            to: None,
            dry_run: false,
            json: false,
            overwrite: false,
            quiet: false,
            yes: false,
            no_install: false,
            outdir: None,
            jobs: None,
            ffmpeg_path,
            ffprobe_path,
            magick_path: None,
            pandoc_path: None,
            soffice_path: None,
            typst_path: None,
            command: None,
        }
    }

    /// An explicit `--ffprobe-path` must take precedence over the sibling
    /// `--ffmpeg-path` otherwise infers from its own directory -- exactly
    /// the scenario the fix brief calls out: pinning `--ffprobe-path` at a
    /// nonexistent file to force the no-probe transcode path must not be
    /// silently overridden by the ffmpeg-directory inference just because
    /// `--ffmpeg-path` also happens to be set.
    #[test]
    fn explicit_ffprobe_path_wins_over_the_ffmpeg_sibling_inference() {
        let ffmpeg = PathBuf::from(r"C:\tools\ffmpeg\bin\ffmpeg.exe");
        let ffprobe = PathBuf::from(r"C:\elsewhere\my-ffprobe.exe");
        let c = cli(Some(ffmpeg), Some(ffprobe.clone()));
        let r = c.resolver();
        let candidates = r.candidates(Backend::Ffprobe);
        assert_eq!(
            candidates.first(),
            Some(&(ffprobe, Source::Override)),
            "{candidates:?}"
        );
    }

    /// With no explicit `--ffprobe-path`, `--ffmpeg-path` alone must still
    /// infer the sibling in the same directory -- the pre-existing
    /// behaviour this fix must not regress.
    ///
    /// Sibling inference is not a Windows concept: it walks whatever
    /// `Path::parent()` returns, so this must hold on every platform. The
    /// input path is built with a platform-appropriate literal (backslashes
    /// are just filename characters on Unix, so a Windows-style literal
    /// here would have no parent at all and the test would pass vacuously
    /// without ever exercising the inference).
    #[test]
    fn ffmpeg_path_alone_still_infers_the_sibling_ffprobe() {
        let ffmpeg = if cfg!(windows) {
            PathBuf::from(r"C:\tools\ffmpeg\bin\ffmpeg.exe")
        } else {
            PathBuf::from("/tools/ffmpeg/bin/ffmpeg")
        };
        let c = cli(Some(ffmpeg), None);
        let r = c.resolver();
        let candidates = r.candidates(Backend::Ffprobe);
        let expected_probe = if cfg!(windows) {
            r"C:\tools\ffmpeg\bin\ffprobe.exe"
        } else {
            "/tools/ffmpeg/bin/ffprobe"
        };
        assert_eq!(
            candidates.first(),
            Some(&(PathBuf::from(expected_probe), Source::Override)),
            "{candidates:?}"
        );
    }

    /// `--ffprobe-path` alone (no `--ffmpeg-path` at all) still overrides
    /// ffprobe, and must never accidentally also override ffmpeg itself.
    #[test]
    fn ffprobe_path_alone_overrides_only_ffprobe() {
        let ffprobe = PathBuf::from(r"C:\elsewhere\my-ffprobe.exe");
        let c = cli(None, Some(ffprobe.clone()));
        let r = c.resolver();
        assert_eq!(
            r.candidates(Backend::Ffprobe).first(),
            Some(&(ffprobe, Source::Override))
        );
        let ffmpeg_candidates = r.candidates(Backend::Ffmpeg);
        assert!(
            !ffmpeg_candidates
                .first()
                .is_some_and(|(_, source)| *source == Source::Override),
            "--ffprobe-path must not also override ffmpeg: {ffmpeg_candidates:?}"
        );
    }
}
