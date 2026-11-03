//! A PDF's pages as images: `conv report.pdf .png` draws every page, as
//! `report/report-01.png` and on, and `--pages 1-3,7` picks some. One page
//! is one file, `ticket.png`, with no folder.
//!
//! qpdf merges the annotations and form fields into each page first
//! (`--generate-appearances --flatten-annotations=screen`), since Typst
//! draws a page's content and not what sits on top of it: comments,
//! highlights, stamps, signatures and filled-in fields would vanish. It
//! also takes off an owner password's restrictions and repairs a damaged
//! file. Typst then draws the pages, in runs small enough to hold in
//! memory, as PNG. A JPEG is encoded from that PNG by ffmpeg, so PNG and
//! JPEG need only backends conv installs; WebP and HEIC by ImageMagick,
//! which writes every WebP and HEIC conv makes.
//!
//! The planning here is pure. `read` and `read_sizes` run qpdf and Typst
//! to learn the page count and sizes the plan needs, and `exec` runs it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Serialize;

use super::range::PageRange;
use super::{display_name, path_arg};
use crate::error::{ConvError, ErrorCode, Remediation, Result};
use crate::plan::{ConversionPlan, PlannedStep, Prepare};
use crate::procutil::backend_command;
use crate::{winpath, Backend, Format, OutputMode, Tuning};

/// The resolution pages are drawn at without `--dpi`: what pdftoppm and
/// most "PDF to JPG" sites use. A Letter page is 1275 x 1650.
pub const DEFAULT_DPI: u32 = 150;
pub const MIN_DPI: u32 = 36;
pub const MAX_DPI: u32 = 1200;

/// A run expected to write more files than this asks first, as a long
/// series of stills does.
pub const ASK_ABOVE: usize = 1000;

/// The largest page conv draws: 800 MB as RGBA. A Letter or A4 page at
/// 1200 dpi is about 140 million pixels; a poster at that resolution would
/// take more memory than most machines have.
pub const MAX_PIXELS: u64 = 200_000_000;

/// How many pixels one Typst run draws. Typst holds every page of a run in
/// memory until it has written them all: measured on a 142-page Letter
/// PDF at 150 dpi, 1.15 GB for one run of every page and 630 MB for runs
/// of 47. This makes runs of 30 such pages. A page larger than this is a
/// run of its own.
const RUN_PIXELS: u64 = 64_000_000;

/// The oldest Typst that has `typst eval`, which reads the page sizes.
/// Typst draws PDF pages from 0.14.
const MIN_TYPST: (u32, u32) = (0, 15);

/// The picture formats a PDF's pages can be saved as.
pub const TARGETS: [Format; 4] = [Format::Png, Format::Jpg, Format::Webp, Format::Heic];

/// Whether `from -> to` saves a PDF's pages as images.
pub fn is_pair(from: Format, to: Format) -> bool {
    from == Format::Pdf && TARGETS.contains(&to)
}

/// The flags these pairs take, for `conv capabilities`.
pub fn flags(to: Format) -> Vec<&'static str> {
    let mut f = vec!["--pages", "--dpi", "--resize"];
    if to != Format::Png {
        f.push("--quality");
    }
    f
}

/// The quality a lossy page is written at without `--quality`: the anchors
/// every other JPEG, WebP and HEIC conv writes use.
pub fn default_quality(to: Format) -> Option<u8> {
    match to {
        Format::Jpg | Format::Webp => Some(92),
        Format::Heic => Some(60),
        _ => None,
    }
}

/// What conv reads of a PDF before drawing its pages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PdfPages {
    pub count: u32,
    /// Each page's size as Typst draws it: the crop box, turned as the
    /// page's `/Rotate` says, in hundredths of a point. Integers, because
    /// `MediaProbe` derives `Eq`. Empty when the sizes were not read.
    pub sizes: Vec<(u32, u32)>,
    /// qpdf had to repair the file to read it.
    pub damaged: bool,
}

/// What `--json` and `--dry-run` say about the pages, and what the run
/// asks about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PagesReport {
    /// Pages in the PDF; absent when it could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// The pages saved, in order.
    pub pages: Vec<u32>,
    /// Whether they go into a folder, the plan's `output`, rather than
    /// being one file.
    pub folder: bool,
    /// Their file names, in page order.
    pub files: Vec<String>,
    /// The resolution they are drawn at; absent under `--resize`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dpi: Option<u32>,
    /// More than `ASK_ABOVE` files: the run asks first.
    pub needs_confirmation: bool,
    /// What the question is about, as printed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// A plan, with what `exec` needs to put the pages in place.
#[derive(Debug, Clone)]
pub(crate) struct PagesPlan {
    pub plan: ConversionPlan,
    /// The work folder every step writes into.
    pub work: PathBuf,
    /// The file each step chain leaves for each page, in `pages` order.
    /// Empty when the plan is a template (sizes not read).
    pub produced: Vec<PathBuf>,
    /// The page count and sizes were read, so the steps are exact.
    pub exact: bool,
    /// The note a damaged file gets. Among the plan's warnings, so a dry
    /// run shows it; a real run says it with the backends' notes, as
    /// `conv split` does.
    pub repaired: Option<String>,
}

/// `1,234`.
pub fn grouped(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The pages `ranges` pick from a PDF of `count` pages, ascending and each
/// once, since each is saved under its own number. Every page without
/// `--pages`.
pub fn selected(ranges: Option<&[PageRange]>, count: u32, name: &str) -> Result<Vec<u32>> {
    if count == 0 {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!("{name} has no pages"),
        ));
    }
    let Some(ranges) = ranges else {
        return Ok((1..=count).collect());
    };
    let mut pages = Vec::new();
    for r in ranges {
        pages.extend(r.resolve(count, name)?.pages());
    }
    pages.sort_unstable();
    pages.dedup();
    Ok(pages)
}

/// Where the pages go, given the job's output P: P itself for one page,
/// or else the folder P names without its extension, holding
/// `<folder>-NN.<ext>` with NN padded to the width of the page count, as
/// `conv split` numbers its files. Returns the destination, whether it is
/// a folder, and the file names.
pub fn destination(output: &Path, pages: &[u32], count: u32) -> (PathBuf, bool, Vec<String>) {
    if let [_] = pages {
        let name = output
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        return (output.to_path_buf(), false, vec![name]);
    }
    let folder = output.with_extension("");
    let stem = folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "page".to_string());
    let ext = output
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    let width = count.to_string().len();
    let names = pages
        .iter()
        .map(|p| format!("{stem}-{p:0width$}.{ext}"))
        .collect();
    (folder, true, names)
}

/// How big a page is drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sizing {
    Dpi(u32),
    /// `--resize`, fitting within the geometry, aspect kept. A percentage
    /// is of the page at `DEFAULT_DPI`.
    Fit(String),
}

impl Sizing {
    fn of(tuning: &Tuning) -> Sizing {
        match &tuning.resize {
            Some(g) => Sizing::Fit(g.clone()),
            None => Sizing::Dpi(tuning.dpi.unwrap_or(DEFAULT_DPI)),
        }
    }

    fn dpi(&self) -> Option<u32> {
        match self {
            Sizing::Dpi(d) => Some(*d),
            Sizing::Fit(_) => None,
        }
    }
}

/// A page's size in pixels. The width is what conv asks Typst for; the
/// height follows the page's shape, rounded as Typst rounds it.
fn pixels((w, h): (u32, u32), sizing: &Sizing) -> (u32, u32) {
    let (wpt, hpt) = (f64::from(w.max(1)) / 100.0, f64::from(h.max(1)) / 100.0);
    let height_for = |width: f64| (width * hpt / wpt).round().max(1.0);
    let num = |s: &str| s.parse::<f64>().unwrap_or(f64::MAX);
    let mut width = match sizing {
        Sizing::Dpi(d) => wpt * f64::from(*d) / 72.0,
        Sizing::Fit(g) => {
            let base = wpt * f64::from(DEFAULT_DPI) / 72.0;
            if let Some(pct) = g.strip_suffix('%') {
                base * num(pct) / 100.0
            } else {
                match g.split_once('x') {
                    Some(("", b)) => num(b) * wpt / hpt,
                    Some((a, "")) => num(a),
                    Some((a, b)) => num(a).min(num(b) * wpt / hpt),
                    None => num(g),
                }
            }
        }
    }
    .round()
    .max(1.0);
    // A height a fit rounds up past its bound comes back down a pixel.
    if let Sizing::Fit(g) = sizing {
        if let Some((_, b)) = g.split_once('x').filter(|(_, b)| !b.is_empty()) {
            while width > 1.0 && height_for(width) > num(b) {
                width -= 1.0;
            }
        }
    }
    (width as u32, height_for(width) as u32)
}

/// Refuses a flag that does nothing to a PDF's pages, naming why.
fn refuse_flags(to: Format, tuning: &Tuning) -> Result<()> {
    let pair = format!("pdf -> {}", to.ext());
    let refused: [(&str, bool, &str); 14] = [
        (
            "--upscale",
            tuning.upscale,
            "a page is drawn at the size asked for, so there is nothing to enlarge",
        ),
        (
            "--colors",
            tuning.colors.is_some(),
            "it reduces a picture's palette, and pages are drawn in full color",
        ),
        (
            "--fps",
            tuning.fps.is_some(),
            "it tunes video and GIF targets",
        ),
        ("--crf", tuning.crf.is_some(), "it tunes video targets"),
        (
            "--max-size",
            tuning.max_size.is_some(),
            "it sizes video targets",
        ),
        (
            "--strip-metadata",
            tuning.strip_metadata,
            "a page drawn from a PDF carries none of its metadata",
        ),
        (
            "--start",
            tuning.range.is_some(),
            "a PDF has no timeline; --pages picks the pages",
        ),
        ("--speed", tuning.speed.is_some(), "a PDF has no timeline"),
        (
            "--rotate",
            tuning.rotate.is_some(),
            "it turns photos and video, not PDF pages",
        ),
        (
            "--flip",
            tuning.flip.is_some(),
            "it mirrors photos and video, not PDF pages",
        ),
        (
            "--crop",
            tuning.crop.is_some(),
            "it crops photos and video; crop the pages once they are pictures",
        ),
        (
            "--audio",
            tuning.audio.is_some(),
            "it gives a video new sound",
        ),
        (
            "--subtitles",
            tuning.subtitles.is_some(),
            "it draws text onto video and GIFs",
        ),
        (
            "--quality",
            tuning.quality.is_some() && to == Format::Png,
            "png is lossless",
        ),
    ];
    for (flag, given, why) in refused {
        if given {
            // A range is any of three flags; name the one given.
            let flag = match (flag, &tuning.range) {
                ("--start", Some(r)) => r.flag(),
                _ => flag,
            };
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!("{flag} does not apply to {pair}: {why}"),
            ));
        }
    }
    if tuning.mute {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "a silent clip needs a video target; {} has no picture",
                to.ext()
            ),
        ));
    }
    Ok(())
}

/// Refuses `--dpi` and `--pages` on a pair that does not save a PDF's
/// pages.
pub(crate) fn refuse_elsewhere(from: Format, to: Format, tuning: &Tuning) -> Result<()> {
    if is_pair(from, to) {
        return Ok(());
    }
    let pair = format!("{} -> {}", from.ext(), to.ext());
    let (flag, why) = if tuning.dpi.is_some() {
        ("--dpi", "it sets the resolution a PDF's pages are drawn at")
    } else if tuning.pages.is_some() {
        (
            "--pages",
            "it picks the pages of a PDF saved as png, jpg, webp or heic; conv split takes pages \
             into PDFs",
        )
    } else {
        return Ok(());
    };
    Err(ConvError::new(
        ErrorCode::InvalidInvocation,
        format!("{flag} does not apply to {pair}: {why}"),
    ))
}

/// The flattened copy of the PDF Typst reads, inside the work folder.
const FLAT: &str = "flat.pdf";

/// The folder a plan writing `output` works in: beside it, as a two-step
/// recipe's intermediate is (`report.convkit-pages`). In a real run
/// `output` is inside the scratch folder, and so is this.
pub(crate) fn work_dir(output: &Path) -> PathBuf {
    output.with_extension("convkit-pages")
}

fn text(p: &Path) -> String {
    path_arg(p)
}

/// `name` inside the work folder `dir`, joined with `/` on every platform,
/// so a plan reads the same everywhere and the snapshot is stable, as
/// stills' image2 patterns are. Windows takes `/`, and the long-path
/// rewriter makes it `\` where it must: `winpath::to_verbatim` makes a
/// path absolute first, which turns every `/` into `\`.
pub(crate) fn inside(dir: &Path, name: &str) -> PathBuf {
    let dir = dir.to_string_lossy();
    match dir.trim_end_matches(['/', '\\']) {
        "" => PathBuf::from(name),
        dir => PathBuf::from(format!("{dir}/{name}")),
    }
}

/// `name`, an ffmpeg image2 pattern such as `png/1-%d.png`, inside `dir`,
/// whose own `%` signs are doubled so ffmpeg reads them as themselves.
/// `exec` hands this a `dir` that is already absolute, so the long-path
/// rewriter has no working directory to add whose `%` would not be.
fn pattern_text(dir: &Path, name: &str) -> String {
    let dir = PathBuf::from(path_arg(dir).replace('%', "%%"));
    inside(&dir, name).to_string_lossy().into_owned()
}

fn step(backend: Backend, argv: Vec<String>, paths: &[usize], output: PathBuf) -> PlannedStep {
    PlannedStep {
        backend,
        program: backend.exe_name().to_string(),
        argv,
        output_mode: OutputMode::Path,
        output,
        intermediate_ext: None,
        path_args: paths.to_vec(),
        prepare: Vec::new(),
        stdin: None,
    }
}

fn flatten_step(input: &Path, work: &Path) -> PlannedStep {
    let flat = inside(work, FLAT);
    let argv = vec![
        // Typst cannot open an encrypted file; an owner password's
        // restrictions do not stop drawing it.
        "--decrypt".to_string(),
        // Form fields filled in by a program that left drawing them to the
        // viewer (`NeedAppearances`) get an appearance to flatten.
        "--generate-appearances".to_string(),
        // Annotations shown on screen become part of the page; ones marked
        // hidden or print-only stay out, as a viewer shows it.
        "--flatten-annotations=screen".to_string(),
        // A repaired file is still good; `read` has already noted it.
        "--warning-exit-0".to_string(),
        text(input),
        text(&flat),
    ];
    let mut s = step(Backend::Qpdf, argv, &[4, 5], flat);
    s.intermediate_ext = Some("pdf".to_string());
    s
}

/// One Typst run: draws the pages of `layout` into `png`, as `K-1.png` and
/// on.
fn typst_step(work: &Path, chunk: usize, layout: String) -> PlannedStep {
    let layout_path = inside(work, &format!("layout-{chunk}.typ"));
    let png = inside(work, "png");
    let argv = vec![
        "compile".to_string(),
        "--root".to_string(),
        text(work),
        "--ignore-system-fonts".to_string(),
        "--diagnostic-format".to_string(),
        "short".to_string(),
        // The layout sets each page's width in points to the pixels wanted,
        // so one point is one pixel. Typst draws a PDF page from its
        // vectors at that size, as sharp as `--ppi` at the page's own size.
        "--ppi".to_string(),
        "72".to_string(),
        text(&layout_path),
        text(&inside(work, &format!("png/{chunk}-{{p}}.png"))),
    ];
    let mut s = step(Backend::Typst, argv, &[2, 8, 9], png);
    s.output_mode = OutputMode::Folder;
    s.intermediate_ext = Some("png".to_string());
    s.prepare = vec![Prepare::Write {
        path: layout_path,
        contents: layout,
    }];
    s
}

/// The layout for one Typst run: each page its own size, the width in
/// points set to the pixels wanted, on white as a viewer shows it.
fn layout(pages: &[(u32, u32)]) -> String {
    let mut s = String::from("#set page(width: auto, height: auto, margin: 0pt, fill: white)\n");
    for (i, (page, width)) in pages.iter().enumerate() {
        if i > 0 {
            s.push_str("#pagebreak()\n");
        }
        s.push_str(&format!(
            "#image(\"/{FLAT}\", page: {page}, width: {width}pt)\n"
        ));
    }
    s
}

/// ffmpeg's `-q:v` for a JPEG at `quality` on ImageMagick's 1-100 scale,
/// so `--quality` means here what it means for every other JPEG conv
/// writes. libjpeg (ImageMagick's encoder) scales the standard tables by
/// `200 - 2q` percent at 50 and above and by `5000 / q` below; ffmpeg's
/// mjpeg encoder scales the same tables by its qscale, one qscale step
/// being 8 percent. Measured against `magick -quality` on a text page and
/// a photo page at 92, 75, 50 and 30: within 1.5 dB PSNR, and up to a
/// quarter smaller. 92 is `-q:v 2`.
fn jpeg_qscale(quality: u8) -> String {
    let q = f64::from(quality.clamp(1, 100));
    let percent = if q < 50.0 {
        5000.0 / q
    } else {
        200.0 - 2.0 * q
    };
    let qscale = (percent * 12.5 / 100.0).clamp(1.0, 31.0);
    let s = format!("{qscale:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// One ffmpeg run: pages `first..first + n` of chunk `chunk`, all the same
/// size (an image2 sequence is one size), PNG to JPEG.
fn jpeg_step(
    work: &Path,
    chunk: usize,
    first: usize,
    n: Option<usize>,
    quality: u8,
) -> PlannedStep {
    let jpg = inside(work, "jpg");
    let mut argv = vec![
        // A numbered sequence, never a glob: the default would read the `?`
        // of an extended-length Windows path as one, and Windows builds of
        // ffmpeg have no glob to read it with.
        "-pattern_type".to_string(),
        "sequence".to_string(),
        "-start_number".to_string(),
        first.to_string(),
        "-i".to_string(),
        pattern_text(work, &format!("png/{chunk}-%d.png")),
    ];
    if let Some(n) = n {
        argv.extend(["-frames:v".to_string(), n.to_string()]);
    }
    argv.extend([
        "-c:v".to_string(),
        "mjpeg".to_string(),
        "-q:v".to_string(),
        jpeg_qscale(quality),
        // Without it ffmpeg stops at qscale 2.
        "-qmin".to_string(),
        "1".to_string(),
        // Full-resolution colour at 90 and above, half below, as
        // ImageMagick samples a JPEG: text in a colour stays sharp. The
        // `j` formats are full range, as JPEG is; ffmpeg calls them
        // deprecated but every version takes them.
        "-pix_fmt".to_string(),
        if quality >= 90 {
            "yuvj444p"
        } else {
            "yuvj420p"
        }
        .to_string(),
        "-start_number".to_string(),
        first.to_string(),
        "-y".to_string(),
        pattern_text(work, &format!("jpg/{chunk}-%d.jpg")),
    ]);
    let last = argv.len() - 1;
    let mut s = step(Backend::Ffmpeg, argv, &[5, last], jpg);
    s.output_mode = OutputMode::Folder;
    s
}

/// One ImageMagick run: one page, PNG to WebP or HEIC.
fn magick_step(work: &Path, chunk: usize, index: usize, to: Format, quality: u8) -> PlannedStep {
    let enc = inside(work, "enc");
    let argv = vec![
        text(&inside(work, &format!("png/{chunk}-{index}.png"))),
        "-quality".to_string(),
        quality.to_string(),
        text(&inside(work, &format!("enc/{chunk}-{index}.{}", to.ext()))),
    ];
    let mut s = step(Backend::Magick, argv, &[0, 3], enc);
    s.output_mode = OutputMode::Folder;
    s
}

/// The note a damaged file gets, as `conv split` words it.
fn repaired_note(name: &str, folder: bool) -> String {
    let check = if folder { "the pages" } else { "the page" };
    format!("{name} was damaged and has been repaired; check {check}.")
}

/// Plans saving the pages of `inputs[0]` as `to`, for a job writing
/// `output`. With no `pdf` read (a dry run with qpdf missing, or the
/// snapshot), the plan is a template: one Typst run over every page.
pub(crate) fn plan(
    to: Format,
    inputs: &[PathBuf],
    output: &Path,
    pdf: Option<&PdfPages>,
    tuning: &Tuning,
) -> Result<PagesPlan> {
    refuse_flags(to, tuning)?;
    let input = &inputs[0];
    let name = display_name(input);
    let work = work_dir(output);
    let sizing = Sizing::of(tuning);
    let quality = tuning.quality.or(default_quality(to)).unwrap_or(92);
    let mut steps = vec![flatten_step(input, &work)];
    let mut warnings = Vec::new();

    let Some(pdf) = pdf else {
        // The template: one run, every page, named as it would be.
        steps.push(typst_step(&work, 1, layout(&[])));
        match to {
            Format::Jpg => steps.push(jpeg_step(&work, 1, 1, None, quality)),
            Format::Webp | Format::Heic => steps.push(magick_step(&work, 1, 1, to, quality)),
            _ => {}
        }
        warnings.push(format!(
            "conv could not read {name} here, so this shows one Typst run for every page; a \
             real run reads it first and draws each page at its own size."
        ));
        let report = PagesReport {
            count: None,
            pages: Vec::new(),
            folder: false,
            files: Vec::new(),
            dpi: sizing.dpi(),
            needs_confirmation: false,
            warning: None,
        };
        return Ok(PagesPlan {
            plan: conversion_plan(to, inputs, output, steps, warnings, report),
            work,
            produced: Vec::new(),
            exact: false,
            repaired: None,
        });
    };

    let pages = selected(tuning.pages.as_deref(), pdf.count, &name)?;
    let (dest, folder, files) = destination(output, &pages, pdf.count);
    let mut report = PagesReport {
        count: Some(pdf.count),
        pages: pages.clone(),
        folder,
        files,
        dpi: sizing.dpi(),
        needs_confirmation: false,
        warning: None,
    };
    if pages.len() > ASK_ABOVE {
        report.needs_confirmation = true;
        report.warning = Some(format!(
            "This writes {} files into {}/, one for each page.",
            grouped(pages.len()),
            display_name(&dest)
        ));
    }
    let repaired = pdf.damaged.then(|| repaired_note(&name, folder));
    warnings.extend(repaired.clone());

    let exact = pdf.sizes.len() == pdf.count as usize;
    if !exact {
        steps.push(typst_step(&work, 1, layout(&[])));
        match to {
            Format::Jpg => steps.push(jpeg_step(&work, 1, 1, None, quality)),
            Format::Webp | Format::Heic => steps.push(magick_step(&work, 1, 1, to, quality)),
            _ => {}
        }
        warnings.push(format!(
            "conv could not read the page sizes of {name} here, so this shows one Typst run for \
             every page; a real run reads them first and draws each page at its own size."
        ));
        return Ok(PagesPlan {
            plan: conversion_plan(to, inputs, &dest, steps, warnings, report),
            work,
            produced: Vec::new(),
            exact: false,
            repaired,
        });
    }

    // Each page's pixels, refused when one is too large to draw.
    let sized: Vec<(u32, (u32, u32))> = pages
        .iter()
        .map(|&p| (p, pixels(pdf.sizes[p as usize - 1], &sizing)))
        .collect();
    if let Some(&(page, (w, h))) = sized
        .iter()
        .find(|(_, (w, h))| u64::from(*w) * u64::from(*h) > MAX_PIXELS)
    {
        return Err(too_large(&name, page, (w, h), &sizing));
    }

    // Runs of pages Typst draws at once, within `RUN_PIXELS`.
    let mut chunks: Vec<Vec<(u32, (u32, u32))>> = Vec::new();
    let mut held = 0u64;
    for &(page, dims) in &sized {
        let px = u64::from(dims.0) * u64::from(dims.1);
        match chunks.last_mut() {
            Some(c) if held + px <= RUN_PIXELS => {
                c.push((page, dims));
                held += px;
            }
            _ => {
                chunks.push(vec![(page, dims)]);
                held = px;
            }
        }
    }

    let mut produced = Vec::with_capacity(sized.len());
    let mut encode = Vec::new();
    for (k, chunk) in chunks.iter().enumerate() {
        let k = k + 1;
        let widths: Vec<(u32, u32)> = chunk.iter().map(|(p, (w, _))| (*p, *w)).collect();
        steps.push(typst_step(&work, k, layout(&widths)));
        match to {
            Format::Png => {
                for i in 1..=chunk.len() {
                    produced.push(inside(&work, &format!("png/{k}-{i}.png")));
                }
            }
            Format::Jpg => {
                // One ffmpeg run per stretch of pages of one size.
                let mut i = 0;
                while i < chunk.len() {
                    let mut j = i;
                    while j + 1 < chunk.len() && chunk[j + 1].1 == chunk[i].1 {
                        j += 1;
                    }
                    let n = j - i + 1;
                    encode.push(jpeg_step(&work, k, i + 1, Some(n), quality));
                    i = j + 1;
                }
                for i in 1..=chunk.len() {
                    produced.push(inside(&work, &format!("jpg/{k}-{i}.jpg")));
                }
            }
            _ => {
                for i in 1..=chunk.len() {
                    encode.push(magick_step(&work, k, i, to, quality));
                    produced.push(inside(&work, &format!("enc/{k}-{i}.{}", to.ext())));
                }
            }
        }
    }
    // Every page is drawn before any is encoded, so a page Typst cannot
    // draw fails the run before the slower encoding starts.
    steps.extend(encode);

    Ok(PagesPlan {
        plan: conversion_plan(to, inputs, &dest, steps, warnings, report),
        work,
        produced,
        exact: true,
        repaired,
    })
}

/// The refusal for a large run made without consent.
pub fn confirmation_error(input: &Path, report: &PagesReport) -> ConvError {
    ConvError::new(
        ErrorCode::ConfirmationRequired,
        format!(
            "saving {} pages of {} not confirmed; pass --yes to write them, or --pages to pick \
             fewer",
            grouped(report.pages.len()),
            input.display()
        ),
    )
}

/// Why writing the pages to `dest` would replace something, unless
/// `overwrite`: the one file, or any of the files a folder would get. A
/// folder that is already there is fine, and nothing else in it is
/// touched, as `conv split` writes beside what is there. A file where the
/// folder would go is refused even with `-y`.
pub fn taken(dest: &Path, report: &PagesReport, overwrite: bool) -> Option<ConvError> {
    let exists = |p: &Path| {
        ConvError::new(
            ErrorCode::OutputExists,
            format!("{} exists; pass -y to overwrite", p.display()),
        )
    };
    if !report.folder {
        if dest.is_dir() {
            return Some(ConvError::new(
                ErrorCode::OutputExists,
                format!("{} is a folder; pick another name", dest.display()),
            ));
        }
        return (dest.exists() && !overwrite).then(|| exists(dest));
    }
    if dest.exists() && !dest.is_dir() {
        return Some(ConvError::new(
            ErrorCode::OutputExists,
            format!(
                "{} is a file, and the pages go into a folder of that name; move it or pick \
                 another name",
                dest.display()
            ),
        ));
    }
    if overwrite {
        return None;
    }
    report
        .files
        .iter()
        .map(|f| dest.join(f))
        .find(|p| p.exists())
        .map(|p| exists(&p))
}

/// A missing ImageMagick says what it is for, since the user asked for
/// pictures and not for ImageMagick, and that PNG and JPEG need nothing
/// conv cannot install.
pub fn why_needed(mut e: ConvError, to: Format) -> ConvError {
    if e.code == ErrorCode::BackendMissing && e.backend == Some(Backend::Magick) {
        e.message = format!(
            "{}; ImageMagick writes the {} pages, and PNG or JPEG pages need only what conv \
             installs",
            e.message,
            to.ext().to_uppercase()
        );
    }
    e
}

/// A Typst or qpdf failure as the user should read it: the copy conv made
/// is named as their file, and a diagnostic's place in conv's layout is
/// left out.
pub(crate) fn name_the_pdf(mut e: ConvError, work: &Path, input: &Path) -> ConvError {
    let shown = display_name(input);
    let flat = inside(work, FLAT).display().to_string();
    e.message = e
        .message
        .split("; ")
        .map(|part| {
            let part = match (part.find(".typ:"), part.find("error:")) {
                (Some(at), Some(err)) if at < err => &part[err..],
                _ => part,
            };
            part.replace(&flat, &shown)
                .replace(&format!("/{FLAT}"), &shown)
                .replace(FLAT, &shown)
        })
        .collect::<Vec<_>>()
        .join("; ");
    e
}

fn too_large(name: &str, page: u32, (w, h): (u32, u32), sizing: &Sizing) -> ConvError {
    let px = u64::from(w) * u64::from(h);
    let fix = match sizing {
        Sizing::Dpi(d) => {
            let fits = (f64::from(*d) * (MAX_PIXELS as f64 / px as f64).sqrt()).floor() as u32;
            format!("try --dpi {} or lower", fits.max(MIN_DPI))
        }
        Sizing::Fit(_) => "try a smaller --resize".to_string(),
    };
    ConvError::new(
        ErrorCode::InvalidInvocation,
        format!(
            "page {page} of {name} would be {w} x {h} pixels, and conv draws at most {} million \
             pixels a page; {fix}",
            MAX_PIXELS / 1_000_000
        ),
    )
}

fn conversion_plan(
    to: Format,
    inputs: &[PathBuf],
    output: &Path,
    steps: Vec<PlannedStep>,
    warnings: Vec<String>,
    report: PagesReport,
) -> ConversionPlan {
    ConversionPlan {
        from: Format::Pdf,
        to,
        inputs: inputs.to_vec(),
        output: output.to_path_buf(),
        steps,
        warnings,
        sizing: None,
        enlarged: None,
        range: None,
        speed: None,
        rotation: None,
        built_in: false,
        pages: Some(report),
        crop: None,
        audio: None,
        subtitles: None,
    }
}

// --- reading ---------------------------------------------------------------

/// Reads the page count of `input` with qpdf, refusing a password-protected
/// file as `conv split` does.
pub fn read(qpdf: &Path, input: &Path) -> Result<PdfPages> {
    let info =
        super::read::read(qpdf, &for_backend(input, input), None).map_err(|e| locked(e, input))?;
    Ok(PdfPages {
        count: info.pages,
        sizes: Vec::new(),
        damaged: info.damaged,
    })
}

/// `p` as a backend should be handed it: in extended (`\\?\`) form on
/// Windows when `opens`, the file the backend will open through it, is past
/// the length most programs there handle, as `exec` hands every recorded
/// path of a step. `p` itself otherwise, and everywhere else.
fn for_backend(p: &Path, opens: &Path) -> PathBuf {
    let long = winpath::is_long(&opens.to_string_lossy());
    match long.then(|| winpath::to_verbatim(&p.to_string_lossy())) {
        Some(Some(v)) => PathBuf::from(v),
        _ => p.to_path_buf(),
    }
}

/// A password-protected PDF, with the way round it: nothing here can ask
/// for the password, so the PDF is unlocked first.
fn locked(e: ConvError, input: &Path) -> ConvError {
    if e.code != ErrorCode::PasswordProtected {
        return e;
    }
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    ConvError {
        remediation: Some(Remediation {
            managed: None,
            manual: Some(format!(
                "unlock it first, `conv {stem}.pdf --unlock`, then convert {stem}-unlocked.pdf"
            )),
        }),
        ..e
    }
}

/// Whether this Typst can read page sizes and draw PDF pages.
pub fn typst_draws_pages(version: &str) -> bool {
    let mut parts = version
        .trim()
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u32>().unwrap_or(0));
    match (parts.next(), parts.next()) {
        (Some(major), Some(minor)) => (major, minor) >= MIN_TYPST,
        _ => false,
    }
}

/// The refusal for a Typst too old to draw pages. conv resolves its own
/// copy before one on `PATH`, so installing it is the fix.
pub fn typst_too_old(version: &str) -> ConvError {
    ConvError {
        code: ErrorCode::ConversionFailed,
        message: format!(
            "typst {version} cannot draw PDF pages for conv, which needs typst {}.{} or newer",
            MIN_TYPST.0, MIN_TYPST.1
        ),
        backend: Some(Backend::Typst),
        remediation: Some(Remediation {
            managed: Some("conv install typst".to_string()),
            manual: None,
        }),
    }
}

/// The Typst source `read_sizes` measures pages with. The file and the page
/// count come in as `sys.inputs`, so no file name needs escaping.
const MEASURE: &str = "#let f = \"/\" + sys.inputs.pdf\n\
#for n in range(1, int(sys.inputs.count) + 1) { context { \
let s = measure(image(f, page: n)); \
[#metadata((calc.round(s.width.pt() * 100), calc.round(s.height.pt() * 100))) <convkit-page>] } }\n";

/// Reads the size of each of `count` pages of `root/name` as Typst draws
/// them, with `typst eval` on a source given on stdin, so nothing is
/// written.
pub fn read_sizes(typst: &Path, root: &Path, name: &str, count: u32) -> Result<Vec<(u32, u32)>> {
    let failed = |why: String| ConvError {
        backend: Some(Backend::Typst),
        ..ConvError::new(ErrorCode::ConversionFailed, why)
    };
    let mut child = backend_command(typst)
        .arg("eval")
        .arg("--in")
        .arg("-")
        .arg("--root")
        .arg(for_backend(root, &root.join(name)))
        .arg("--ignore-system-fonts")
        .arg("--diagnostic-format")
        .arg("short")
        .arg("--input")
        .arg(format!("pdf={name}"))
        .arg("--input")
        .arg(format!("count={count}"))
        .arg("query(<convkit-page>).map(it => it.value)")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| failed(format!("failed to run {}: {e}", typst.display())))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A Typst that exits early closes the pipe; its stderr says why.
        let _ = stdin.write_all(MEASURE.as_bytes());
    }
    let out = child
        .wait_with_output()
        .map_err(|e| failed(format!("failed to run {}: {e}", typst.display())))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let reason = stderr
            .lines()
            .map(str::trim)
            .find(|l| l.contains("error:"))
            .or_else(|| stderr.lines().map(str::trim).rfind(|l| !l.is_empty()))
            .unwrap_or("typst exited with an error")
            .to_string();
        return Err(failed(format!("typst could not read the pages: {reason}")));
    }
    parse_sizes(&out.stdout, count)
        .ok_or_else(|| failed("typst's page sizes were not what conv asked for".to_string()))
}

/// `[[61200, 79200], ...]`, one pair per page.
fn parse_sizes(json: &[u8], count: u32) -> Option<Vec<(u32, u32)>> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    let sizes: Vec<(u32, u32)> = v
        .as_array()?
        .iter()
        .map(|pair| {
            let a = pair.as_array()?;
            let n = |i: usize| {
                a.get(i)?
                    .as_f64()
                    .filter(|x| *x >= 1.0 && *x < f64::from(u32::MAX))
                    .map(|x| x as u32)
            };
            Some((n(0)?, n(1)?))
        })
        .collect::<Option<_>>()?;
    (sizes.len() == count as usize).then_some(sizes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdf(count: u32, size: (u32, u32)) -> PdfPages {
        PdfPages {
            count,
            sizes: vec![size; count as usize],
            damaged: false,
        }
    }

    const LETTER: (u32, u32) = (61200, 79200);
    const A4: (u32, u32) = (59528, 84189);

    fn tuning() -> Tuning {
        Tuning::default()
    }

    fn pages(text: &str) -> Tuning {
        Tuning {
            pages: Some(crate::pdf::range::parse_list(text).unwrap()),
            ..Tuning::default()
        }
    }

    fn planned(to: Format, p: &PdfPages, t: &Tuning) -> PagesPlan {
        plan(
            to,
            &[PathBuf::from("report.pdf")],
            Path::new("report.png").with_extension(to.ext()).as_path(),
            Some(p),
            t,
        )
        .unwrap()
    }

    fn lines(plan: &ConversionPlan) -> Vec<String> {
        plan.steps
            .iter()
            .map(|s| format!("{} {}", s.program, s.argv.join(" ")))
            .collect()
    }

    #[test]
    fn only_pdf_to_the_four_picture_formats_is_a_pair() {
        for to in TARGETS {
            assert!(is_pair(Format::Pdf, to));
        }
        assert!(!is_pair(Format::Pdf, Format::Docx));
        assert!(!is_pair(Format::Png, Format::Jpg));
        assert!(!is_pair(Format::Pdf, Format::Gif));
    }

    #[test]
    fn a_selection_is_ascending_and_each_page_once() {
        let r = |t: &str| crate::pdf::range::parse_list(t).unwrap();
        assert_eq!(selected(None, 3, "a.pdf").unwrap(), vec![1, 2, 3]);
        assert_eq!(
            selected(Some(&r("5-3,1,4,z")), 6, "a.pdf").unwrap(),
            vec![1, 3, 4, 5, 6]
        );
        let e = selected(Some(&r("9")), 3, "a.pdf").unwrap_err();
        assert_eq!(e.message, "a.pdf has 3 pages; 9 goes past the end");
        let e = selected(None, 0, "empty.pdf").unwrap_err();
        assert_eq!(e.message, "empty.pdf has no pages");
    }

    #[test]
    fn one_page_is_the_named_file_and_more_go_into_a_folder_padded_to_the_count() {
        let out = Path::new("docs/report.png");
        assert_eq!(
            destination(out, &[1], 1),
            (out.to_path_buf(), false, vec!["report.png".to_string()])
        );
        let names = |pages: &[u32], count: u32| destination(out, pages, count).2;
        let (dir, folder, _) = destination(out, &[1, 2], 2);
        assert_eq!((dir, folder), (PathBuf::from("docs/report"), true));
        assert_eq!(
            names(&(1..=9).collect::<Vec<_>>(), 9).last().unwrap(),
            "report-9.png"
        );
        let ten = names(&(1..=10).collect::<Vec<_>>(), 10);
        assert_eq!(
            (ten[0].as_str(), ten[9].as_str()),
            ("report-01.png", "report-10.png")
        );
        let hundred = names(&(1..=100).collect::<Vec<_>>(), 100);
        assert_eq!(
            (hundred[0].as_str(), hundred[99].as_str()),
            ("report-001.png", "report-100.png")
        );
        // Pages picked keep their own numbers, padded to the whole PDF.
        assert_eq!(names(&[2, 7], 12), vec!["report-02.png", "report-07.png"]);
    }

    #[test]
    fn pixel_sizes_follow_the_dpi_and_fit_a_resize() {
        let dpi = |d| Sizing::Dpi(d);
        let fit = |g: &str| Sizing::Fit(g.to_string());
        assert_eq!(pixels(LETTER, &dpi(150)), (1275, 1650));
        assert_eq!(pixels(LETTER, &dpi(300)), (2550, 3300));
        assert_eq!(pixels(LETTER, &dpi(72)), (612, 792));
        assert_eq!(pixels(A4, &dpi(150)), (1240, 1754));
        assert_eq!(pixels((79200, 61200), &dpi(150)), (1650, 1275));
        assert_eq!(pixels(LETTER, &fit("1600x")), (1600, 2071));
        assert_eq!(pixels(LETTER, &fit("1600")), (1600, 2071));
        assert_eq!(pixels(LETTER, &fit("x900")), (695, 899));
        assert_eq!(pixels(LETTER, &fit("1600x900")), (695, 899));
        assert_eq!(pixels(LETTER, &fit("1000x5000")), (1000, 1294));
        assert_eq!(pixels(LETTER, &fit("50%")), (638, 826));
        assert_eq!(pixels(LETTER, &fit("200%")), (2550, 3300));
        let (_, h) = pixels(A4, &fit("x1000"));
        assert!(h <= 1000);
    }

    #[test]
    fn jpeg_quality_maps_onto_ffmpegs_scale() {
        assert_eq!(jpeg_qscale(92), "2");
        assert_eq!(jpeg_qscale(90), "2.5");
        assert_eq!(jpeg_qscale(75), "6.25");
        assert_eq!(jpeg_qscale(50), "12.5");
        assert_eq!(jpeg_qscale(100), "1");
        assert_eq!(jpeg_qscale(1), "31");
    }

    #[test]
    fn a_letter_pdf_to_png_is_one_flatten_and_one_typst_run() {
        let p = planned(Format::Png, &pdf(12, LETTER), &tuning());
        assert_eq!(
            lines(&p.plan),
            vec![
                "qpdf --decrypt --generate-appearances --flatten-annotations=screen \
                 --warning-exit-0 report.pdf report.convkit-pages/flat.pdf",
                "typst compile --root report.convkit-pages --ignore-system-fonts \
                 --diagnostic-format short --ppi 72 report.convkit-pages/layout-1.typ \
                 report.convkit-pages/png/1-{p}.png",
            ]
        );
        let Prepare::Write { contents, .. } = &p.plan.steps[1].prepare[0] else {
            panic!("the layout is written first");
        };
        assert!(contents.starts_with(
            "#set page(width: auto, height: auto, margin: 0pt, fill: white)\n\
             #image(\"/flat.pdf\", page: 1, width: 1275pt)\n#pagebreak()\n"
        ));
        assert_eq!(contents.matches("#image").count(), 12);
        let r = p.plan.pages.as_ref().unwrap();
        assert!(r.folder);
        assert_eq!(p.plan.output, PathBuf::from("report"));
        assert_eq!(r.files[0], "report-01.png");
        assert_eq!(r.dpi, Some(150));
        assert_eq!(p.produced.len(), 12);
        assert_eq!(
            p.produced[11],
            PathBuf::from("report.convkit-pages/png/1-12.png")
        );
        assert!(p.exact);
        assert!(p.plan.warnings.is_empty());
    }

    #[test]
    fn long_documents_are_drawn_in_runs_that_fit_the_memory_budget() {
        let p = planned(Format::Png, &pdf(142, LETTER), &tuning());
        // 1275 x 1650 is 2.1 million pixels: 30 pages a run.
        let runs: Vec<&PlannedStep> = p
            .plan
            .steps
            .iter()
            .filter(|s| s.backend == Backend::Typst)
            .collect();
        assert_eq!(runs.len(), 5);
        let count = |s: &PlannedStep| match &s.prepare[0] {
            Prepare::Write { contents, .. } => contents.matches("#image").count(),
            _ => 0,
        };
        assert_eq!(
            runs.iter().map(|s| count(s)).collect::<Vec<_>>(),
            [30, 30, 30, 30, 22]
        );
        assert_eq!(
            p.produced[30],
            PathBuf::from("report.convkit-pages/png/2-1.png")
        );
        // A page larger than a run's budget is a run of its own.
        let p = planned(
            Format::Png,
            &pdf(3, LETTER),
            &Tuning {
                dpi: Some(1200),
                ..Tuning::default()
            },
        );
        assert_eq!(
            p.plan
                .steps
                .iter()
                .filter(|s| s.backend == Backend::Typst)
                .count(),
            3
        );
    }

    #[test]
    fn jpeg_pages_are_encoded_by_ffmpeg_one_run_per_stretch_of_one_size() {
        let mut p = pdf(4, LETTER);
        p.sizes[2] = (79200, 61200);
        let plan = planned(Format::Jpg, &p, &tuning());
        let ffmpeg: Vec<String> = lines(&plan.plan)
            .into_iter()
            .filter(|l| l.starts_with("ffmpeg"))
            .collect();
        assert_eq!(
            ffmpeg,
            vec![
                "ffmpeg -pattern_type sequence -start_number 1 -i report.convkit-pages/png/1-%d.png -frames:v 2 -c:v mjpeg \
                 -q:v 2 -qmin 1 -pix_fmt yuvj444p -start_number 1 -y \
                 report.convkit-pages/jpg/1-%d.jpg",
                "ffmpeg -pattern_type sequence -start_number 3 -i report.convkit-pages/png/1-%d.png -frames:v 1 -c:v mjpeg \
                 -q:v 2 -qmin 1 -pix_fmt yuvj444p -start_number 3 -y \
                 report.convkit-pages/jpg/1-%d.jpg",
                "ffmpeg -pattern_type sequence -start_number 4 -i report.convkit-pages/png/1-%d.png -frames:v 1 -c:v mjpeg \
                 -q:v 2 -qmin 1 -pix_fmt yuvj444p -start_number 4 -y \
                 report.convkit-pages/jpg/1-%d.jpg",
            ]
        );
        assert_eq!(
            plan.produced[3],
            PathBuf::from("report.convkit-pages/jpg/1-4.jpg")
        );
        let low = planned(
            Format::Jpg,
            &pdf(1, LETTER),
            &Tuning {
                quality: Some(75),
                ..Tuning::default()
            },
        );
        assert!(lines(&low.plan)[2].contains("-q:v 6.25 -qmin 1 -pix_fmt yuvj420p"));
    }

    #[test]
    fn webp_and_heic_pages_are_encoded_by_imagemagick_page_by_page() {
        let p = planned(Format::Webp, &pdf(2, LETTER), &tuning());
        let magick: Vec<String> = lines(&p.plan)
            .into_iter()
            .filter(|l| l.starts_with("magick"))
            .collect();
        assert_eq!(
            magick,
            vec![
                "magick report.convkit-pages/png/1-1.png -quality 92 report.convkit-pages/enc/1-1.webp",
                "magick report.convkit-pages/png/1-2.png -quality 92 report.convkit-pages/enc/1-2.webp",
            ]
        );
        let p = planned(Format::Heic, &pdf(1, LETTER), &tuning());
        assert!(lines(&p.plan)[2].contains("-quality 60"));
        // Every page is drawn before any is encoded.
        let p = planned(Format::Webp, &pdf(60, LETTER), &tuning());
        let typst_last = p
            .plan
            .steps
            .iter()
            .rposition(|s| s.backend == Backend::Typst)
            .unwrap();
        let magick_first = p
            .plan
            .steps
            .iter()
            .position(|s| s.backend == Backend::Magick)
            .unwrap();
        assert!(typst_last < magick_first);
    }

    #[test]
    fn a_resize_sets_each_pages_width_and_no_dpi() {
        let p = planned(
            Format::Png,
            &pdf(2, LETTER),
            &Tuning {
                resize: Some("x900".to_string()),
                ..Tuning::default()
            },
        );
        let Prepare::Write { contents, .. } = &p.plan.steps[1].prepare[0] else {
            panic!();
        };
        assert!(contents.contains("page: 2, width: 695pt"));
        assert_eq!(p.plan.pages.as_ref().unwrap().dpi, None);
    }

    #[test]
    fn a_page_too_large_to_draw_is_refused_with_a_dpi_that_fits() {
        let poster = PdfPages {
            count: 1,
            sizes: vec![(238400, 337100)],
            damaged: false,
        };
        let e = plan(
            Format::Png,
            &[PathBuf::from("poster.pdf")],
            Path::new("poster.png"),
            Some(&poster),
            &Tuning {
                dpi: Some(600),
                ..Tuning::default()
            },
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "page 1 of poster.pdf would be 19867 x 28092 pixels, and conv draws at most 200 \
             million pixels a page; try --dpi 359 or lower"
        );
    }

    #[test]
    fn picking_pages_draws_only_those() {
        let p = planned(Format::Png, &pdf(12, LETTER), &pages("2,5-6"));
        let r = p.plan.pages.as_ref().unwrap();
        assert_eq!(r.pages, vec![2, 5, 6]);
        assert_eq!(
            r.files,
            vec!["report-02.png", "report-05.png", "report-06.png"]
        );
        let Prepare::Write { contents, .. } = &p.plan.steps[1].prepare[0] else {
            panic!();
        };
        assert_eq!(contents.matches("#image").count(), 3);
        assert!(contents.contains("page: 6,"));
        // One page picked is one file.
        let p = planned(Format::Png, &pdf(12, LETTER), &pages("3"));
        let r = p.plan.pages.as_ref().unwrap();
        assert!(!r.folder);
        assert_eq!(p.plan.output, PathBuf::from("report.png"));
    }

    #[test]
    fn more_than_a_thousand_files_asks_first() {
        let p = planned(Format::Png, &pdf(1001, LETTER), &tuning());
        let r = p.plan.pages.unwrap();
        assert!(r.needs_confirmation);
        assert_eq!(
            r.warning.as_deref(),
            Some("This writes 1,001 files into report/, one for each page.")
        );
        let p = planned(Format::Png, &pdf(1000, LETTER), &tuning());
        assert!(!p.plan.pages.unwrap().needs_confirmation);
    }

    #[test]
    fn a_damaged_file_is_noted() {
        let mut p = pdf(1, LETTER);
        p.damaged = true;
        let plan = planned(Format::Png, &p, &tuning());
        assert_eq!(
            plan.plan.warnings,
            vec!["report.pdf was damaged and has been repaired; check the page."]
        );
    }

    #[test]
    fn without_sizes_the_plan_is_a_template_that_says_so() {
        let p = plan(
            Format::Jpg,
            &[PathBuf::from("report.pdf")],
            Path::new("report.jpg"),
            None,
            &tuning(),
        )
        .unwrap();
        assert!(!p.exact);
        assert_eq!(p.plan.steps.len(), 3);
        assert!(p.plan.warnings[0].starts_with("conv could not read report.pdf"));
        let counted = PdfPages {
            count: 3,
            sizes: Vec::new(),
            damaged: false,
        };
        let p = planned(Format::Png, &counted, &tuning());
        assert!(!p.exact);
        assert_eq!(p.plan.pages.as_ref().unwrap().files.len(), 3);
        assert!(p.plan.warnings[0].starts_with("conv could not read the page sizes"));
    }

    #[test]
    fn flags_with_nothing_to_do_to_a_page_are_refused_by_name() {
        let cases: Vec<(Tuning, Format, &str)> = vec![
            (
                Tuning {
                    quality: Some(80),
                    ..Tuning::default()
                },
                Format::Png,
                "--quality does not apply to pdf -> png: png is lossless",
            ),
            (
                Tuning {
                    strip_metadata: true,
                    ..Tuning::default()
                },
                Format::Jpg,
                "--strip-metadata does not apply to pdf -> jpg: a page drawn from a PDF carries \
                 none of its metadata",
            ),
            (
                Tuning {
                    upscale: true,
                    resize: Some("4000x".into()),
                    ..Tuning::default()
                },
                Format::Png,
                "--upscale does not apply to pdf -> png: a page is drawn at the size asked for, \
                 so there is nothing to enlarge",
            ),
            (
                Tuning {
                    rotate: Some(crate::rotate::Turn::Right),
                    ..Tuning::default()
                },
                Format::Webp,
                "--rotate does not apply to pdf -> webp: it turns photos and video, not PDF pages",
            ),
        ];
        for (t, to, want) in cases {
            let e = plan(to, &[PathBuf::from("a.pdf")], Path::new("a.png"), None, &t).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidInvocation);
            assert_eq!(e.message, want);
        }
        assert!(plan(
            Format::Jpg,
            &[PathBuf::from("a.pdf")],
            Path::new("a.jpg"),
            None,
            &Tuning {
                quality: Some(80),
                ..Tuning::default()
            }
        )
        .is_ok());
    }

    #[test]
    fn dpi_and_pages_are_refused_on_every_other_pair() {
        let dpi = Tuning {
            dpi: Some(300),
            ..Tuning::default()
        };
        let e = refuse_elsewhere(Format::Png, Format::Jpg, &dpi).unwrap_err();
        assert_eq!(
            e.message,
            "--dpi does not apply to png -> jpg: it sets the resolution a PDF's pages are drawn at"
        );
        let e = refuse_elsewhere(Format::Pdf, Format::Docx, &pages("1")).unwrap_err();
        assert!(e
            .message
            .starts_with("--pages does not apply to pdf -> docx"));
        assert!(refuse_elsewhere(Format::Pdf, Format::Png, &dpi).is_ok());
        assert!(refuse_elsewhere(Format::Png, Format::Jpg, &tuning()).is_ok());
    }

    #[test]
    fn a_path_ffmpeg_would_read_as_a_pattern_has_its_percent_doubled() {
        let p = plan(
            Format::Jpg,
            &[PathBuf::from("100% done.pdf")],
            Path::new("100% done.jpg"),
            Some(&pdf(2, LETTER)),
            &tuning(),
        )
        .unwrap();
        let ffmpeg = &p.plan.steps[2];
        assert_eq!(ffmpeg.argv[5], "100%% done.convkit-pages/png/1-%d.png");
        assert_eq!(p.plan.output, PathBuf::from("100% done"));
    }

    #[test]
    fn work_folder_paths_are_joined_with_a_slash_on_every_platform() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(
            inside(Path::new("out.convkit-pages"), "png/1-2.png"),
            p("out.convkit-pages/png/1-2.png")
        );
        // A Windows scratch folder keeps its own separators; only the part
        // conv adds is `/`, which Windows takes.
        assert_eq!(
            inside(
                Path::new("C:\\Docs\\.convkit-1-0\\r.convkit-pages\\"),
                "flat.pdf"
            ),
            p("C:\\Docs\\.convkit-1-0\\r.convkit-pages/flat.pdf")
        );
        assert_eq!(inside(Path::new(""), "flat.pdf"), p("flat.pdf"));
        assert_eq!(
            pattern_text(Path::new("/tmp/100% done/r.convkit-pages"), "png/1-%d.png"),
            "/tmp/100%% done/r.convkit-pages/png/1-%d.png"
        );
        assert_eq!(
            pattern_text(Path::new("-r.convkit-pages"), "jpg/1-%d.jpg"),
            "./-r.convkit-pages/jpg/1-%d.jpg"
        );
    }

    #[test]
    fn a_path_only_goes_to_a_backend_verbatim_when_it_is_long_on_windows() {
        let short = Path::new("report.pdf");
        assert_eq!(for_backend(short, short), short.to_path_buf());
        if !cfg!(windows) {
            let long = PathBuf::from("a".repeat(300));
            assert_eq!(for_backend(&long, &long), long, "no limit to work around");
        }
    }

    #[test]
    fn typst_versions_before_0_15_cannot_draw_pages() {
        assert!(typst_draws_pages("0.15.1"));
        assert!(typst_draws_pages("0.16.0"));
        assert!(typst_draws_pages("1.0.0"));
        assert!(!typst_draws_pages("0.14.2"));
        assert!(!typst_draws_pages("0.13.1"));
        assert!(!typst_draws_pages("unknown"));
        let e = typst_too_old("0.13.1");
        assert_eq!(e.code, ErrorCode::ConversionFailed);
        assert_eq!(
            e.remediation.unwrap().managed.as_deref(),
            Some("conv install typst")
        );
        assert_eq!(e.backend, Some(Backend::Typst));
    }

    #[test]
    fn page_sizes_parse_from_typsts_answer() {
        assert_eq!(
            parse_sizes(b"[[61200,79200],[79200,61200]]", 2),
            Some(vec![(61200, 79200), (79200, 61200)])
        );
        assert_eq!(parse_sizes(b"[[61200.0, 79200.0]]", 1), Some(vec![LETTER]));
        assert_eq!(parse_sizes(b"[[61200,79200]]", 2), None, "one page short");
        assert_eq!(parse_sizes(b"[[0,79200]]", 1), None);
        assert_eq!(parse_sizes(b"not json", 1), None);
    }

    #[test]
    fn a_locked_pdf_is_password_protected_with_the_way_round_it() {
        let enc = Path::new("enc.pdf");
        let e = locked(crate::pdf::read::password_needed(enc), enc);
        assert_eq!(e.code, ErrorCode::PasswordProtected);
        assert_eq!(e.message, "enc.pdf is password-protected");
        assert_eq!(
            e.remediation.unwrap().manual.as_deref(),
            Some("unlock it first, `conv enc.pdf --unlock`, then convert enc-unlocked.pdf")
        );
        let other = locked(
            ConvError::new(ErrorCode::ConversionFailed, "x.pdf is damaged"),
            Path::new("x.pdf"),
        );
        assert_eq!(other.code, ErrorCode::ConversionFailed);
    }

    #[test]
    fn grouped_puts_commas_in_thousands() {
        assert_eq!(grouped(7), "7");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(1234567), "1,234,567");
    }
}
