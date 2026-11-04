use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use convkit_core::{Resolver, Tuning};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "conv",
    version = env!("CONV_VERSION"),
    about = "One command for everyday file conversion, offline"
)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Cli {
    /// Input paths, then optionally an output path or a bare `.ext`. With
    /// `--max-size`, `--compress`, `--strip-metadata`,
    /// `--start`/`--end`/`--duration`, `--speed`, `--rotate`, `--flip` or
    /// `--crop`, a lone input keeps its format and is written as
    /// NAME-SUFFIX.EXT (`clip.mp4` -> `clip-10mb.mp4`, `scan-compressed.pdf`,
    /// `photo-stripped.jpg`, `clip-1m02s-1m10s.mp4`, `clip-2x.mp4`,
    /// `photo-rotated.jpg`, `photo-flipped.jpg`, `photo-cropped.jpg`).
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
    /// smaller is left alone, unless --upscale is given. A lone input keeps
    /// its format and is written as NAME-SIZE.EXT, e.g. photo-50pct.jpg or
    /// clip-1280w.mp4.
    // Not `global` -- see `dry_run`'s comment; likewise the flags below.
    #[arg(long, value_name = "GEOMETRY", value_parser = parse_resize_geometry)]
    pub resize: Option<String>,

    /// Let --resize enlarge a source smaller than its geometry. Enlarging
    /// adds no detail and makes a larger file, so conv warns when it does,
    /// and asks first past four times the source's pixels. Image, video and
    /// GIF targets.
    #[arg(long, requires = "resize", conflicts_with = "max_size")]
    pub upscale: bool,

    /// Quality 1-100 for lossy image targets (jpg/webp/avif/heic) and
    /// image -> pdf [default: 92; 60 for heic].
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

    /// Keep each output at or under this size: a video's resolution, frame
    /// rate and bitrates are chosen to fit, and a PDF's images go down a
    /// step at a time until it fits. Video and PDF targets. SIZE is a number
    /// and a unit: 500kb, 10mb, 1.5gb, 10mib. A lone input keeps its format
    /// and is written as NAME-SIZE.EXT, e.g. clip-10mb.mp4; a --to batch
    /// writes into a folder named after the size, e.g. 10mb/clip.mp4, unless
    /// -o is given.
    // Not `global` -- see `dry_run`'s comment.
    #[arg(long, value_name = "SIZE", value_parser = parse_max_size, conflicts_with = "crf")]
    pub max_size: Option<convkit_core::size::MaxSize>,

    /// Make a PDF smaller without changing how it looks: images drawn
    /// above 225 dpi go to 150 dpi, and high-quality JPEGs and lossless
    /// photos are re-encoded. Text, links, bookmarks, form fields and tags
    /// stay as they are, and a result that isn't smaller is a copy of the
    /// original. A lone input is written as NAME-compressed.pdf; a --to pdf
    /// batch writes into a compressed folder, unless -o is given.
    // Not `global` -- see `dry_run`'s comment.
    #[arg(long)]
    pub compress: bool,

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

    /// Play the output at FACTOR times the source's speed: 2 (or 2x) is
    /// twice as fast, 0.5 is half as fast, from 0.01 to 100. The sound keeps
    /// its pitch. Video, GIF and audio targets. A lone input keeps its
    /// format and is written as NAME-FACTORx.EXT, e.g. clip-2x.mp4; a --to
    /// batch writes into a folder named after the speed, e.g. 2x/clip.mp4,
    /// unless -o is given.
    // Not `global` -- see `dry_run`'s comment.
    #[arg(long, value_name = "FACTOR", value_parser = parse_speed)]
    pub speed: Option<convkit_core::speed::Speed>,

    /// Let the pitch move with --speed, as a tape played fast does: higher
    /// when sped up, lower when slowed down. Adds -pitched to the name.
    #[arg(long, requires = "speed")]
    pub shift_pitch: bool,

    /// Turn the picture as it is shown: right (90 degrees clockwise), left
    /// or 180. The picture itself is turned, so it shows the same way in
    /// every app; a JPEG, MP4 or MOV that was sideways only because of its
    /// orientation tag is fixed without re-encoding. Images, video and GIFs.
    /// A lone input keeps its format: photo.jpg -> photo-rotated.jpg; a --to
    /// batch writes into a rotated folder, e.g. rotated/photo.jpg, unless -o
    /// is given.
    // `allow_hyphen_values` so `--rotate -90` is a turn, not an unknown
    // flag. Not `global` -- see `dry_run`'s comment.
    #[arg(
        long,
        value_name = "TURN",
        value_parser = parse_rotate,
        allow_hyphen_values = true
    )]
    pub rotate: Option<convkit_core::rotate::Turn>,

    /// The voice that reads text aloud (txt, md, html, docx, odt or rtf to
    /// m4a, mp3, wav or aiff): a name from `conv speak --voices`, `system`
    /// for the system's own, or a Piper voice file (.onnx). On macOS conv
    /// picks the best Premium or Enhanced voice in your language.
    // Not `global` -- see `dry_run`'s comment.
    #[arg(long, value_name = "VOICE")]
    pub voice: Option<String>,

    /// Words per minute for text read aloud, 80 to 450. [default: the
    /// voice's own, about 175]
    #[arg(long, value_name = "WPM", value_parser = parse_rate)]
    pub rate: Option<u16>,

    /// Lock a PDF with a password (AES-256), written as NAME-locked.pdf, or
    /// into a locked folder with --to pdf. conv asks for it twice without
    /// showing it, or reads CONVKIT_PASSWORD; it never takes one on the
    /// command line, where it would stay in your shell history.
    // A flag, not an option with a value, for exactly that reason. Not
    // `global` -- see `dry_run`'s comment.
    #[arg(long)]
    pub password: bool,

    /// Write a copy of a PDF without its password and restrictions, as
    /// NAME-unlocked.pdf, or into an unlocked folder with --to pdf. conv
    /// asks for the password when the PDF needs one, or reads
    /// CONVKIT_PASSWORD.
    #[arg(long, conflicts_with = "password")]
    pub unlock: bool,

    /// With --password, turn off printing, copying or editing for whoever
    /// opens it, e.g. --restrict print,copy. PDF apps honour this, though
    /// nothing forces them to.
    #[arg(
        long,
        value_name = "WHAT",
        value_delimiter = ',',
        value_parser = parse_restriction,
        requires = "password"
    )]
    pub restrict: Vec<convkit_core::pdf::Restriction>,

    /// The pages of a PDF to save as pictures, by split's grammar between
    /// commas: 5, 1-3, 11-z (z is the last page), as in --pages 1-3,7.
    /// Every page without it. One page is one file, report-3.png; more go
    /// into a folder, report/report-01.png and on.
    // Not `global` -- see `dry_run`'s comment.
    // Held as typed, so clap does not read the list as several values;
    // `parse_pages` has already checked it.
    #[arg(long, value_name = "PAGES", value_parser = parse_pages)]
    pub pages: Option<String>,

    /// The resolution a PDF's pages are drawn at when saved as pictures,
    /// 36-1200 [default: 150, which makes a Letter page 1275x1650].
    /// --resize sets the pixel size instead.
    // Not `global` -- see `dry_run`'s comment.
    #[arg(
        long,
        value_name = "N",
        value_parser = clap::value_parser!(u32).range(
            i64::from(convkit_core::pdf::raster::MIN_DPI)..=i64::from(convkit_core::pdf::raster::MAX_DPI)
        ),
        conflicts_with = "resize"
    )]
    pub dpi: Option<u32>,

    /// Keep part of the picture as it is shown (after its orientation,
    /// --rotate and --flip, before --resize): WxH+X+Y, a box from the top left; WxH, a
    /// box in the middle; W:H, the largest box of that shape, e.g. 16:9,
    /// 1:1 or 9:16; or auto, which cuts away black bars around a video or a
    /// plain border around a picture. Images, video and GIFs. A lone input
    /// keeps its format: photo.jpg -> photo-cropped.jpg; a --to batch
    /// writes into a cropped folder, e.g. cropped/photo.jpg, unless -o is
    /// given.
    // Not `global` -- see `dry_run`'s comment.
    #[arg(long, value_name = "BOX", value_parser = parse_crop)]
    pub crop: Option<convkit_core::crop::Spec>,

    /// Put this file's sound on the video in place of its own: a song, a
    /// voice-over, or another video's sound. The video is stream-copied
    /// where nothing else changes it. A sound longer than the video stops
    /// where it ends; a shorter one leaves the rest silent. Video targets
    /// (mp4, mov, mkv, webm). A lone input keeps its format:
    /// clip.mp4 -> clip-audio.mp4.
    // Not `global` -- see `dry_run`'s comment.
    #[arg(long, value_name = "FILE")]
    pub audio: Option<PathBuf>,

    /// Mix --audio's sound under the video's own instead of replacing it,
    /// at LEVEL of the video's loudness: --mix is a quarter as loud
    /// (25%), --mix=50% half. Both are measured first, so the level holds
    /// whatever each was recorded at. Named NAME-mixed.EXT.
    // `require_equals` so `--mix clip.mp4` leaves the path a path. Not
    // `global` -- see `dry_run`'s comment.
    #[arg(
        long,
        value_name = "LEVEL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "25%",
        requires = "audio",
        value_parser = parse_mix_level
    )]
    pub mix: Option<convkit_core::soundtrack::Level>,

    /// Burn the subtitles of an .srt, .vtt or .ass file into the picture:
    /// white with a black outline, near the bottom, sized to the picture,
    /// in the system's fonts, right-to-left text included. A cut or speed
    /// change moves them with the video. Video and GIF targets. A lone
    /// input keeps its format: clip.mp4 -> clip-subtitled.mp4.
    // Not `global` -- see `dry_run`'s comment.
    #[arg(long, value_name = "FILE")]
    pub subtitles: Option<PathBuf>,

    /// Mirror the picture as it is shown: horizontal (or h) swaps left and
    /// right, vertical (or v) swaps top and bottom. With --rotate, the turn
    /// comes first. The picture itself is flipped, as --rotate turns it.
    /// Images, video and GIFs. A lone input keeps its format: photo.jpg ->
    /// photo-flipped.jpg, or photo-rotated-flipped.jpg with --rotate; a
    /// --to batch writes into a flipped folder unless -o is given.
    // Not `global` -- see `dry_run`'s comment.
    #[arg(long, value_name = "DIRECTION", value_parser = parse_flip)]
    pub flip: Option<convkit_core::rotate::Flip>,

    /// Copy what was written to the clipboard, ready to paste: a picture
    /// as itself (and its file), an md, csv, html, txt, json, yaml or toml
    /// file as its text, and anything else, or several outputs, as the
    /// files. Over SSH it goes through the terminal, which takes only a
    /// picture or text.
    // Not `global` -- see `dry_run`'s comment.
    #[arg(long)]
    pub copy: bool,

    /// Take the input from the clipboard: files copied in Finder, Explorer
    /// or a file manager, or a copied picture or screenshot, which is
    /// converted into this folder as clipboard.EXT. `conv --paste .jpg`,
    /// `conv --paste --to webp`, or `conv --paste` alone to save a copied
    /// picture as it is. Any other path is the output.
    // Not `global` -- see `dry_run`'s comment.
    #[arg(long)]
    pub paste: bool,

    /// Assume yes to every prompt: installing a missing backend, converting
    /// an extreme --max-size target or a large --upscale, or rewriting a
    /// signed PDF. For a script that wants any of these without a terminal
    /// to answer. Contradicts `--no-install`,
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

    /// Parallel jobs in batch mode. Defaults to the core count, and to 2 for
    /// a batch that saves PDFs' pages, which each use every core.
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
    /// Use this qpdf binary instead of the resolved one.
    #[arg(long, global = true, value_name = "PATH")]
    pub qpdf_path: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// `conv fmt`'s arguments.
#[derive(Args, Debug, Clone)]
pub struct FmtArgs {
    /// The JSON, YAML or TOML file (or CSV, to convert with --to). `-`, or
    /// nothing with text piped in, reads standard input.
    pub input: Option<PathBuf>,
    /// Write the result here instead of printing it: a file, or a bare
    /// `.ext` for FILE's name with that extension. Another extension writes
    /// that language. Writing over FILE needs -y.
    pub output: Option<PathBuf>,
    /// Write the result into this directory, under FILE's name.
    #[arg(short = 'o', long)]
    pub outdir: Option<PathBuf>,
    /// Read the text as this, not by its extension or how it looks.
    #[arg(long, value_name = "LANG", value_parser = ["json", "yaml", "toml", "csv"])]
    pub from: Option<String>,
    /// Write this language instead of the text's own.
    #[arg(long, value_name = "LANG", value_parser = ["json", "yaml", "toml", "csv"])]
    pub to: Option<String>,
    /// Indent by N spaces, JSON and YAML. [default: 2]
    #[arg(
        long,
        value_name = "N",
        value_parser = clap::value_parser!(u8).range(1..=8),
        conflicts_with_all = ["tab", "minify"]
    )]
    pub indent: Option<u8>,
    /// Indent JSON with tabs.
    #[arg(long, conflicts_with = "minify")]
    pub tab: bool,
    /// Pack JSON onto one line.
    #[arg(long)]
    pub minify: bool,
    /// Sort every object's keys.
    #[arg(long)]
    pub sort_keys: bool,
    /// Read JSON5 and JSONC, and YAML with tags conv can't keep, saying
    /// what was dropped, rather than refuse them.
    #[arg(long)]
    pub lenient: bool,
    /// Only check that the text is valid.
    #[arg(
        long,
        conflicts_with_all = ["output", "outdir", "copy", "to", "indent", "tab", "minify", "sort_keys"]
    )]
    pub check: bool,
    /// Overwrite OUT if it exists, FILE included.
    #[arg(short = 'y', long)]
    pub overwrite: bool,
    /// Put the result on the clipboard instead of printing it (with OUT,
    /// as well as writing it).
    #[arg(long)]
    pub copy: bool,
    /// Take the text from the clipboard. The one path given is then OUT.
    #[arg(long)]
    pub paste: bool,
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
named for its range: talk-1m02s-1m10s.mp4, and w asks before it \
replaces a file already there.")]
    Trim {
        /// The video or audio file to cut.
        #[arg(required_unless_present = "paste")]
        file: Option<PathBuf>,
        /// Cut the file copied to the clipboard.
        #[arg(long, conflicts_with = "file")]
        paste: bool,
        /// Write the clips with a picture in this format (an audio format
        /// keeps only the sound).
        #[arg(long)]
        to: Option<String>,
        /// Write the clips into this directory.
        #[arg(short = 'o', long)]
        outdir: Option<PathBuf>,
        /// Overwrite clips that already exist, without asking.
        #[arg(short = 'y', long)]
        overwrite: bool,
        /// Print each clip's command instead of running it.
        #[arg(long)]
        dry_run: bool,
        /// How to draw the picture. By default it is told from the
        /// terminal: kitty's protocol in kitty and Ghostty, iTerm2's in
        /// iTerm2 and WezTerm, half blocks in Apple's Terminal, and
        /// sextants (six pixels a character) elsewhere and under tmux.
        /// Use blocks where sextants show as boxes or question marks.
        #[arg(
            long,
            value_name = "HOW",
            value_parser = ["sextants", "blocks", "kitty", "iterm"]
        )]
        graphics: Option<String>,
    },
    /// Make a QR code (`conv qr "https://example.com"` writes qr.png), or
    /// read one (`conv qr --read photo.jpg`).
    #[command(long_about = "\
Makes a QR code that holds TEXT, a link or any other text, exactly as \
given. The text is in the code itself, with no redirect service behind \
it, so the code never expires and nobody counts its scans.

conv draws PNG and SVG itself. Any other image format (jpg, webp, pdf, \
...) is drawn as a PNG and converted, which needs ImageMagick. Before \
writing, conv reads the code back and checks it gives exactly TEXT.

With --logo, the image sits in the middle on a white square about a \
fifth of the code's width, and the code is made to survive losing 30% \
of itself instead of 15%, so it still scans.

With --read, conv reads the QR codes in pictures instead and prints what \
each one holds on its own line. A Wi-Fi code's network, security and \
password are also shown. Links are printed, never opened. Screenshots \
and phone photos both work, in png, jpg, gif, webp or svg, and in other \
formats (heic, avif, tiff, ...) through ImageMagick.")]
    Qr {
        /// What the code holds: a link or any text, always the text even
        /// when a file has that name (--read reads a picture). With --wifi,
        /// the network's name.
        #[arg(required_unless_present_any = ["paste", "wifi", "read"])]
        text: Option<String>,
        /// The file to write, or a bare `.ext` for qr.EXT (`.svg`). Without
        /// it, conv writes qr.png.
        output: Option<PathBuf>,
        /// Write the file into this directory (created if missing).
        #[arg(short = 'o', long)]
        outdir: Option<PathBuf>,
        /// Put this image (svg, png, jpg, gif or webp) in the middle.
        #[arg(long, value_name = "FILE")]
        logo: Option<PathBuf>,
        /// Width and height in pixels, rounded down to whole pixels a
        /// module so the edges stay sharp. An SVG scales to any size; this
        /// is only the size it opens at. [default: 1024]
        #[arg(
            long,
            value_name = "PX",
            value_parser = clap::value_parser!(u32).range(1..=convkit_core::qr::MAX_SIZE as i64)
        )]
        size: Option<u32>,
        /// Overwrite the file if it exists.
        #[arg(short = 'y', long)]
        overwrite: bool,
        /// Also copy the code to the clipboard, ready to paste.
        #[arg(long)]
        copy: bool,
        /// Take from the clipboard what it holds: copied text or a link is
        /// what the code holds (the one path given is then OUT), and a
        /// copied picture or file is the logo.
        #[arg(long)]
        paste: bool,
        /// Make a code that joins a saved Wi-Fi network: TEXT is its name
        /// (the one this computer is on without it), and the file is
        /// NAME-wifi.png. See conv wifi.
        #[arg(long, conflicts_with = "paste")]
        wifi: bool,
        /// Read the QR codes in these pictures instead of making one.
        #[arg(
            long,
            value_name = "PICTURE",
            num_args = 1..,
            conflicts_with_all = ["text", "output", "outdir", "logo", "size", "overwrite", "copy", "paste", "wifi"]
        )]
        read: Vec<PathBuf>,
    },
    /// Make ASCII art from a picture: `conv ascii photo.jpg` prints it here.
    #[command(long_about = "\
Turns a picture into ASCII art. Without OUT it prints it here, sized to \
the window. With OUT it writes a .txt, .png, .html or any other picture \
format conv writes, 100 characters wide unless --width or --height says \
otherwise.

Styles: ascii (characters by brightness, with | / \\ - _ on clear edges), \
blocks (two colours a character where colour can show), braille (8 dots \
a character, the most detail), or your own characters from least ink to \
most, like --chars \" .:oO@\".

--bg says what the art will be shown on, so it doesn't look like a \
negative there. --clipboard also copies the text; paste it inside ``` in \
Discord, Slack or GitHub to keep it lined up.

-i opens a preview: the arrows change the width and contrast, [ ] the \
brightness, c the style, b the background, o colour; w writes, y copies, \
and q quits and prints the command for what you made.")]
    Ascii {
        /// The picture: an image, a GIF or a video (its first frame).
        #[arg(required_unless_present = "paste")]
        input: Option<PathBuf>,
        /// The file to write (.txt, .png, .html, or another picture
        /// format), or a bare `.ext` for NAME-ascii.EXT. Without it, the
        /// art is printed.
        output: Option<PathBuf>,
        /// Write the file into this directory (created if missing). Alone,
        /// it writes NAME-ascii.txt there.
        #[arg(short = 'o', long)]
        outdir: Option<PathBuf>,
        /// Columns. [default: the window when printing, 100 otherwise]
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=400))]
        width: Option<u32>,
        /// Rows. With --width, the art fits inside both.
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=400))]
        height: Option<u32>,
        /// ascii, blocks, braille, or your own characters from least ink
        /// to most, like " .:oO@".
        #[arg(long, value_name = "STYLE", default_value = "ascii")]
        chars: String,
        /// What the art will be shown on.
        #[arg(long, value_name = "BG", default_value = "dark", value_parser = ["dark", "light"])]
        bg: String,
        /// Keep the picture's colours (blocks do by default where colour
        /// can show).
        #[arg(long, overrides_with = "no_color")]
        color: bool,
        /// No colour, even for blocks.
        #[arg(long, overrides_with = "color")]
        no_color: bool,
        /// More or less contrast, -100 to 100.
        #[arg(
            long,
            value_name = "N",
            default_value_t = 0,
            allow_hyphen_values = true,
            value_parser = clap::value_parser!(i32).range(-100..=100)
        )]
        contrast: i32,
        /// Brighter or darker, -100 to 100.
        #[arg(
            long,
            value_name = "N",
            default_value_t = 0,
            allow_hyphen_values = true,
            value_parser = clap::value_parser!(i32).range(-100..=100)
        )]
        brightness: i32,
        /// Also copy the art as plain text.
        #[arg(long, visible_alias = "copy")]
        clipboard: bool,
        /// Take the picture from the clipboard: a copied picture, or one
        /// copied file. The one path given is then OUT.
        #[arg(long)]
        paste: bool,
        /// Change the look with keys and see it as you go; prints the
        /// command for it when you quit.
        #[arg(short = 'i', long, conflicts_with = "clipboard")]
        tune: bool,
        /// Overwrite the file if it exists.
        #[arg(short = 'y', long)]
        overwrite: bool,
    },
    /// Format, check or convert JSON, YAML and TOML: `conv fmt data.json`.
    #[command(long_about = "\
Pretty-prints JSON, YAML or TOML (two spaces, or --indent N, or --tab), \
or packs JSON onto one line with --minify, keeping every key in its place \
and every number as written. Invalid text is refused with the line, the \
column, a caret under the spot and the usual cause.

The text comes from FILE, from a pipe, or from the clipboard with --paste. \
Its language is told from the file's extension, or from how it looks; \
--from says it. The result is printed (in colour on a terminal), written \
to OUT, or put on the clipboard with --copy. --to, or OUT's extension, \
writes another language: conv fmt --paste --to yaml --copy turns copied \
JSON into YAML. To format a file where it is, name it twice and add -y: \
conv fmt data.json data.json -y. conv won't write over a file whose \
comments it would lose.

--check only says whether the text is valid. --lenient reads JSON5 and \
JSONC (comments, trailing commas, single quotes, unquoted keys) and drops \
YAML tags, saying what it dropped.")]
    Fmt(FmtArgs),
    /// Read text aloud into a sound file: `conv speak "Hello" hello.m4a`.
    #[command(long_about = "\
Reads TEXT aloud into a sound file, with the speech engine your system has: \
say on macOS, Windows' own voices, and espeak-ng on Linux (or Piper, with \
--voice VOICE.onnx). Nothing leaves your machine.

TEXT is what is read: the words given, `-` or a pipe for what is piped in, \
-f FILE for a file (txt, md, html, docx, odt or rtf), or --paste for the \
clipboard. OUT is the file to write, or a bare `.ext` (.m4a, .mp3, .wav, \
.aiff); without it, conv writes speech.mp3, or NAME.mp3 for -f NAME.EXT.

On macOS conv reads with the best Premium or Enhanced voice in your \
language, and says how to download one when there is none. --voices lists \
the voices --voice takes.")]
    Speak(SpeakArgs),
    /// Set a screenshot on a background: `conv beautify shot.png` writes
    /// shot-beautified.png.
    #[command(long_about = "\
Sets a screenshot on a background with padding, rounded corners and a soft \
shadow, ready for a post or a slide: conv beautify shot.png writes \
shot-beautified.png beside it. With --paste and --copy it goes from the \
clipboard to the clipboard.

The screenshot itself is copied pixel for pixel, never scaled, so a 2x \
(Retina) screenshot stays sharp, and the PNG keeps its colour profile and \
pixel density. --padding and --radius are in points, the pixels of a 1x \
screen, doubled on a 2x screenshot so it looks the same. Left to conv, they \
grow with the screenshot.

A picture whose corners are already clear, like a macOS window captured \
with its shadow, keeps its own shape: conv adds no corners and no second \
shadow unless --radius or --shadow asks for them.

OUT can be any picture format conv writes; PNG is drawn by conv itself, \
and the rest are converted from it, which needs ImageMagick.")]
    Beautify {
        /// The screenshot: a PNG, JPG or any picture conv reads.
        #[arg(required_unless_present = "paste")]
        input: Option<PathBuf>,
        /// The file to write, or a bare `.ext` for NAME-beautified.EXT.
        /// Without it, NAME-beautified.png beside the input.
        output: Option<PathBuf>,
        /// Write the file into this directory (created if missing).
        #[arg(short = 'o', long)]
        outdir: Option<PathBuf>,
        /// aurora, sunset, candy, ocean, mint, night, transparent, a colour
        /// like '#1e1e2e', or colours joined by commas for a gradient of
        /// your own. [default: aurora]
        #[arg(long, value_name = "BG", value_parser = parse_background)]
        bg: Option<convkit_core::beautify::Background>,
        /// Space around the screenshot, in points. [default: 8% of the
        /// screenshot's size]
        #[arg(long, value_name = "PT", value_parser = clap::value_parser!(u32).range(0..=10_000))]
        padding: Option<u32>,
        /// Corner radius in points; 0 keeps them square. [default: about
        /// 1% of the screenshot's size]
        #[arg(long, value_name = "PT", value_parser = clap::value_parser!(u32).range(0..=10_000))]
        radius: Option<u32>,
        /// Draw no shadow.
        #[arg(long, overrides_with = "shadow")]
        no_shadow: bool,
        /// Draw the shadow even under a picture with its own shape.
        #[arg(long, overrides_with = "no_shadow")]
        shadow: bool,
        /// Overwrite the file if it exists.
        #[arg(short = 'y', long)]
        overwrite: bool,
        /// Also copy the result to the clipboard, ready to paste.
        #[arg(long)]
        copy: bool,
        /// Take the screenshot from the clipboard: a copied picture, or one
        /// copied file. The one path given is then OUT. A copied picture is
        /// written here as clipboard-beautified.png.
        #[arg(long)]
        paste: bool,
        /// Say what would be written, and how big, without writing it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print the main colours of a picture: `conv colors photo.jpg`.
    #[command(
        visible_alias = "colours",
        long_about = "\
Prints the main colours of a picture, the most common first, each with its \
share of the picture, as HEX, RGB and HSL, with a swatch of each on a \
terminal that shows colour. Colours a person would call the same are \
counted as one, so a picture with few colours gives fewer than asked for.

--at X,Y gives the colour of one pixel instead, counted from the top-left \
corner as the picture is shown (a phone photo's EXIF turn included). \
--code writes the colours as code: css (custom properties), swiftui, \
tailwind (Tailwind 4's @theme) or tailwind3 (tailwind.config.js).

The picture can be any one conv reads; PNG, JPG, GIF, WebP and SVG are read \
by conv itself, and HEIC, AVIF, TIFF and the rest go through ImageMagick. - \
or a pipe reads one piped in, and --paste a copied one. --copy puts the \
result on the clipboard: the HEX values, one a line, or the code."
    )]
    Colors {
        /// The picture; - reads one piped in.
        input: Option<PathBuf>,
        /// How many colours, 1 to 16.
        #[arg(
            short = 'n',
            long,
            value_name = "N",
            default_value_t = 6,
            value_parser = clap::value_parser!(u8).range(1..=16)
        )]
        count: u8,
        /// The colour of the pixel at X,Y, from the top-left corner.
        #[arg(long, value_name = "X,Y", value_parser = parse_point)]
        at: Option<(u32, u32)>,
        /// Write the colours as code: css, swiftui, tailwind or tailwind3.
        #[arg(long, value_name = "LANG", value_parser = parse_code)]
        code: Option<convkit_core::palette::Code>,
        /// Also copy the result to the clipboard.
        #[arg(long)]
        copy: bool,
        /// Take the picture from the clipboard: a copied picture, or one
        /// copied file.
        #[arg(long, conflicts_with = "input")]
        paste: bool,
    },
    /// Show a saved Wi-Fi network's password, or make a QR code that joins it.
    #[command(long_about = "\
Shows a saved Wi-Fi network's name, security and password: the one this \
computer is on, or NAME. --qr writes a QR code that joins it instead, for a \
phone to scan.

On macOS the password is in the keychain, and macOS asks you to allow conv \
to read it, with your login or an administrator's password; conv says so \
before the prompt appears. macOS hides which network it is on from programs \
without Location access, so when it does, conv lists the saved networks \
and asks which. Windows reads it with netsh, and Linux with NetworkManager's \
nmcli.

The password is printed only to a terminal. In a pipe, a file or --json it \
is left out unless --show-password is given, so it doesn't end up in a log \
by accident; --copy puts it on the clipboard instead.")]
    Wifi {
        /// The network's name. Without it, the one this computer is on.
        name: Option<String>,
        /// List the saved networks.
        #[arg(long, conflicts_with_all = ["name", "qr", "copy", "show_password"])]
        list: bool,
        /// Write a QR code that joins the network instead: NAME-wifi.png, or
        /// the file or bare `.ext` given.
        // `--qr` alone is "", which clap's own path parser refuses.
        #[arg(
            long,
            value_name = "OUT",
            num_args = 0..=1,
            default_missing_value = "",
            value_parser = parse_any_path
        )]
        qr: Option<PathBuf>,
        /// Write the code into this directory (created if missing).
        #[arg(short = 'o', long, requires = "qr")]
        outdir: Option<PathBuf>,
        /// Overwrite the code if it exists.
        #[arg(short = 'y', long, requires = "qr")]
        overwrite: bool,
        /// Also copy the password to the clipboard, or with --qr the code.
        #[arg(long)]
        copy: bool,
        /// Print the password into a pipe, a file or --json too.
        #[arg(long, conflicts_with = "qr")]
        show_password: bool,
        /// Print the command that reads the password, without running it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show what each version of conv added.
    #[command(long_about = "\
Shows what each version of conv added, newest first, from the changelog \
built into this copy, so it works offline. Give a version to see just that \
one, or --since with the version you had to see what's new after an \
upgrade.")]
    Changelog {
        /// One version to show, like 0.5.0.
        version: Option<String>,
        /// Show the versions after this one, like the one you upgraded from.
        #[arg(long, value_name = "VERSION", conflicts_with = "version")]
        since: Option<String>,
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
    /// Show what a file is: format, size, streams, dimensions, pages.
    #[command(long_about = "\
Shows what each FILE is. For video and audio: the container, duration, \
bitrate and every stream, with its codec, size, frame rate, rotation, HDR, \
sample rate, channels and language. For pictures: the format, size, colour \
profile, bit depth, transparency, frames, orientation tag, camera, date \
taken and whether a GPS position is recorded. For PDFs: pages, page size, \
whether it is encrypted, and what made it. Every file also lists what conv \
can convert it into.

Nothing is changed or uploaded. Video and audio are read with ffprobe and \
pictures with ImageMagick; when one is missing, conv shows what it can \
without it and says how to install it. A folder is read one level deep.")]
    Info {
        /// The files to describe, or folders to describe the files in.
        #[arg(required = true)]
        files: Vec<std::path::PathBuf>,
    },
    /// Update managed backends to the versions this convkit pins.
    #[command(long_about = "\
Brings managed backends (ffmpeg, ffprobe, pandoc, typst, qpdf) in line with the \
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
    /// Join files into one, in the order given: PDFs and pictures into a PDF,
    /// videos into a video, sound into an audio file.
    #[command(long_about = "\
Joins files into one, in the order given. The last argument is the output, \
and its extension says what is made: .pdf from PDFs and pictures, a video \
(.mp4, .mov, .mkv, .webm) from videos, or an audio file (.mp3, .m4a, .wav, \
.flac, .opus, .ogg) from audio files and the sound of videos. conv refuses \
to replace an existing file unless -y is given. A folder adds every file \
directly inside it that the output takes, in natural order (p2 before p10).

PDFs are joined by qpdf, which rewrites the files' structure rather than \
re-rendering pages, so text stays selectable and links keep working. The \
first file's bookmarks are kept; later files' bookmarks and every file's \
permission restrictions are not, and conv prints a note when that happens. \
A picture among PDFs gets a page the size of the PDF page beside it, turned \
to suit the picture, with the picture centred at its own size or shrunk to \
fit. Pictures alone get a page each of their own size, as `conv a.jpg b.jpg \
out.pdf` makes.

A PDF that needs a password to open is asked for it, or opened with \
CONVKIT_PASSWORD. --password locks the merged PDF.

Videos and audio files that match (codecs, picture size, frame rate, sound) \
are joined without re-encoding. Otherwise every clip is encoded once to the \
first one's size, frame rate and sound: a clip of another shape is scaled \
to fit, with black bars, never stretched, a clip without sound is silent \
for its length, and a note says which files differed and how.")]
    Merge(MergeArgs),

    /// Split a PDF into one file per page, or one per page range.
    #[command(long_about = "\
Writes one file per page, or one file per RANGE. A RANGE is a page (5), a \
span (1-3), or uses z for the last page (11-z); 5-1 writes pages 5 to 1 in \
reverse order. Files are named after the input and their pages -- \
report-01.pdf ... report-12.pdf, or report-1-3.pdf -- and go next to the \
input, or into -o DIR.

Pages no range covers are left out with a warning, and a page in more than \
one range is written to each, with a note. A range past the last page is \
refused before anything is written. A locked PDF is asked for its \
password, and --password locks every file written.")]
    Split(SplitArgs),

    /// Keep, reorder, delete and turn a PDF's pages: `conv pages report.pdf 3 1-2 5-z`.
    #[command(long_about = "\
Writes one PDF from another's pages: the PAGES listed, in that order, \
without the --delete pages, turned by --rotate. A page is a number (5), a \
span (1-3), z for the last page (11-z), or a reversed span (5-1); give \
several with spaces or commas, and repeat a page to repeat it. With no \
PAGES, every page is kept. Every page number is the input's own, in all \
three places.

The file is named after the input and its pages -- report-1-3.pdf, \
report-3_1-2_5-12.pdf, and -rotated when pages turn -- next to the input or \
in -o DIR. Name it yourself as the last argument (out.pdf). The input's \
bookmarks, links and metadata are kept; conv says when a bookmark led to a \
page that is gone.")]
    Pages(PagesArgs),

    /// The text and developer tools: base64, hash, uuid, jwt, time, link,
    /// count, case, chars, lorem and diff.
    #[command(flatten)]
    Tools(crate::commands::tools::Tool),
}

#[derive(Args, Debug, Clone)]
pub struct SpeakArgs {
    /// What to read: the words themselves, or `-` for what is piped in.
    /// With -f or --paste, the one path given is OUT.
    pub text: Option<String>,

    /// The file to write, or a bare `.ext` (.m4a, .mp3, .wav, .aiff).
    /// [default: speech.mp3]
    pub output: Option<PathBuf>,

    /// Read this file aloud instead: txt, md, html, docx, odt or rtf.
    #[arg(short = 'f', long, value_name = "FILE", conflicts_with = "paste")]
    pub file: Option<PathBuf>,

    /// Read what is on the clipboard: copied text, or one copied file.
    #[arg(long)]
    pub paste: bool,

    /// The voice: a name from --voices, `system` for the system's own, or
    /// a Piper voice file (.onnx).
    #[arg(long, value_name = "VOICE")]
    pub voice: Option<String>,

    /// Words per minute, 80 to 450. [default: the voice's own, about 175]
    #[arg(long, value_name = "WPM", value_parser = parse_rate)]
    pub rate: Option<u16>,

    /// List the voices --voice takes, and write nothing.
    #[arg(long, conflicts_with_all = ["file", "paste"])]
    pub voices: bool,

    /// Write the file into this directory (created if missing).
    #[arg(short = 'o', long)]
    pub outdir: Option<PathBuf>,

    /// Overwrite the file if it exists.
    #[arg(short = 'y', long)]
    pub overwrite: bool,

    /// Print the commands instead of running them.
    #[arg(long)]
    pub dry_run: bool,

    /// Also copy the sound file to the clipboard, ready to paste.
    #[arg(long)]
    pub copy: bool,
}

#[derive(Args, Debug, Clone)]
pub struct MergeArgs {
    /// Files to join, then the output file: `a.pdf b.pdf out.pdf`,
    /// `a.mp4 b.mov out.mp4`. A folder adds every file in it the output
    /// takes.
    #[arg(required = true, value_name = "FILE")]
    pub paths: Vec<PathBuf>,

    /// Overwrite the output if it exists.
    #[arg(short = 'y', long)]
    pub overwrite: bool,

    /// Print the backend commands instead of running them.
    #[arg(long)]
    pub dry_run: bool,

    /// Show each backend command as it runs, and the backend's own output.
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// Videos only: fit the joined video within this size (1280x720, 1280x,
    /// x720, 50%), never larger than the first clip. Encodes every clip.
    #[arg(long, value_name = "GEOMETRY", value_parser = parse_resize_geometry)]
    pub resize: Option<String>,

    /// Videos only: the encode's quality, 0-51 for mp4, mov and mkv, 0-63 for
    /// webm; lower is better. Encodes every clip.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u8).range(0..=63))]
    pub crf: Option<u8>,

    /// Videos only: keep the joined video at or under this size (25mb,
    /// 1.5gb), choosing resolution, frame rate and bitrates to fit, as
    /// `conv clip.mp4 --max-size` does.
    #[arg(long, value_name = "SIZE", value_parser = parse_max_size, conflicts_with = "crf")]
    pub max_size: Option<convkit_core::size::MaxSize>,

    #[command(flatten)]
    pub lock: LockArgs,
}

/// `--password` and `--restrict` for a PDF command, meaning what they mean
/// on a PDF kept as a PDF: lock what it writes.
#[derive(Args, Debug, Clone, Default)]
pub struct LockArgs {
    /// Lock what it writes with a password (AES-256). conv asks for it
    /// twice without showing it, or reads CONVKIT_PASSWORD.
    #[arg(long)]
    pub password: bool,

    /// With --password, turn off printing, copying or editing for whoever
    /// opens it, e.g. --restrict print,copy.
    #[arg(
        long,
        value_name = "WHAT",
        value_delimiter = ',',
        value_parser = parse_restriction,
        requires = "password"
    )]
    pub restrict: Vec<convkit_core::pdf::Restriction>,
}

#[derive(Args, Debug, Clone)]
pub struct PagesArgs {
    /// The PDF.
    #[arg(value_name = "PDF")]
    pub input: PathBuf,

    /// The pages to keep, in the order to write them: 5, 1-3, 11-z (z is
    /// the last page), 5-1 (reversed), or several with commas (1-3,5).
    /// Last, optionally, the file to write (out.pdf).
    #[arg(value_name = "PAGES")]
    pub pages: Vec<String>,

    /// Leave these pages out: --delete 4,6-7.
    #[arg(long, value_name = "PAGES")]
    pub delete: Vec<String>,

    /// Turn pages: right (clockwise), left or 180 turns every page, and
    /// right:2,4 only those. Repeat it for different turns.
    // `allow_hyphen_values` so `--rotate -90:2` is a turn, not a flag.
    #[arg(long, value_name = "TURN[:PAGES]", allow_hyphen_values = true)]
    pub rotate: Vec<String>,

    /// Write the file into this folder (created if missing).
    #[arg(short = 'o', long, value_name = "DIR")]
    pub outdir: Option<PathBuf>,

    /// Overwrite the file if it exists.
    #[arg(short = 'y', long)]
    pub overwrite: bool,

    /// Print the qpdf command instead of running it.
    #[arg(long)]
    pub dry_run: bool,

    /// Show the qpdf command as it runs, and qpdf's own output.
    #[arg(short = 'v', long)]
    pub verbose: bool,

    #[command(flatten)]
    pub lock: LockArgs,
}

#[derive(Args, Debug, Clone)]
pub struct SplitArgs {
    /// The PDF to split.
    #[arg(value_name = "PDF")]
    pub input: PathBuf,

    /// Pages for each file: 5, 1-3, 11-z (z is the last page). Without
    /// ranges, every page gets its own file.
    #[arg(value_name = "RANGE")]
    pub ranges: Vec<String>,

    /// Write the files into this folder (created if missing).
    #[arg(short = 'o', long, value_name = "DIR")]
    pub outdir: Option<PathBuf>,

    /// Overwrite files that already exist.
    #[arg(short = 'y', long)]
    pub overwrite: bool,

    /// Print the qpdf commands instead of running them.
    #[arg(long)]
    pub dry_run: bool,

    /// Show each qpdf command as it runs, and qpdf's own output.
    #[arg(short = 'v', long)]
    pub verbose: bool,

    #[command(flatten)]
    pub lock: LockArgs,
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
fn parse_pages(s: &str) -> Result<String, String> {
    convkit_core::pdf::parse_list(s)
        .map(|_| s.to_string())
        .map_err(|e| e.message)
}

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

fn parse_speed(s: &str) -> Result<convkit_core::speed::Speed, String> {
    convkit_core::speed::parse(s)
}

fn parse_rotate(s: &str) -> Result<convkit_core::rotate::Turn, String> {
    convkit_core::rotate::parse(s)
}

fn parse_background(s: &str) -> Result<convkit_core::beautify::Background, String> {
    convkit_core::beautify::Background::parse(s)
}

fn parse_code(s: &str) -> Result<convkit_core::palette::Code, String> {
    convkit_core::palette::Code::parse(s)
}

/// A path, the empty one included.
fn parse_any_path(s: &str) -> Result<PathBuf, String> {
    Ok(PathBuf::from(s))
}

/// `X,Y`: two whole numbers of pixels, from the top-left corner.
fn parse_point(s: &str) -> Result<(u32, u32), String> {
    let bad = || format!("expected X,Y in pixels from the top-left corner, like 120,45; got {s:?}");
    let (x, y) = s.split_once(',').ok_or_else(bad)?;
    let n = |t: &str| t.trim().parse::<u32>().map_err(|_| bad());
    Ok((n(x)?, n(y)?))
}

fn parse_restriction(s: &str) -> Result<convkit_core::pdf::Restriction, String> {
    convkit_core::pdf::Restriction::parse(s)
}

fn parse_mix_level(s: &str) -> Result<convkit_core::soundtrack::Level, String> {
    convkit_core::soundtrack::parse_level(s)
}

fn parse_crop(s: &str) -> Result<convkit_core::crop::Spec, String> {
    convkit_core::crop::parse(s)
}

fn parse_flip(s: &str) -> Result<convkit_core::rotate::Flip, String> {
    convkit_core::rotate::parse_flip(s)
}

fn parse_duration(s: &str) -> Result<convkit_core::trim::Time, String> {
    convkit_core::trim::parse_duration(s)
}

/// Words per minute, in the range every engine reads at.
fn parse_rate(s: &str) -> Result<u16, String> {
    use convkit_core::speech::{RATE_MAX, RATE_MIN};
    let wpm: u16 = s
        .trim()
        .parse()
        .map_err(|_| format!("{s:?} is not a number of words per minute, like 200"))?;
    if !(RATE_MIN..=RATE_MAX).contains(&wpm) {
        return Err(format!(
            "{wpm} words per minute is out of range; give {RATE_MIN} to {RATE_MAX} \
             (most voices read about 175)"
        ));
    }
    Ok(wpm)
}

impl Cli {
    /// The tuning this invocation asked for — empty (registry defaults)
    /// unless one of `--resize`/`--quality`/`--colors`/`--fps`/`--crf`/
    /// `--max-size`/`--start`/`--end`/`--duration`/`--speed`/`--rotate`/
    /// `--flip`/`--crop` was passed.
    pub fn tuning(&self) -> Tuning {
        Tuning {
            resize: self.resize.clone(),
            quality: self.quality,
            colors: self.colors,
            fps: self.fps.clone(),
            crf: self.crf,
            max_size: self.max_size.clone(),
            compress: self.compress,
            upscale: self.upscale,
            strip_metadata: self.strip_metadata,
            // `check` has already refused a range that does not hold
            // together; see main.
            range: self.range().ok().flatten(),
            // No flag asks for a silent clip; conv trim sets it per clip.
            mute: false,
            speed: self.speed,
            shift_pitch: self.shift_pitch,
            rotate: self.rotate,
            voice: self.voice.clone(),
            rate: self.rate,
            dpi: self.dpi,
            pages: self
                .pages
                .as_deref()
                .and_then(|p| convkit_core::pdf::parse_list(p).ok()),
            crop: self.crop,
            audio: self
                .audio
                .as_ref()
                .map(|path| convkit_core::soundtrack::Request {
                    path: path.clone(),
                    mix: self.mix,
                }),
            subtitles: self.subtitles.clone(),
            flip: self.flip,
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
        // `--mix 30%` left the level a path: say how to write it.
        if self.mix.is_some() {
            if let Some(level) = self.paths.iter().find(|p| {
                let s = p.to_string_lossy();
                convkit_core::soundtrack::parse_level(&s).is_ok() && s.ends_with('%')
            }) {
                return Err(format!(
                    "--mix takes its level after an =: write --mix={}",
                    level.display()
                ));
            }
        }
        if self.paste && self.paths.len() > 1 {
            return Err(
                "--paste takes the input from the clipboard; give at most the output's name or .ext"
                    .to_string(),
            );
        }
        self.range().map(|_| ())
    }

    /// Builds the `Resolver` every conversion, `doctor`, and `update` run
    /// through, from whichever `--<backend>-path` flags were passed. The
    /// actual override-application and ffprobe-sibling-inference logic
    /// lives in `convkit_core::BackendOverrides` -- this just maps this
    /// struct's own seven flag fields onto its seven fields, so `conv`'s CLI
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
            qpdf: self.qpdf_path.clone(),
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
            compress: false,
            strip_metadata: false,
            start: None,
            end: None,
            duration: None,
            speed: None,
            shift_pitch: false,
            rotate: None,
            voice: None,
            rate: None,
            password: false,
            unlock: false,
            restrict: vec![],
            pages: None,
            dpi: None,
            crop: None,
            audio: None,
            mix: None,
            subtitles: None,
            copy: false,
            paste: false,
            flip: None,
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
            qpdf_path: None,
            command: None,
        }
    }

    /// The override-application and ffprobe-sibling-inference precedence
    /// this used to test directly is now `convkit_core::BackendOverrides`'s
    /// own responsibility, tested thoroughly (including the cross-platform
    /// path reasoning) in `convkit-core`. What's left to prove here is
    /// narrower but still real: that `Cli::resolver()` maps every one of
    /// its seven flag fields onto the matching `BackendOverrides` field,
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
        c.qpdf_path = Some(PathBuf::from("/o/qpdf"));

        let r = c.resolver();
        for (backend, expected) in [
            (Backend::Ffmpeg, "/o/ffmpeg"),
            (Backend::Ffprobe, "/o/ffprobe"),
            (Backend::Magick, "/o/magick"),
            (Backend::Pandoc, "/o/pandoc"),
            (Backend::Soffice, "/o/soffice"),
            (Backend::Typst, "/o/typst"),
            (Backend::Qpdf, "/o/qpdf"),
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

    #[test]
    fn a_speed_parses_into_the_tuning() {
        let c = Cli::try_parse_from(["conv", "a.mp4", "--speed", "2x"]).unwrap();
        let t = c.tuning();
        assert_eq!(t.speed, Some(convkit_core::speed::parse("2").unwrap()));
        assert!(!t.shift_pitch);
        let c = Cli::try_parse_from(["conv", "a.mp4", "--speed", "0.5", "--shift-pitch"]).unwrap();
        assert!(c.tuning().shift_pitch);
    }

    #[test]
    fn a_speed_that_changes_nothing_is_a_usage_error() {
        let e = Cli::try_parse_from(["conv", "a.mp4", "--speed", "1"]).unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::ValueValidation);
        assert!(
            e.to_string().contains("1 is the speed it already plays at"),
            "{e}"
        );
    }

    #[test]
    fn shift_pitch_needs_a_speed() {
        let e = Cli::try_parse_from(["conv", "a.mp4", "--shift-pitch"]).unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn a_turn_parses_into_the_tuning() {
        let c = Cli::try_parse_from(["conv", "a.jpg", "--rotate", "left"]).unwrap();
        assert_eq!(c.tuning().rotate, Some(convkit_core::rotate::Turn::Left));
        // A negative angle is a turn, not a flag.
        let c = Cli::try_parse_from(["conv", "a.jpg", "--rotate", "-90"]).unwrap();
        assert_eq!(c.tuning().rotate, Some(convkit_core::rotate::Turn::Left));
    }

    #[test]
    fn a_flip_parses_into_the_tuning_in_full_or_short() {
        use convkit_core::rotate::Flip;
        for (text, want) in [
            ("horizontal", Flip::Horizontal),
            ("h", Flip::Horizontal),
            ("H", Flip::Horizontal),
            ("vertical", Flip::Vertical),
            ("v", Flip::Vertical),
        ] {
            let c = Cli::try_parse_from(["conv", "a.jpg", "--flip", text]).unwrap();
            assert_eq!(c.tuning().flip, Some(want), "{text}");
            assert_eq!(c.tuning().rotate, None, "{text}");
        }
        let c = Cli::try_parse_from(["conv", "a.jpg", "--rotate", "right", "--flip", "h"]).unwrap();
        let change = c.tuning().change().unwrap();
        assert_eq!(change.turn, Some(convkit_core::rotate::Turn::Right));
        assert_eq!(change.flip, Some(Flip::Horizontal));
    }

    #[test]
    fn a_flip_that_is_not_a_direction_is_a_usage_error() {
        for text in ["diagonal", "x", "", "hv"] {
            let e = Cli::try_parse_from(["conv", "a.jpg", "--flip", text]).unwrap_err();
            assert_eq!(e.kind(), clap::error::ErrorKind::ValueValidation, "{text}");
            assert!(
                e.to_string()
                    .contains("expected horizontal or vertical (h or v)"),
                "{e}"
            );
        }
    }

    #[test]
    fn a_turn_that_is_not_a_quarter_is_a_usage_error() {
        let e = Cli::try_parse_from(["conv", "a.jpg", "--rotate", "45"]).unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::ValueValidation);
        assert!(e.to_string().contains("expected right, left or 180"), "{e}");
    }
}
