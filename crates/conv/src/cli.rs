use std::path::PathBuf;

use clap::{Parser, Subcommand};
use convkit_core::{Resolver, Tuning};

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

    /// Show each backend command as it is spawned (resolved program, final
    /// argv) and the backend's full output afterwards, on stderr.
    ///
    /// Not `global` -- see `dry_run`'s doc comment. In a parallel batch the
    /// lines from different jobs interleave; this is a debugging aid, not a
    /// machine interface (that's --json's `backend_output`).
    #[arg(short = 'v', long, conflicts_with = "quiet")]
    pub verbose: bool,

    /// Fit the image within this geometry, aspect preserved: `1600x900`,
    /// `1600x` (width), `x900` (height), or `50%`. Image conversions only.
    ///
    /// Not `global` -- see `dry_run`'s doc comment; likewise the two flags
    /// below.
    #[arg(long, value_name = "GEOMETRY", value_parser = parse_resize_geometry)]
    pub resize: Option<String>,

    /// Quality 1-100 for lossy image targets (jpg/webp/avif) and
    /// image -> pdf [default: 92].
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u8).range(1..=100))]
    pub quality: Option<u8>,

    /// Reduce the palette to at most N colors (2-256). Raster image
    /// targets only.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u16).range(2..=256))]
    pub colors: Option<u16>,

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
    /// List every supported conversion; with a FORMAT, show that format's
    /// pairs, baked-in defaults, and which tuning flags apply.
    Capabilities {
        /// A format extension, e.g. `jpg` — shows what converts to and
        /// from it, the defaults its recipes use, and the applicable
        /// tuning flags (--resize/--quality/--colors).
        format: Option<String>,
    },
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

/// Validates `--resize` down to the five geometry forms convkit supports:
/// `W`, `WxH`, `Wx`, `xH`, `N%`. Strictly digits plus one `x` or a
/// trailing `%` — ImageMagick's own geometry grammar also accepts `@`,
/// `!`, `<`, `>` and `^` operators, and letting those through would make
/// the flag a side-channel into magick semantics this help text never
/// promised.
fn parse_resize_geometry(s: &str) -> Result<String, String> {
    let all_digits = |t: &str| t.chars().all(|c| c.is_ascii_digit());
    let ok = if let Some(pct) = s.strip_suffix('%') {
        !pct.is_empty() && all_digits(pct)
    } else if let Some((w, h)) = s.split_once('x') {
        (!w.is_empty() || !h.is_empty()) && all_digits(w) && all_digits(h)
    } else {
        !s.is_empty() && all_digits(s)
    };
    if ok {
        Ok(s.to_string())
    } else {
        Err(format!(
            "geometry must be W, WxH, Wx, xH, or N% (e.g. 1600x900, 50%), got {s:?}"
        ))
    }
}

impl Cli {
    /// The tuning this invocation asked for — empty (registry defaults)
    /// unless one of `--resize`/`--quality`/`--colors` was passed.
    pub fn tuning(&self) -> Tuning {
        Tuning {
            resize: self.resize.clone(),
            quality: self.quality,
            colors: self.colors,
        }
    }

    /// Builds the `Resolver` every conversion, `doctor`, and `update` run
    /// through, from whichever `--<backend>-path` flags were passed. The
    /// actual override-application and ffprobe-sibling-inference logic
    /// lives in `convkit_core::BackendOverrides` -- this just maps this
    /// struct's own six flag fields onto its six fields, so `conv`'s CLI
    /// surface (flag names, `#[arg(...)]` attributes, doc comments shown in
    /// `--help`) stays exactly where it already was, on `Cli` itself.
    pub fn resolver(&self) -> Resolver {
        convkit_core::BackendOverrides {
            ffmpeg: self.ffmpeg_path.clone(),
            ffprobe: self.ffprobe_path.clone(),
            magick: self.magick_path.clone(),
            pandoc: self.pandoc_path.clone(),
            soffice: self.soffice_path.clone(),
            typst: self.typst_path.clone(),
        }
        .resolver()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use convkit_core::Backend;

    fn cli(ffmpeg_path: Option<PathBuf>, ffprobe_path: Option<PathBuf>) -> Cli {
        Cli {
            paths: vec![],
            to: None,
            dry_run: false,
            json: false,
            overwrite: false,
            quiet: false,
            verbose: false,
            resize: None,
            quality: None,
            colors: None,
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

    /// The override-application and ffprobe-sibling-inference precedence
    /// this used to test directly is now `convkit_core::BackendOverrides`'s
    /// own responsibility, tested thoroughly (including the cross-platform
    /// path reasoning) in `convkit-core`. What's left to prove here is
    /// narrower but still real: that `Cli::resolver()` maps every one of
    /// its six flag fields onto the matching `BackendOverrides` field,
    /// rather than, say, `magick_path` ending up on `Backend::Pandoc`.
    #[test]
    fn resolver_maps_every_flag_to_its_own_backend_override() {
        let mut c = cli(
            Some(PathBuf::from("/o/ffmpeg")),
            Some(PathBuf::from("/o/ffprobe")),
        );
        c.magick_path = Some(PathBuf::from("/o/magick"));
        c.pandoc_path = Some(PathBuf::from("/o/pandoc"));
        c.soffice_path = Some(PathBuf::from("/o/soffice"));
        c.typst_path = Some(PathBuf::from("/o/typst"));

        let r = c.resolver();
        for (backend, expected) in [
            (Backend::Ffmpeg, "/o/ffmpeg"),
            (Backend::Ffprobe, "/o/ffprobe"),
            (Backend::Magick, "/o/magick"),
            (Backend::Pandoc, "/o/pandoc"),
            (Backend::Soffice, "/o/soffice"),
            (Backend::Typst, "/o/typst"),
        ] {
            assert_eq!(
                r.candidates(backend).first().map(|(p, _)| p.as_path()),
                Some(Path::new(expected)),
                "{backend:?}: wrong override made it through Cli::resolver()"
            );
        }
    }
}
