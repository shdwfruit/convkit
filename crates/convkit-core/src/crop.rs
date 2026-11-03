//! `--crop`: keep part of the picture as it is shown, after its orientation
//! and any `--rotate`, and before `--resize`.
//!
//! Three forms. A box, `WxH+X+Y` as ImageMagick writes it, or `WxH` for one
//! centred in the picture. An aspect ratio, `16:9`, `1:1` or `9:16`: the
//! largest box of that shape, centred. And `auto`, which finds the box
//! itself: the black bars around a video (ffmpeg's `cropdetect`, over many
//! frames), or the plain border around a picture (ImageMagick's trim box).
//!
//! Every form is resolved to a box in pixels before the plan is built, so
//! `--dry-run` shows the numbers and the notes say what was kept. A picture's
//! size is read with `-ping`; a video's comes from ffprobe; `auto` looks at
//! the pixels once before the conversion (`attach`). The box then reaches
//! argv as ffmpeg's `crop` filter, right after any turn, or as ImageMagick's
//! `-crop WxH+X+Y +repage`, right after any `-rotate`.

use std::path::Path;

use serde::Serialize;

use crate::error::{ConvError, ErrorCode, Result};
use crate::probe::MediaProbe;
use crate::procutil::backend_command;
use crate::rotate::Turn;
use crate::{Backend, Format, Kind};

/// A crop as typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spec {
    /// `WxH+X+Y`, or `WxH` centred (`at` is `None`).
    Box {
        w: u32,
        h: u32,
        at: Option<(u32, u32)>,
    },
    /// `W:H`, the largest box of that shape, centred.
    Aspect { w: u32, h: u32 },
    /// The black bars or plain border, found in the pixels.
    Auto,
}

const FORMS: &str =
    "expected WxH+X+Y (a box), WxH (a box in the middle), W:H (an aspect ratio, e.g. 16:9) or auto";

/// Parses `auto`, `16:9`, `1280x720` or `1280x720+0+140`.
pub fn parse(text: &str) -> std::result::Result<Spec, String> {
    let bad = || format!("invalid crop '{text}'; {FORMS}");
    let lower = text.trim().to_ascii_lowercase();
    if lower == "auto" {
        return Ok(Spec::Auto);
    }
    let number = |t: &str| -> Option<u32> {
        let ok = !t.is_empty() && t.len() <= 6 && t.bytes().all(|b| b.is_ascii_digit());
        ok.then(|| t.parse().ok()).flatten()
    };
    if let Some((w, h)) = lower.split_once(':') {
        let (w, h) = (number(w).ok_or_else(bad)?, number(h).ok_or_else(bad)?);
        if w == 0 || h == 0 {
            return Err(format!(
                "invalid crop '{text}'; an aspect ratio needs two sides above 0"
            ));
        }
        return Ok(Spec::Aspect { w, h });
    }
    let (size, at) = match lower.split_once('+') {
        Some((size, at)) => {
            let (x, y) = at.split_once('+').ok_or_else(bad)?;
            (
                size,
                Some((number(x).ok_or_else(bad)?, number(y).ok_or_else(bad)?)),
            )
        }
        None => (lower.as_str(), None),
    };
    let (w, h) = size.split_once('x').ok_or_else(bad)?;
    let (w, h) = (number(w).ok_or_else(bad)?, number(h).ok_or_else(bad)?);
    if w == 0 || h == 0 {
        return Err(format!(
            "invalid crop '{text}'; a box needs a width and height above 0"
        ));
    }
    Ok(Spec::Box { w, h, at })
}

impl Spec {
    /// The crop as typed back.
    pub fn text(&self) -> String {
        match self {
            Spec::Box { w, h, at: None } => format!("{w}x{h}"),
            Spec::Box {
                w,
                h,
                at: Some((x, y)),
            } => format!("{w}x{h}+{x}+{y}"),
            Spec::Aspect { w, h } => format!("{w}:{h}"),
            Spec::Auto => "auto".to_string(),
        }
    }
}

/// A box in a picture, in pixels from its top left corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Rect {
    pub w: u32,
    pub h: u32,
    pub x: u32,
    pub y: u32,
}

impl Rect {
    /// ffmpeg's filter: `crop=w:h:x:y`.
    pub(crate) fn filter(&self) -> String {
        format!("crop={}:{}:{}:{}", self.w, self.h, self.x, self.y)
    }

    /// ImageMagick's operator, then `+repage`, which forgets the offset the
    /// crop leaves on the page: without it a PNG or GIF records the box's
    /// place in the old picture, and viewers draw it there.
    pub(crate) fn magick_args(&self) -> [String; 3] {
        [
            "-crop".to_string(),
            format!("{}x{}+{}+{}", self.w, self.h, self.x, self.y),
            "+repage".to_string(),
        ]
    }

    /// The box as a note writes it: `1920x800 at 0,140`.
    fn show(&self) -> String {
        format!("{}x{} at {},{}", self.w, self.h, self.x, self.y)
    }

    /// This box in a `(w, h)` picture after `turn`, in the turned picture.
    fn turned(self, (w, h): (u32, u32), turn: Option<Turn>) -> Rect {
        match turn {
            None => self,
            // A point (x, y) goes to (h - 1 - y, x).
            Some(Turn::Right) => Rect {
                w: self.h,
                h: self.w,
                x: h - self.y - self.h,
                y: self.x,
            },
            Some(Turn::Half) => Rect {
                x: w - self.x - self.w,
                y: h - self.y - self.h,
                ..self
            },
            // A point (x, y) goes to (y, w - 1 - x).
            Some(Turn::Left) => Rect {
                w: self.h,
                h: self.w,
                x: self.y,
                y: w - self.x - self.w,
            },
        }
    }
}

/// What `auto` found in the pixels, before any turn: the box to keep, in a
/// picture of `size`, or `None` when there is no border to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    pub size: (u32, u32),
    pub rect: Option<Rect>,
}

/// What `--json` says about a crop: what was asked for, and the box kept,
/// or `null` when nothing needed cutting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CropReport {
    pub asked: String,
    #[serde(rename = "box")]
    pub rect: Option<Rect>,
}

/// A crop resolved for one source: the box, if anything is cut, the probe
/// as everything after the crop should see it, and what to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cropped {
    pub rect: Option<Rect>,
    pub probe: Option<MediaProbe>,
    pub notes: Vec<String>,
    pub report: CropReport,
}

/// The fuzz an image's trim box is found with: how far from the corner's
/// colour a pixel may be and still count as border. 3% steps over JPEG's
/// noise around a flat border, and leaves a photo's own sky alone, which
/// 5% began to cut into on a phone photo.
const TRIM_FUZZ: &str = "3%";

/// The darkest a pixel may be and still count as a black bar, out of 255:
/// ffmpeg's own default.
const BLACK_LIMIT: &str = "24";

/// Fewer frames than this read on keyframes alone, and every frame is read
/// instead: a short clip may hold one keyframe, which a fade-in leaves
/// black.
const MIN_FRAMES: usize = 10;

/// Resolves `tuning`'s crop against `probe`, the source as it is shown
/// after any turn, for a conversion from `from` to `to`. `None` without a
/// crop.
pub(crate) fn resolve(
    tuning: &crate::Tuning,
    from: Format,
    to: Format,
    probe: Option<&MediaProbe>,
    input: &Path,
) -> Result<Option<Cropped>> {
    let Some(spec) = tuning.crop else {
        return Ok(None);
    };
    let name = input.display();
    let size = probe.and_then(|p| p.display_dimensions()).ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("cannot read the picture size of {name}, which --crop needs"),
        )
    })?;
    let (pw, ph) = size;
    // libx264 and VP9 take an even width and height, and ffmpeg's crop of a
    // video's yuv420p frames lands on even sides and offsets anyway; a GIF
    // or an image takes any.
    let even = crate::sized::is_video_target(to) || from.kind() == Kind::Video;
    let floor_even = |v: u32| if even { (v & !1).max(2) } else { v };
    let floor_offset = |v: u32| if even { v & !1 } else { v };
    let mut notes = Vec::new();
    let rect = match spec {
        Spec::Box { w, h, at } => {
            let (x, y) = match at {
                Some((x, y)) => {
                    if x >= pw || y >= ph {
                        return Err(ConvError::new(
                            ErrorCode::InvalidInvocation,
                            format!(
                                "--crop {} starts outside the {pw}x{ph} picture of {name}",
                                spec.text()
                            ),
                        ));
                    }
                    (x, y)
                }
                None => (pw.saturating_sub(w) / 2, ph.saturating_sub(h) / 2),
            };
            let (x, y) = (floor_offset(x), floor_offset(y));
            let (cw, ch) = (w.min(pw - x), h.min(ph - y));
            let (cw, ch) = (floor_even(cw), floor_even(ch));
            let rect = Rect { w: cw, h: ch, x, y };
            if (cw, ch, Some((x, y))) != (w, h, at.or(Some((x, y)))) {
                let why = if w > pw - x || h > ph - y {
                    format!("it runs past the edge of the {pw}x{ph} picture")
                } else {
                    "a video is cropped on even sides and offsets".to_string()
                };
                notes.push(format!(
                    "--crop {} is cut to {}: {why}.",
                    spec.text(),
                    rect.show()
                ));
            }
            Some(rect)
        }
        Spec::Aspect { w, h } => {
            let (aw, ah) = (u64::from(w), u64::from(h));
            let (fw, fh) = (u64::from(pw), u64::from(ph));
            // The tighter side decides, as a fit within does.
            let (cw, ch) = if fw * ah >= fh * aw {
                ((fh * aw + ah / 2) / ah, fh)
            } else {
                (fw, (fw * ah + aw / 2) / aw)
            };
            let side = |v: u64, max: u32| v.clamp(1, u64::from(max)) as u32;
            let (cw, ch) = (floor_even(side(cw, pw)), floor_even(side(ch, ph)));
            if (cw, ch) == (pw, ph) {
                notes.push(format!(
                    "The {pw}x{ph} picture is already {w}:{h}; --crop left it whole."
                ));
                None
            } else {
                Some(Rect {
                    w: cw,
                    h: ch,
                    x: floor_offset((pw - cw) / 2),
                    y: floor_offset((ph - ch) / 2),
                })
            }
        }
        Spec::Auto => {
            let found = probe.and_then(|p| p.crop_found).ok_or_else(|| {
                ConvError::new(
                    ErrorCode::ConversionFailed,
                    format!("could not look for a border around {name}, which --crop auto needs"),
                )
            })?;
            let what = if from.kind() == Kind::Video {
                "black bars"
            } else {
                "a plain border"
            };
            match found.rect {
                None => {
                    notes.push(format!(
                        "Found no {} around the picture; --crop auto left it whole.",
                        what.trim_start_matches("a ")
                    ));
                    None
                }
                Some(r) => {
                    let turn = tuning.rotate;
                    let r = r.turned(found.size, turn);
                    let r = Rect {
                        w: floor_even(r.w.min(pw)),
                        h: floor_even(r.h.min(ph)),
                        x: floor_offset(r.x.min(pw - 1)),
                        y: floor_offset(r.y.min(ph - 1)),
                    };
                    notes.push(format!(
                        "Cropped {what} away: the {pw}x{ph} picture is now {}.",
                        r.show()
                    ));
                    Some(r)
                }
            }
        }
    };
    let probe = rect.zip(probe).map(|(r, p)| MediaProbe {
        width: Some(r.w),
        height: Some(r.h),
        rotation: None,
        ..p.clone()
    });
    Ok(Some(Cropped {
        rect,
        probe,
        notes,
        report: CropReport {
            asked: spec.text(),
            rect,
        },
    }))
}

/// Reads what a crop needs of the source before planning: a picture's size
/// (a video's comes from ffprobe), and for `auto` the box its pixels call
/// for. A picture's goes on a probe made for it if none ran. Shared by
/// `exec::run` and the `--dry-run` preview, so the two agree.
pub fn attach(
    resolver: &crate::Resolver,
    from: Format,
    input: &Path,
    tuning: &crate::Tuning,
    probed: &mut Option<MediaProbe>,
) -> Result<()> {
    let Some(spec) = tuning.crop else {
        return Ok(());
    };
    if !input.is_file() {
        return Err(ConvError::new(
            ErrorCode::InputNotFound,
            format!("input not found: {}", input.display()),
        ));
    }
    match from.kind() {
        Kind::Video => {
            // No probe means no video to crop; the plan says so.
            let Some(p) = probed.as_mut() else {
                return Ok(());
            };
            if spec == Spec::Auto && p.video_streams > 0 {
                let ffmpeg = resolver.resolve(Backend::Ffmpeg)?;
                p.crop_found = Some(detect_bars(&ffmpeg.path, input, p)?);
            }
        }
        // A GIF ffmpeg converts has been read by ffprobe, whose size is its
        // canvas: ImageMagick is needed only to look for its border.
        Kind::Image
            if spec != Spec::Auto
                && probed
                    .as_ref()
                    .is_some_and(|p| p.width.is_some() && p.height.is_some()) => {}
        Kind::Image => {
            let magick = resolver.resolve(Backend::Magick)?;
            let (size, rect) = if spec == Spec::Auto {
                let found = detect_border(&magick.path, from, input)?;
                (found.size, Some(found))
            } else {
                (image_size(&magick.path, from, input)?, None)
            };
            let p = probed.get_or_insert_with(MediaProbe::default);
            p.width = Some(size.0);
            p.height = Some(size.1);
            p.rotation = None;
            p.crop_found = rect;
        }
        _ => {}
    }
    Ok(())
}

/// The size a picture is shown at, read with `-ping`: turned upright as its
/// EXIF says, as every image recipe does with `-auto-orient`. A GIF's canvas,
/// which its first frame may not fill.
fn image_size(magick: &Path, from: Format, input: &Path) -> Result<(u32, u32)> {
    let mut target = input.as_os_str().to_owned();
    target.push("[0]");
    let format = if from == Format::Gif {
        "%W %H TopLeft\n"
    } else {
        "%w %h %[orientation]\n"
    };
    let out = backend_command(magick)
        .arg("-ping")
        .arg(target)
        .args(["-format", format, "info:"])
        .output()
        .map_err(|e| failed_to_run("ImageMagick", e))?;
    let text = String::from_utf8_lossy(&out.stdout);
    parse_size(text.lines().next().unwrap_or("")).ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!(
                "ImageMagick could not read the size of {}, which --crop needs",
                input.display()
            ),
        )
    })
}

/// `W H ORIENTATION`, swapped for the orientations that turn a picture on
/// its side.
fn parse_size(line: &str) -> Option<(u32, u32)> {
    let mut it = line.split_whitespace();
    let positive = |t: Option<&str>| t?.parse::<u32>().ok().filter(|&n| n > 0);
    let (w, h) = (positive(it.next())?, positive(it.next())?);
    let on_its_side = matches!(
        it.next(),
        Some("LeftTop" | "RightTop" | "RightBottom" | "LeftBottom")
    );
    Some(if on_its_side { (h, w) } else { (w, h) })
}

/// How far apart, in all, the colours of a picture's opposite corners are
/// (red, green and blue, 0 to 1 each, for both diagonals): 0 when a border
/// of one colour goes all round, or across the top and bottom.
const CORNERS: &str = "%[fx:abs(p{0,0}.r-p{w-1,h-1}.r)+abs(p{0,0}.g-p{w-1,h-1}.g)\
     +abs(p{0,0}.b-p{w-1,h-1}.b)+abs(p{w-1,0}.r-p{0,h-1}.r)+abs(p{w-1,0}.g-p{0,h-1}.g)\
     +abs(p{w-1,0}.b-p{0,h-1}.b)]";

/// Corners further apart than this are no one border's: `TRIM_FUZZ` on each
/// of the six channels.
const CORNERS_APART: f64 = 0.18;

/// The plain border around a picture: ImageMagick's trim box (`%@`) at
/// `TRIM_FUZZ`, of the picture turned upright. A GIF's frames are drawn
/// whole first, and the box is the one that holds every frame's. The trim
/// box takes each edge's own colour, so a picture blue above and red below
/// was cut to the line between them; a border has to be one colour, which
/// opposite corners that differ are not.
fn detect_border(magick: &Path, from: Format, input: &Path) -> Result<Found> {
    let mut cmd = backend_command(magick);
    if from == Format::Gif {
        cmd.arg(input).arg("-coalesce");
    } else {
        let mut first = input.as_os_str().to_owned();
        first.push("[0]");
        cmd.arg(first).arg("-auto-orient");
    }
    let format = format!("%w %h %@ {CORNERS}\n");
    let out = cmd
        .args(["-fuzz", TRIM_FUZZ, "-format", &format, "info:"])
        .output()
        .map_err(|e| failed_to_run("ImageMagick", e))?;
    let text = String::from_utf8_lossy(&out.stdout);
    parse_trim(&text).ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!(
                "ImageMagick could not look for a border around {}: {}",
                input.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        )
    })
}

/// Parses `W H BOX CORNERS` lines, one per frame, into the box that holds
/// them all. A frame with nothing but border reads `0x0+W+H` and adds
/// nothing; a box the size of the picture, or corners of different colours
/// in any frame, mean there is no border.
fn parse_trim(text: &str) -> Option<Found> {
    let mut size = None;
    let mut union: Option<(u32, u32, u32, u32)> = None;
    let mut one_colour = true;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut it = line.split_whitespace();
        let w: u32 = it.next()?.parse().ok()?;
        let h: u32 = it.next()?.parse().ok()?;
        size.get_or_insert((w, h));
        let r = parse_geometry(it.next()?)?;
        let apart: f64 = it.next()?.parse().ok()?;
        one_colour &= apart <= CORNERS_APART;
        if r.w == 0 || r.h == 0 {
            continue;
        }
        let (x1, y1, x2, y2) = (r.x, r.y, r.x + r.w, r.y + r.h);
        union = Some(match union {
            None => (x1, y1, x2, y2),
            Some((a, b, c, d)) => (a.min(x1), b.min(y1), c.max(x2), d.max(y2)),
        });
    }
    let size = size?;
    let rect = union
        .map(|(x1, y1, x2, y2)| Rect {
            w: x2.min(size.0) - x1,
            h: y2.min(size.1) - y1,
            x: x1,
            y: y1,
        })
        .filter(|r| one_colour && (r.w, r.h) != size && r.w > 0 && r.h > 0);
    Some(Found { size, rect })
}

/// `WxH+X+Y`.
fn parse_geometry(g: &str) -> Option<Rect> {
    let (size, rest) = g.split_once('+')?;
    let (x, y) = rest.split_once('+')?;
    let (w, h) = size.split_once('x')?;
    Some(Rect {
        w: w.parse().ok()?,
        h: h.parse().ok()?,
        x: x.parse().ok()?,
        y: y.parse().ok()?,
    })
}

/// The black bars around a video: ffmpeg's `cropdetect` over its frames,
/// keeping the box that holds every frame's picture, so a dark scene or a
/// fade from black does not narrow it. Keyframes alone first, which is
/// quick on a long file; a clip with too few of them is read whole.
fn detect_bars(ffmpeg: &Path, input: &Path, probe: &MediaProbe) -> Result<Found> {
    let size = probe.display_dimensions().ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!(
                "cannot read the picture size of {}, which --crop needs",
                input.display()
            ),
        )
    })?;
    let run = |keyframes: bool| -> Result<(usize, Option<Rect>)> {
        let mut cmd = backend_command(ffmpeg);
        cmd.args(["-hide_banner", "-nostats"]);
        if keyframes {
            cmd.args(["-skip_frame", "nokey"]);
        }
        let filter = format!("cropdetect=limit={BLACK_LIMIT}:round=2:reset=0:skip=0");
        let out = cmd
            .arg("-i")
            .arg(input)
            .args([
                "-map", "0:v:0", "-vf", &filter, "-an", "-sn", "-f", "null", "-",
            ])
            .output()
            .map_err(|e| failed_to_run("ffmpeg", e))?;
        if !out.status.success() {
            return Err(ConvError::new(
                ErrorCode::ConversionFailed,
                format!(
                    "ffmpeg could not look for black bars in {}: {}",
                    input.display(),
                    last_line(&out.stderr)
                ),
            ));
        }
        Ok(parse_cropdetect(&String::from_utf8_lossy(&out.stderr)))
    };
    let (frames, mut rect) = run(true)?;
    if frames < MIN_FRAMES {
        rect = run(false)?.1;
    }
    // A box the size of the picture is no border; one with nothing in it
    // (a black video) is none that can be cut.
    let rect = rect.filter(|r| (r.w, r.h) != size && r.w >= 2 && r.h >= 2);
    let rect = rect.filter(|r| r.x + r.w <= size.0 && r.y + r.h <= size.1);
    Ok(Found { size, rect })
}

/// The number of frames `cropdetect` reported on, and the last box it gave,
/// which with `reset=0` holds every frame before it.
fn parse_cropdetect(log: &str) -> (usize, Option<Rect>) {
    let mut frames = 0;
    let mut last = None;
    for line in log.lines().filter(|l| l.contains("cropdetect")) {
        let Some(at) = line.rfind("crop=") else {
            continue;
        };
        let mut it = line[at + 5..].trim().split(':');
        let mut next = || it.next().and_then(|v| v.trim().parse::<i64>().ok());
        let (Some(w), Some(h), Some(x), Some(y)) = (next(), next(), next(), next()) else {
            continue;
        };
        frames += 1;
        last = (w > 0 && h > 0 && x >= 0 && y >= 0).then_some(Rect {
            w: w as u32,
            h: h as u32,
            x: x as u32,
            y: y as u32,
        });
    }
    (frames, last)
}

fn last_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn failed_to_run(what: &str, e: std::io::Error) -> ConvError {
    ConvError::new(
        ErrorCode::ConversionFailed,
        format!("failed to run {what}: {e}"),
    )
}

/// The note on a video re-encoded only to crop it.
pub(crate) const VIDEO_NOTE: &str = "The video is re-encoded to crop it.";

/// The note on a lossy image cropped into its own format, or written
/// again uncropped where there was nothing to cut.
pub(crate) fn image_note(to: Format, tuning: &crate::Tuning, cut: bool) -> String {
    let why = if cut { " to crop it" } else { " all the same" };
    format!(
        "The {} is re-encoded at quality {}{why}.",
        to.ext(),
        crate::metadata::quality_for(to, tuning)
    )
}

/// Whether a file of `f` can be cropped into its own format; `Err` says
/// what to do instead. The formats a turn keeps, less the ones whose size
/// conv cannot read the way it draws them.
pub fn in_place(f: Format) -> std::result::Result<(), String> {
    match f {
        Format::Svg | Format::Ico => Err(format!(
            "--crop does not apply to {0}: conv draws a {0} at its own size; \
             convert it to png first, then crop that",
            f.ext()
        )),
        _ => match crate::rotate::in_place(f) {
            Ok(()) => Ok(()),
            Err(_) => match crate::rotate::target_for(f) {
                Some(to) => Err(format!(
                    "conv cannot keep {0} files as {0}; add --to {1} to crop them",
                    f.ext(),
                    to.ext()
                )),
                None => Err(no_picture(f, f)),
            },
        },
    }
}

/// The format a file conv cannot crop into its own is cropped into
/// instead.
pub fn target_for(f: Format) -> Option<Format> {
    match f {
        Format::Svg | Format::Ico => None,
        _ => crate::rotate::target_for(f),
    }
}

fn no_picture(from: Format, to: Format) -> String {
    format!(
        "--crop does not apply to {} -> {}: it crops images, video and GIFs",
        from.ext(),
        to.ext()
    )
}

/// The refusal of `--crop` on a pair whose recipe cannot crop anything.
pub(crate) fn not_applicable(from: Format, to: Format) -> ConvError {
    ConvError::new(ErrorCode::InvalidInvocation, no_picture(from, to))
}

/// The refusals that hold whatever the recipe: a source whose size conv
/// cannot read the way it draws it, an icon target, whose sizes are fixed,
/// and a merge, whose pictures each have their own size.
pub(crate) fn refuse(
    from: Format,
    to: Format,
    inputs: usize,
    tuning: &crate::Tuning,
) -> Result<()> {
    if tuning.crop.is_none() {
        return Ok(());
    }
    let invalid = |why: String| Err(ConvError::new(ErrorCode::InvalidInvocation, why));
    if matches!(from, Format::Svg | Format::Ico) {
        return invalid(format!(
            "--crop does not apply to {0}: conv draws a {0} at its own size; \
             convert it to png first, then crop that",
            from.ext()
        ));
    }
    if to == Format::Ico {
        return invalid(format!(
            "--crop does not apply to {} -> ico: an icon holds fixed sizes, 16 to 256 px; \
             crop the picture first",
            from.ext()
        ));
    }
    if inputs > 1 {
        return invalid(
            "--crop crops one picture, and a merge takes several; crop them first".to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(w: u32, h: u32) -> MediaProbe {
        MediaProbe {
            width: Some(w),
            height: Some(h),
            ..MediaProbe::default()
        }
    }

    fn crop(spec: &str, to: Format, p: &MediaProbe) -> Cropped {
        let t = crate::Tuning {
            crop: Some(parse(spec).unwrap()),
            ..crate::Tuning::default()
        };
        let from = if crate::sized::is_video_target(to) {
            Format::Mp4
        } else {
            Format::Png
        };
        resolve(&t, from, to, Some(p), Path::new("in"))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn every_form_parses() {
        assert_eq!(parse("auto"), Ok(Spec::Auto));
        assert_eq!(parse("AUTO"), Ok(Spec::Auto));
        assert_eq!(parse("16:9"), Ok(Spec::Aspect { w: 16, h: 9 }));
        assert_eq!(
            parse("1280x720"),
            Ok(Spec::Box {
                w: 1280,
                h: 720,
                at: None
            })
        );
        assert_eq!(
            parse("1280x720+0+140"),
            Ok(Spec::Box {
                w: 1280,
                h: 720,
                at: Some((0, 140))
            })
        );
        for spec in ["auto", "16:9", "1280x720", "1280x720+0+140"] {
            assert_eq!(parse(spec).unwrap().text(), spec);
        }
    }

    #[test]
    fn a_crop_that_is_no_box_is_refused() {
        for text in [
            "",
            "16",
            "16:",
            ":9",
            "x720",
            "1280x",
            "1280x720+5",
            "1280x720-5-5",
            "a:b",
        ] {
            let e = parse(text).unwrap_err();
            assert!(e.contains(FORMS), "{text}: {e}");
        }
        assert!(parse("0:9").unwrap_err().contains("above 0"));
        assert!(parse("0x720").unwrap_err().contains("above 0"));
    }

    #[test]
    fn an_aspect_ratio_takes_the_largest_centred_box_of_that_shape() {
        let wide = probe(1920, 1080);
        assert_eq!(
            crop("1:1", Format::Mp4, &wide).rect,
            Some(Rect {
                w: 1080,
                h: 1080,
                x: 420,
                y: 0
            })
        );
        assert_eq!(
            crop("9:16", Format::Mp4, &wide).rect,
            Some(Rect {
                w: 608,
                h: 1080,
                x: 656,
                y: 0
            })
        );
        // A picture already that shape is left whole, and the note says so.
        let same = crop("16:9", Format::Mp4, &wide);
        assert_eq!(same.rect, None);
        assert!(same.notes[0].contains("already 16:9"), "{:?}", same.notes);
        // A GIF or an image takes an odd side; a video does not.
        let odd = probe(1001, 1000);
        assert_eq!(crop("1:1", Format::Png, &odd).rect.unwrap().w, 1000);
        assert_eq!(
            crop("4:3", Format::Mp4, &probe(640, 360)).rect,
            Some(Rect {
                w: 480,
                h: 360,
                x: 80,
                y: 0
            })
        );
    }

    #[test]
    fn a_box_is_cut_at_the_picture_edge_and_kept_even_for_video() {
        let p = probe(640, 360);
        let c = crop("400x300+300+100", Format::Png, &p);
        assert_eq!(
            c.rect,
            Some(Rect {
                w: 340,
                h: 260,
                x: 300,
                y: 100
            })
        );
        assert!(c.notes[0].contains("runs past the edge"), "{:?}", c.notes);
        let c = crop("201x101+1+1", Format::Mp4, &p);
        assert_eq!(
            c.rect,
            Some(Rect {
                w: 200,
                h: 100,
                x: 0,
                y: 0
            })
        );
        assert!(c.notes[0].contains("even sides"), "{:?}", c.notes);
        // An even box at even offsets is kept as asked.
        assert!(crop("200x100+2+4", Format::Mp4, &p).notes.is_empty());
        let c = crop("200x100", Format::Jpg, &p);
        assert_eq!(
            c.rect,
            Some(Rect {
                w: 200,
                h: 100,
                x: 220,
                y: 130
            })
        );
        assert!(c.notes.is_empty());
        // What comes after the crop sees the box's size.
        assert_eq!(c.probe.unwrap().display_dimensions(), Some((200, 100)));
    }

    #[test]
    fn a_box_that_starts_outside_the_picture_is_refused() {
        let t = crate::Tuning {
            crop: Some(parse("10x10+640+0").unwrap()),
            ..crate::Tuning::default()
        };
        let e = resolve(
            &t,
            Format::Png,
            Format::Png,
            Some(&probe(640, 360)),
            Path::new("a.png"),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.message.contains("starts outside the 640x360"), "{e}");
    }

    /// A box found in the picture as stored is turned with it.
    #[test]
    fn a_found_box_turns_with_the_picture() {
        let r = Rect {
            w: 100,
            h: 50,
            x: 10,
            y: 20,
        };
        let size = (400, 300);
        assert_eq!(r.turned(size, None), r);
        assert_eq!(
            r.turned(size, Some(Turn::Right)),
            Rect {
                w: 50,
                h: 100,
                x: 230,
                y: 10
            }
        );
        assert_eq!(
            r.turned(size, Some(Turn::Left)),
            Rect {
                w: 50,
                h: 100,
                x: 20,
                y: 290
            }
        );
        assert_eq!(
            r.turned(size, Some(Turn::Half)),
            Rect {
                w: 100,
                h: 50,
                x: 290,
                y: 230
            }
        );
    }

    #[test]
    fn auto_uses_what_was_found_or_leaves_the_picture_whole() {
        let mut p = probe(640, 480);
        p.video_streams = 1;
        p.crop_found = Some(Found {
            size: (640, 480),
            rect: Some(Rect {
                w: 640,
                h: 360,
                x: 0,
                y: 60,
            }),
        });
        let c = crop("auto", Format::Mp4, &p);
        assert_eq!(c.rect.unwrap().h, 360);
        assert!(
            c.notes[0].starts_with("Cropped black bars away"),
            "{:?}",
            c.notes
        );
        p.crop_found = Some(Found {
            size: (640, 480),
            rect: None,
        });
        let c = crop("auto", Format::Mp4, &p);
        assert_eq!(c.rect, None);
        assert!(c.notes[0].contains("no black bars"), "{:?}", c.notes);
    }

    #[test]
    fn a_trim_box_holds_every_frame_and_a_whole_picture_is_no_border() {
        let f = parse_trim("200 100 123x66+29+19 0.02\n").unwrap();
        assert_eq!(f.size, (200, 100));
        assert_eq!(
            f.rect,
            Some(Rect {
                w: 123,
                h: 66,
                x: 29,
                y: 19
            })
        );
        let f = parse_trim("50 50 10x10+0+0 0\n50 50 0x0+50+50 0\n50 50 10x10+30+30 0\n").unwrap();
        assert_eq!(
            f.rect,
            Some(Rect {
                w: 40,
                h: 40,
                x: 0,
                y: 0
            })
        );
        assert_eq!(parse_trim("70 46 70x46+0+0 1.2\n").unwrap().rect, None);
        assert_eq!(parse_trim("50 50 0x0+50+50 0\n").unwrap().rect, None);
        // Blue above and red below is no border, though each edge is plain.
        assert_eq!(parse_trim("20 40 20x2+0+19 3.98\n").unwrap().rect, None);
        assert_eq!(parse_trim(""), None);
    }

    #[test]
    fn cropdetect_gives_its_last_box_and_counts_its_frames() {
        let log = "[Parsed_cropdetect_0 @ 0x1] x1:0 x2:639 y1:60 y2:419 w:640 h:360 x:0 y:60 \
                   pts:0 t:0.000000 limit:24.000000 crop=640:360:0:60\n\
                   [Parsed_cropdetect_0 @ 0x1] x1:0 x2:639 y1:58 y2:421 w:640 h:364 x:0 y:58 \
                   pts:1 t:0.033 limit:24.000000 crop=640:364:0:58\n\
                   frame=2 fps=0.0\n";
        assert_eq!(
            parse_cropdetect(log),
            (
                2,
                Some(Rect {
                    w: 640,
                    h: 364,
                    x: 0,
                    y: 58
                })
            )
        );
        assert_eq!(parse_cropdetect("nothing here"), (0, None));
        // A frame with no picture at all reads as a box that is no box.
        let black = "[Parsed_cropdetect_0 @ 0x1] x1:639 x2:0 crop=-624:-464:632:472\n";
        assert_eq!(parse_cropdetect(black), (1, None));
    }

    #[test]
    fn a_picture_on_its_side_reads_upright() {
        assert_eq!(parse_size("40 20 TopLeft"), Some((40, 20)));
        assert_eq!(parse_size("40 20 RightTop"), Some((20, 40)));
        assert_eq!(parse_size("40 20 Undefined"), Some((40, 20)));
        assert_eq!(parse_size("0 20 TopLeft"), None);
    }

    #[test]
    fn crop_keeps_the_formats_a_turn_keeps_except_svg_and_ico() {
        assert!(in_place(Format::Jpg).is_ok());
        assert!(in_place(Format::Gif).is_ok());
        assert!(in_place(Format::Mp4).is_ok());
        assert!(in_place(Format::Svg).unwrap_err().contains("png first"));
        assert_eq!(
            in_place(Format::Avi).unwrap_err(),
            "conv cannot keep avi files as avi; add --to mp4 to crop them"
        );
        assert_eq!(
            in_place(Format::Mp3).unwrap_err(),
            "--crop does not apply to mp3 -> mp3: it crops images, video and GIFs"
        );
    }
}
