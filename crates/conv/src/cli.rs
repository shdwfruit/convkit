use std::path::PathBuf;

use clap::{Parser, Subcommand};
use convkit_core::{Resolver, Tuning};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "conv",
    version,
    about = "One command for everyday file conversion, offline"
)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Cli {
    /// Input paths, then optionally an output path or a bare `.ext`. With
    /// `--max-size`, `--strip-metadata` or `--start`/`--end`/`--duration`, a
    /// lone input keeps its format and is written as NAME-SUFFIX.EXT
    /// (`clip.mp4` -> `clip-10mb.mp4`, `photo-stripped.jpg`,
    /// `clip-1m02s-1m10s.mp4`).
    pub paths: Vec<PathBuf>,

    /// Target format for batch conversion, e.g. `--to jpg`.
    #[arg(long)]
    pub to: Option<String>,

    /// Print the backend command instead of running it.
    // Not `global`: this only means something for the implicit conversion
    // path (no subcommand), so it must not show up in `conv doctor --help`,
    // `conv install --help`, etc. A `//` comment, not `///`: clap prints a
    // field's whole doc comment in `--help`.
    #[arg(long)]
    pub dry_run: bool,

    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,

    /// Overwrite existing outputs.
    // Not `global` -- see `dry_run`'s comment; the same reasoning applies to
    // every conversion-only flag below it.
    #[arg(short = 'y', long)]
    pub overwrite: bool,

    /// Suppress progress output.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Show each backend command as it is spawned (resolved program, final
    /// argv) and the backend's full output afterwards, on stderr.
    ///
    /// In a parallel batch the lines from different jobs interleave; this is
    /// a debugging aid, not a machine interface (that's --json's
    /// `backend_output`).
    // Not `global` -- see `dry_run`'s comment.
    #[arg(short = 'v', long, conflicts_with = "quiet")]
    pub verbose: bool,

    /// Fit within this geometry, aspect preserved: `1600x900`, `1600x`
    /// (width), `x900` (height), or `50%`. Never enlarges: a source already
    /// smaller is left alone, unless --upscale is given.
    // Not `global` -- see `dry_run`'s comment; likewise the flags below.
    #[arg(long, value_name = "GEOMETRY", value_parser = parse_resize_geometry)]
    pub resize: Option<String>,

    /// Let --resize enlarge a source smaller than its geometry. Enlarging
    /// adds no detail and makes a larger file, so conv warns when it does,
    /// and asks first past four times the source's pixels. Image, video and
    /// GIF targets.
    #[arg(long, requires = "resize", conflicts_with = "max_size")]
    pub upscale: bool,

    /// Quality 1-100 for lossy image targets (jpg/webp/avif) and
    /// image -> pdf [default: 92].
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u8).range(1..=100))]
    pub quality: Option<u8>,

    /// Reduce the palette to at most N colors (2-256). Raster image
    /// targets only.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u16).range(2..=256))]
    pub colors: Option<u16>,

    /// Cap the frame rate; slower sources are left alone. Video and GIF
    /// targets only.
    // Not `global` -- see `dry_run`'s comment, as with the three flags above.
    #[arg(long, value_name = "RATE", value_parser = parse_frame_rate)]
    pub fps: Option<String>,

    /// Constant-quality anchor for video targets: 0-51 for mp4/mov/mkv,
    /// 0-63 for webm. Lower is better.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u8).range(0..=63))]
    pub crf: Option<u8>,

    /// Keep each output at or under this size, choosing resolution, frame
    /// rate and bitrates to fit. Video targets only. SIZE is a number and a
    /// unit: 500kb, 10mb, 1.5gb, 10mib. A lone input keeps its format and is
    /// written as NAME-SIZE.EXT, e.g. clip-10mb.mp4; a --to batch writes into a
    /// folder named after the size, e.g. 10mb/clip.mp4, unless -o is given.
    // Not `global` -- see `dry_run`'s comment.
    #[arg(long, value_name = "SIZE", value_parser = parse_max_size, conflicts_with = "crf")]
    pub max_size: Option<convkit_core::size::MaxSize>,

    /// Remove location and the rest of the metadata: everything but the
    /// colour profile from images (orientation is applied first), and every
    /// tag but title, artist, album, album artist, composer, genre and
    /// track/disc numbers from video and audio. A lone input keeps its
    /// format: photo.jpg -> photo-stripped.jpg; a --to batch writes into a
    /// stripped folder, e.g. stripped/photo.jpg, unless -o is given.
    // Not `global` -- see `dry_run`'s comment.
    #[arg(long)]
    pub strip_metadata: bool,

    /// Keep the output from TIME: seconds (90, 90.5, 30s), m:ss (1:02.5) or
    /// h:mm:ss (1:02:03). A leading - counts back from the end (-30 is the
    /// last 30 s). The cut is exact. Video, GIF and audio targets. A lone
    /// input keeps its format and is written as NAME-START-END.EXT, e.g.
    /// talk-1m02s-1m10s.mp4; a --to batch writes into a folder named after
    /// the range, e.g. 1m02s-1m10s/talk.mp4, unless -o is given.
    // `allow_hyphen_values` so `--start -30` is a time, not an unknown
    // flag. A missing value then swallows the next flag, but the time
    // parser refuses that by name. Not `global` -- see `dry_run`'s comment.
    #[arg(long, value_name = "TIME", value_parser = parse_time, allow_hyphen_values = true)]
    pub start: Option<convkit_core::trim::Time>,

    /// Stop the output at TIME, in the same forms as --start; -5 stops 5 s
    /// before the end. An end past the file stops at its end.
    #[arg(
        long,
        value_name = "TIME",
        value_parser = parse_time,
        allow_hyphen_values = true,
        conflicts_with = "duration"
    )]
    pub end: Option<convkit_core::trim::Time>,

    /// Keep TIME's worth from --start (or from the beginning).
    #[arg(long, value_name = "TIME", value_parser = parse_duration)]
    pub duration: Option<convkit_core::trim::Time>,

    /// Assume yes to every prompt: installing a missing backend, or
    /// converting an extreme --max-size target or a large --upscale. For a script that wants
    /// either without a terminal to answer. Contradicts `--no-install`,
    /// which asks the opposite question ("never install"): passing both is
    /// a usage error.
    #[arg(long, global = true, conflicts_with = "no_install")]
    pub yes: bool,

    /// Never offer to install a missing backend — always fail with the
    /// structured `backend_missing` error, even in an interactive session
    /// that could otherwise be prompted.
    #[arg(long, global = true)]
    pub no_install: bool,

    /// Write outputs into this directory.
    // Not `global` -- see `dry_run`'s comment.
    #[arg(short = 'o', long)]
    pub outdir: Option<PathBuf>,

    /// Parallel jobs in batch mode. Defaults to the core count.
    // Not `global` -- see `dry_run`'s comment.
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

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Report which backends are installed and how to install the rest.
    Doctor,
    /// Cut clips out of a video or audio file in the terminal.
    #[command(long_about = "\
Cut clips out of a video or audio file in the terminal. The frame under \
the slider is drawn at the top, with a video bar and a loudness bar \
below it.

←/→ move the slider 10 ms; held, they move 2 s a second. PgUp/PgDn jump \
a tenth of the bars, ,/. step a frame, +/- zoom the bars, Home/End go to \
either end.

c marks a clip's start, then its end. With no bar selected a clip keeps \
the picture and the sound; ↑/↓ and Enter select a bar, and a clip then \
keeps that bar alone (a silent video, or the sound as m4a). u undoes, \
Esc drops an open mark, w writes every clip, q quits.

Each clip is written as conv FILE --start T --end T would write it, \
named for its range: talk-1m02s-1m10s.mp4.")]
    Trim {
        /// The video or audio file to cut.
        file: PathBuf,
        /// Write the clips with a picture in this format (an audio format
        /// keeps only the sound).
        #[arg(long)]
        to: Option<String>,
        /// Write the clips into this directory.
        #[arg(short = 'o', long)]
        outdir: Option<PathBuf>,
        /// Overwrite clips that already exist.
        #[arg(short = 'y', long)]
        overwrite: bool,
        /// Print each clip's command instead of running it.
        #[arg(long)]
        dry_run: bool,
        /// How to draw the picture. By default it is told from the
        /// terminal: kitty's protocol in kitty, Ghostty and WezTerm, iTerm2's
        /// in iTerm2, half blocks elsewhere.
        #[arg(long, value_name = "HOW", value_parser = ["blocks", "kitty", "iterm"])]
        graphics: Option<String>,
    },
    /// Download and verify a managed backend.
    Install { backend: String },
    /// List every supported conversion; with a FORMAT, show that format's
    /// pairs, baked-in defaults, and which tuning flags apply.
    Capabilities {
        /// A format extension, e.g. `jpg` — shows what converts to and
        /// from it, the defaults its recipes use, and which tuning flags
        /// apply.
        format: Option<String>,
    },
    /// List the files here and what each one could be converted into.
    #[command(
        long_about = "Lists the files in PATHS (the current directory by default) and, for each, the formats convkit could convert it into.

This is the contextual companion to `conv capabilities`, which answers the same question globally (every pair convkit knows) or per format (`conv capabilities heic`). Neither tells you what is actually in front of you, and the alternative inverted the question: `conv <dir> --to jpg --dry-run` makes you name a target before it tells you anything, when choosing the target is what you wanted help with.

Pure lookup on the file extension. Nothing is opened, decoded or probed, and no backend is run -- so this reports what convkit SUPPORTS, not what this machine can currently run. For that, see `conv doctor`.

Files convkit does not recognise are listed with `--` rather than hidden, so an unconvertible file is never a silent omission. Only regular files are listed: a directory argument is read one level deep, and subdirectories inside it are neither descended into nor shown. A path that does not exist is reported as an error rather than described, and exits 2."
    )]
    Scan {
        /// Files or directories to list. Defaults to the current directory.
        paths: Vec<std::path::PathBuf>,
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
platform-specific security surface, and one this pinned-and-verified \
design has no honest answer for -- the checksum of a release that does \
not exist yet cannot be pinned in the build that would verify it. So \
instead this detects how conv was installed and prints the exact command \
to upgrade it, alongside the version currently running.

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

/// A dimension past this is not a size any real raster or frame reaches --
/// JPEG's own dimension fields are 16 bits and top out at 65535, and
/// nothing else convkit targets goes further. Refusing it here, rather than
/// letting it reach the `u32` parse downstream, keeps the error at the edge,
/// where the message can still name the forms this flag accepts.
const MAX_GEOMETRY_VALUE: u32 = 65_535;

/// Validates `--resize` down to the five geometry forms convkit supports:
/// `W`, `WxH`, `Wx`, `xH`, `N%`. Strictly digits plus one `x` or a
/// trailing `%` — ImageMagick's own geometry grammar also accepts `@`,
/// `!`, `<`, `>` and `^` operators, and letting those through would make
/// the flag a side-channel into magick semantics this help text never
/// promised.
fn parse_resize_geometry(s: &str) -> Result<String, String> {
    let all_digits = |t: &str| t.chars().all(|c| c.is_ascii_digit());
    // A dimension of zero passes `all_digits` but is not a size. magick and
    // ffmpeg each do something different and surprising with it, and
    // neither is what was asked for. A dimension over MAX_GEOMETRY_VALUE
    // passes `all_digits` too, and would otherwise overflow `u32::parse`
    // downstream.
    let positive = |t: &str| {
        !t.is_empty()
            && all_digits(t)
            && t.chars().any(|c| c != '0')
            && t.parse::<u32>().is_ok_and(|n| n <= MAX_GEOMETRY_VALUE)
    };
    let ok = if let Some(pct) = s.strip_suffix('%') {
        positive(pct)
    } else if let Some((w, h)) = s.split_once('x') {
        match (w.is_empty(), h.is_empty()) {
            (true, true) => false,
            (true, false) => positive(h),
            (false, true) => positive(w),
            (false, false) => positive(w) && positive(h),
        }
    } else {
        positive(s)
    };
    if ok {
        Ok(s.to_string())
    } else {
        Err(format!(
            "geometry must be W, WxH, Wx, xH, or N% (e.g. 1600x900, 50%), got {s:?}"
        ))
    }
}

/// Validates `--fps` down to the three forms convkit supports: an integer,
/// a decimal, or an `N/D` rational. ffmpeg's `fps` filter accepts a great
/// deal more -- expressions, `source_fps`, constants -- and letting those
/// through would make the flag a side-channel into ffmpeg's filter grammar
/// this help text never promised, exactly as `parse_resize_geometry`
/// refuses magick's `@ ! < > ^`.
///
/// Zero is refused at both ends: a zero rate is not a slower rate, and a
/// zero denominator divides by zero wherever the cap is compared.
fn parse_frame_rate(s: &str) -> Result<String, String> {
    let bad =
        || format!("frame rate must be N, N.N, or N/D (e.g. 24, 29.97, 30000/1001), got {s:?}");
    let positive = |t: &str| -> bool {
        !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()) && t.chars().any(|c| c != '0')
    };
    let ok = if let Some((n, d)) = s.split_once('/') {
        positive(n) && positive(d)
    } else if let Some((w, f)) = s.split_once('.') {
        !f.is_empty()
            && f.chars().all(|c| c.is_ascii_digit())
            && w.chars().all(|c| c.is_ascii_digit())
            && (positive(w) || f.chars().any(|c| c != '0'))
    } else {
        positive(s)
    };
    if ok {
        Ok(s.to_string())
    } else {
        Err(bad())
    }
}

/// The whole grammar lives in `convkit_core::size` so a library caller
/// gets exactly the same answer; this only adapts it to clap.
fn parse_max_size(s: &str) -> Result<convkit_core::size::MaxSize, String> {
    convkit_core::size::parse(s)
}

/// The grammar lives in `convkit_core::trim`; this only adapts it to clap.
fn parse_time(s: &str) -> Result<convkit_core::trim::Time, String> {
    convkit_core::trim::parse_time(s)
}

fn parse_duration(s: &str) -> Result<convkit_core::trim::Time, String> {
    convkit_core::trim::parse_duration(s)
}

impl Cli {
    /// The tuning this invocation asked for — empty (registry defaults)
    /// unless one of `--resize`/`--quality`/`--colors`/`--fps`/`--crf`/
    /// `--max-size`/`--start`/`--end`/`--duration` was passed.
    pub fn tuning(&self) -> Tuning {
        Tuning {
            resize: self.resize.clone(),
            quality: self.quality,
            colors: self.colors,
            fps: self.fps.clone(),
            crf: self.crf,
            max_size: self.max_size.clone(),
            upscale: self.upscale,
            strip_metadata: self.strip_metadata,
            // `check` has already refused a range that does not hold
            // together; see main.
            range: self.range().ok().flatten(),
            // No flag asks for a silent clip; conv trim sets it per clip.
            mute: false,
        }
    }

    /// The range the three flags ask for, or why they contradict each
    /// other.
    pub fn range(&self) -> Result<Option<convkit_core::trim::Range>, String> {
        convkit_core::trim::Range::new(self.start.clone(), self.end.clone(), self.duration.clone())
    }

    /// Checks across flags that clap's per-flag parsers cannot see, so a
    /// contradiction is a usage error before anything runs, like clap's
    /// own.
    pub fn check(&self) -> Result<(), String> {
        self.range().map(|_| ())
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

    use clap::Parser;

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
            upscale: false,
            quality: None,
            colors: None,
            fps: None,
            crf: None,
            max_size: None,
            strip_metadata: false,
            start: None,
            end: None,
            duration: None,
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

    #[test]
    fn max_size_parses_and_rejects_a_bare_number() {
        let c = Cli::try_parse_from(["conv", "a.mp4", "--max-size", "10MB"]).unwrap();
        assert_eq!(c.max_size.as_ref().unwrap().bytes, 10_000_000);
        assert_eq!(c.tuning().max_size, c.max_size);
        let e = Cli::try_parse_from(["conv", "a.mp4", "--max-size", "10"]).unwrap_err();
        assert!(e.to_string().contains("add a unit"), "{e}");
    }

    #[test]
    fn times_parse_including_ones_counted_from_the_end() {
        let c = Cli::try_parse_from(["conv", "a.mp4", "--start", "-30"]).unwrap();
        assert_eq!(
            c.start.as_ref().map(|t| (t.ms, t.from_end)),
            Some((30_000, true))
        );
        let c = Cli::try_parse_from(["conv", "a.mp4", "--start=-0:30", "--end", "-5"]).unwrap();
        assert!(c.end.as_ref().is_some_and(|t| t.from_end));
        assert!(c.tuning().range.is_some());
    }

    #[test]
    fn end_and_duration_conflict() {
        let e =
            Cli::try_parse_from(["conv", "a.mp4", "--end", "10", "--duration", "5"]).unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn an_order_that_cannot_work_fails_the_check() {
        let c = Cli::try_parse_from(["conv", "a.mp4", "--start", "1:10", "--end", "1:02"]).unwrap();
        assert_eq!(
            c.check().unwrap_err(),
            "--start 1:10 is not before --end 1:02"
        );
    }

    #[test]
    fn max_size_and_crf_conflict() {
        let e = Cli::try_parse_from(["conv", "a.mp4", "--max-size", "10mb", "--crf", "20"])
            .unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
