//! Stills from a video or a GIF: `conv clip.mp4 .png --start 0:12` saves the
//! frame at 0:12 as one picture, and `conv clip.mp4 .jpg --every 2s` saves a
//! numbered series into a folder named after the clip.
//!
//! What to save is decided here, against the probe, as a `Pick` the ffmpeg
//! step renders (`Arg::StillsSeek`, `Arg::StillsPick`). The recipes are in
//! `registry.rs`. Pure: no process, no filesystem, except `occupied`, which
//! lists a folder.
//!
//! With no time and no `--every`, a video gives ten stills spread evenly
//! across it (or across the range): one frame from the start says nothing
//! about the rest, and a frame a second over a two-hour film is 7,200
//! files. A GIF gives every frame, which is what splitting a GIF means. A
//! series past `ASK_ABOVE` files is asked about first, as a large
//! `--upscale` is.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::probe::MediaProbe;
use crate::recipe::Tuning;
use crate::trim::{self, Time};
use crate::video::{ResolvedVideo, Target};
use crate::{registry, ConvError, ErrorCode, Format};

/// How many stills a video gives when no time and no `--every` say which.
pub const SPREAD: u64 = 10;

/// A series expected to write more files than this asks first.
pub const ASK_ABOVE: u64 = 1000;

/// `--every`: a still every so often, or every frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Every {
    Frame,
    Interval(Time),
}

impl Every {
    /// As typed, for notes and `--json`.
    pub fn text(&self) -> &str {
        match self {
            Every::Frame => "frame",
            Every::Interval(t) => &t.text,
        }
    }
}

/// Parses `--every`: `frame`, or a length in `--duration`'s forms (`2s`,
/// `0.5`, `1:00`).
pub fn parse_every(text: &str) -> Result<Every, String> {
    if matches!(text.to_ascii_lowercase().as_str(), "frame" | "frames") {
        return Ok(Every::Frame);
    }
    trim::parse_duration(text)
        .map(Every::Interval)
        .map_err(|_| {
            format!(
                "invalid --every '{text}'; expected a length (2s, 0.5, 1:00) or `frame` for \
                 every frame"
            )
        })
}

/// Whether `from -> to` saves stills: a video or a GIF into a picture.
pub fn is_pair(from: Format, to: Format) -> bool {
    registry::is_stills_pair(from, to)
}

/// Whether the flags ask for one still rather than a series: `--start` with
/// no end, no length and no `--every`. Decided from the flags alone, so the
/// output can be named before anything is probed.
pub fn is_single(tuning: &Tuning) -> bool {
    tuning.every.is_none()
        && tuning
            .range
            .as_ref()
            .is_some_and(|r| r.start.is_some() && r.end.is_none())
}

/// Whether this conversion writes a folder of stills.
pub fn is_series(from: Format, to: Format, tuning: &Tuning) -> bool {
    is_pair(from, to) && !is_single(tuning)
}

/// What the ffmpeg step saves, resolved against the source. Carried on
/// `ResolvedVideo::stills`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    /// Input `-ss`, in milliseconds; 0 renders none.
    pub seek_ms: u64,
    /// Input `-t`, when the stills stop before the end.
    pub len_ms: Option<u64>,
    /// The `fps=` value that picks one frame per tick, as a reduced
    /// fraction (`1/2`, `10/93`).
    pub rate: Option<String>,
    /// `-frames:v`: exactly this many, where the count is known.
    pub frames: Option<u64>,
    /// Every frame as it is: `-fps_mode passthrough`, so a GIF's uneven
    /// frames are neither doubled nor dropped to fit a rate.
    pub passthrough: bool,
    /// Map an HDR source to SDR first, as the GIF recipe does: a PNG has no
    /// way to say its values are PQ or HLG, so they show grey and washed
    /// out.
    pub tonemap: bool,
    /// The series' file names before the number: the folder's name.
    pub name: String,
    /// Digits in the number, at least four, so the files sort in order.
    pub digits: usize,
}

impl Pick {
    /// The input options: `-ss` and `-t`, before `-i`. An input `-ss` seeks
    /// and still lands on the exact frame (see `trim::Cut::input_args`).
    pub(crate) fn input_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self.seek_ms > 0 {
            args.extend(["-ss".to_string(), trim::seconds(self.seek_ms)]);
        }
        if let Some(len) = self.len_ms {
            args.extend(["-t".to_string(), trim::seconds(len)]);
        }
        args
    }

    /// The output options: the filter chain, then how many frames and how.
    /// `fps=` comes first in the chain so only the frames kept are turned
    /// and scaled; the tonemap before it, as it must see the source signal.
    pub(crate) fn output_args(&self, resolved: &ResolvedVideo) -> Vec<String> {
        let mut chain: Vec<String> = Vec::new();
        if self.tonemap {
            chain.push(TONEMAP.to_string());
        }
        if let Some(rate) = &self.rate {
            chain.push(format!("fps={rate}"));
        }
        if let Some(turn) = resolved.turn {
            chain.push(turn.filter().to_string());
        }
        if let Some(scale) = &resolved.scale {
            chain.push(scale.clone());
        }
        let mut args = Vec::new();
        if !chain.is_empty() {
            args.extend(["-vf".to_string(), chain.join(",")]);
        }
        if let Some(n) = self.frames {
            args.extend(["-frames:v".to_string(), n.to_string()]);
        }
        if self.passthrough {
            args.extend(["-fps_mode".to_string(), "passthrough".to_string()]);
        }
        args
    }

    /// The image2 pattern a series is written to: `DIR/NAME-%04d.EXT`.
    /// ffmpeg reads every `%` in the path as a pattern, so one in a folder
    /// or file name is doubled. Joined with `/` on every platform: Windows
    /// takes it, and the long-path rewriter makes it `\` where it must.
    pub(crate) fn pattern(&self, dir: &Path, ext: &str) -> String {
        let dir = dir.to_string_lossy().replace('%', "%%");
        let name = self.name.replace('%', "%%");
        format!("{}/{name}-%0{}d.{ext}", folder_text(&dir), self.digits)
    }

    /// The template a plan with no probe renders: every frame, named after
    /// the folder.
    pub(crate) fn template(name: String) -> Pick {
        Pick {
            seek_ms: 0,
            len_ms: None,
            rate: None,
            frames: None,
            passthrough: true,
            tonemap: false,
            name,
            digits: 4,
        }
    }
}

/// A folder as the start of a path inside it: no trailing separator, and
/// `.` for none.
pub(crate) fn folder_text(dir: &str) -> &str {
    let dir = dir.trim_end_matches(['/', '\\']);
    if dir.is_empty() {
        "."
    } else {
        dir
    }
}

/// `TO_GIF_TONEMAP_CHAIN`'s HDR->SDR mapping, without its trailing comma.
const TONEMAP: &str = "scale=out_color_matrix=bt709:out_primaries=bt709:out_transfer=bt709:\
                       intent=perceptual,format=yuv420p";

const HDR_NOTE: &str = "HDR source tonemapped to SDR; colors are approximated, not exact.";

/// What `--json` says about the stills, and what the run asks about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StillsReport {
    /// A folder of stills rather than one.
    pub series: bool,
    /// How they were picked: `one`, `spread`, `every` or `frame`.
    pub pick: &'static str,
    /// `--every` as typed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub every: Option<String>,
    /// The single still's time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<u64>,
    /// The part of the source the series is taken from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<u64>,
    /// How many files the plan expects; absent when the source's length or
    /// frame rate could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<u64>,
    /// How many were written. Set by the executor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub written: Option<u64>,
    /// More than `ASK_ABOVE` files: the run asks first.
    pub needs_confirmation: bool,
    /// What the question is about, as printed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// `1,234`.
pub fn grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The refusal for a large series run without consent.
pub fn confirmation_error(input: &Path, report: &StillsReport) -> ConvError {
    let many = report
        .expected
        .map_or_else(|| "many".to_string(), |n| format!("about {}", grouped(n)));
    ConvError::new(
        ErrorCode::ConfirmationRequired,
        format!(
            "saving {many} stills from {} not confirmed; pass --yes to write them, or a \
             longer --every to write fewer",
            input.display()
        ),
    )
}

/// The first file in `folder` a series named `name` in `ext` would replace,
/// or that an earlier series of the same name left: `NAME-<digits>.EXT`.
/// `None` when the folder is missing or holds none.
pub fn occupied(folder: &Path, ext: &str) -> Option<PathBuf> {
    series_files(folder, ext).into_iter().next()
}

/// Every `NAME-<digits>.EXT` in `folder`, NAME being the folder's own name,
/// in name order.
pub(crate) fn series_files(folder: &Path, ext: &str) -> Vec<PathBuf> {
    let Some(name) = folder.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let prefix = format!("{}-", name.to_lowercase());
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let (Some(stem), Some(e)) = (p.file_stem(), p.extension()) else {
                return false;
            };
            let stem = stem.to_string_lossy().to_lowercase();
            e.to_string_lossy().eq_ignore_ascii_case(ext)
                && stem
                    .strip_prefix(&prefix)
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    found.sort();
    found
}

/// The refusal for a series whose folder already holds one.
pub fn exists_error(folder: &Path, first: &Path) -> ConvError {
    ConvError::new(
        ErrorCode::OutputExists,
        format!(
            "{} already holds stills ({}); pass -y to replace them",
            folder.display(),
            first.file_name().unwrap_or_default().to_string_lossy()
        ),
    )
}

/// The flags stills take, for `conv capabilities`.
pub fn flags(to: Format) -> Vec<&'static str> {
    let mut f = vec!["--every", "--start", "--end", "--duration"];
    f.extend(["--resize", "--upscale", "--rotate", "--flip"]);
    if to != Format::Png {
        f.push("--quality");
    }
    f
}

/// Refuses a flag stills do not take, naming what to use instead.
fn refuse_flags(from: Format, to: Format, tuning: &Tuning) -> crate::Result<()> {
    let pair = format!("{} -> {}", from.ext(), to.ext());
    let refused: [(&str, bool, &str); 12] = [
        (
            "--fps",
            tuning.fps.is_some(),
            "use --every to say how often",
        ),
        ("--crf", tuning.crf.is_some(), "it tunes video targets"),
        (
            "--colors",
            tuning.colors.is_some(),
            "it tunes raster image conversions",
        ),
        (
            "--max-size",
            tuning.max_size.is_some(),
            "it sizes video targets",
        ),
        ("--speed", tuning.speed.is_some(), "a still has no speed"),
        ("--mute", tuning.mute, "a still has no sound"),
        (
            "--denoise",
            tuning.denoise.is_some(),
            "a still has no sound",
        ),
        ("--audio", tuning.audio.is_some(), "a still has no sound"),
        (
            "--crop",
            tuning.crop.is_some(),
            "crop the stills once they are saved",
        ),
        (
            "--subtitles",
            tuning.subtitles.is_some(),
            "burn them into a video first, then save its stills",
        ),
        (
            "--strip-metadata",
            tuning.strip_metadata,
            "a still saved from a video carries none of its metadata",
        ),
        (
            "--quality",
            tuning.quality.is_some() && to == Format::Png,
            "png is lossless",
        ),
    ];
    for (flag, given, why) in refused {
        if given {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!("{flag} does not apply to {pair} stills: {why}"),
            ));
        }
    }
    Ok(())
}

/// Plans stills: one, or a series into `output` as a folder.
pub(crate) fn plan(
    from: Format,
    to: Format,
    inputs: &[PathBuf],
    output: &Path,
    probe: Option<&MediaProbe>,
    tuning: &Tuning,
) -> crate::Result<crate::ConversionPlan> {
    refuse_flags(from, to, tuning)?;
    let input = &inputs[0];
    if probe.is_some_and(|p| p.video_streams == 0 && p.video_codec.is_none()) {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "{} has no video stream to save stills from",
                input.display()
            ),
        ));
    }
    let single = is_single(tuning);
    let name = output
        .file_stem()
        .filter(|_| single)
        .or_else(|| output.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut notes = Vec::new();
    let (pick, report) = match probe {
        Some(p) => resolve(tuning, p, input, name, &mut notes)?,
        // No probe: the template, as the snapshot shows it. A real run
        // always has one (`registry::requires_probe`).
        None if single => (
            Pick {
                frames: Some(1),
                passthrough: false,
                ..Pick::template(name)
            },
            report(false, "one", tuning, Some(1)),
        ),
        None => (Pick::template(name), report(true, "frame", tuning, None)),
    };
    if pick.tonemap {
        notes.push(HDR_NOTE.to_string());
    }

    // Size and turn as an image target would: `--resize` fits within and
    // never enlarges without `--upscale`, against the picture as shown.
    let change = tuning.change();
    let turned = change
        .zip(probe)
        .map(|(c, p)| crate::rotate::turned_probe(c, p));
    let shown = turned.as_ref().or(probe);
    let mut resolved = crate::video::resolve(tuning, shown, image_target(to));
    resolved.turn = change;
    notes.append(&mut resolved.notes);
    resolved.stills = Some(pick);

    let recipe = registry::stills_recipe(to, !single).expect("every stills target has a recipe");
    let steps = crate::plan::render_steps(&recipe, inputs, output, tuning, &resolved, &[]);
    Ok(crate::ConversionPlan {
        from,
        to,
        inputs: inputs.to_vec(),
        output: output.to_path_buf(),
        steps,
        warnings: notes,
        sizing: None,
        enlarged: resolved.enlarged.clone(),
        range: None,
        speed: None,
        denoise: None,
        rotation: None,
        built_in: false,
        pages: None,
        crop: None,
        audio: None,
        subtitles: None,
        stills: Some(report),
    })
}

/// The `--upscale` estimate's figures for one still in `to`.
fn image_target(to: Format) -> Target {
    let bytes_per_pixel = match to {
        Format::Jpg => Some((0.05, 0.3)),
        Format::Webp => Some((0.02, 0.15)),
        Format::Heic => Some((0.003, 0.06)),
        Format::Png => Some((0.3, 2.0)),
        _ => None,
    };
    Target::Image { bytes_per_pixel }
}

fn report(
    series: bool,
    pick: &'static str,
    tuning: &Tuning,
    expected: Option<u64>,
) -> StillsReport {
    StillsReport {
        series,
        pick,
        every: tuning.every.as_ref().map(|e| e.text().to_string()),
        at_ms: None,
        start_ms: None,
        end_ms: None,
        expected,
        written: None,
        needs_confirmation: false,
        warning: None,
    }
}

/// `n/d` reduced, and a whole number alone: `1000/2000` is `1/2`, and
/// `2000/1000` is `2`.
fn fraction(n: u64, d: u64) -> String {
    let g = gcd(n, d).max(1);
    match (n / g, d / g) {
        (n, 1) => n.to_string(),
        (n, d) => format!("{n}/{d}"),
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// A frame's length in whole milliseconds, at least 1.
fn frame_ms((n, d): (u32, u32)) -> u64 {
    (1000 * u64::from(d) / u64::from(n)).max(1)
}

/// How many frames `len_ms` of the source holds, at its rate.
fn frames_in(len_ms: u64, rate: Option<(u32, u32)>) -> Option<u64> {
    let (n, d) = rate?;
    Some((len_ms * u64::from(n)).div_ceil(1000 * u64::from(d)).max(1))
}

fn resolve(
    tuning: &Tuning,
    probe: &MediaProbe,
    input: &Path,
    name: String,
    notes: &mut Vec<String>,
) -> crate::Result<(Pick, StillsReport)> {
    let base = Pick {
        tonemap: probe.is_hdr(),
        ..Pick::template(name)
    };
    let length = probe.duration_ms.filter(|&d| d > 0);

    if is_single(tuning) {
        let t = tuning
            .range
            .as_ref()
            .and_then(|r| r.start.as_ref())
            .expect("a single still has a --start");
        let at = match (length, t.from_end) {
            (Some(len), true) if t.ms > len => {
                notes.push(format!(
                    "Source is {} long, so the still is its first frame rather than the one \
                     at --start {}.",
                    trim::format_time(len),
                    t.text
                ));
                0
            }
            (Some(len), true) => len - t.ms,
            (None, true) => {
                return Err(ConvError::new(
                    ErrorCode::InvalidInvocation,
                    format!(
                        "cannot read the length of {}, so --start {} cannot count back from \
                         its end; give a time from the start instead",
                        input.display(),
                        t.text
                    ),
                ))
            }
            (_, false) => t.ms,
        };
        if let Some(len) = length {
            if at >= len {
                return Err(ConvError::new(
                    ErrorCode::InvalidInvocation,
                    format!(
                        "--start {} is past the end of {}, which is {} long",
                        t.text,
                        input.display(),
                        trim::format_time(len)
                    ),
                ));
            }
        }
        // A time inside the last frame seeks past every frame's start, and
        // ffmpeg writes nothing: take the last frame.
        let last = match (length, probe.frame_rate) {
            (Some(len), Some(rate)) => Some(len.saturating_sub(frame_ms(rate))),
            _ => None,
        };
        let at = last.map_or(at, |last| at.min(last));
        let pick = Pick {
            seek_ms: at,
            frames: Some(1),
            passthrough: false,
            ..base
        };
        let mut r = report(false, "one", tuning, Some(1));
        r.at_ms = Some(at);
        return Ok((pick, r));
    }

    // The part of the source the series comes from: the range, resolved as
    // a cut is, or all of it.
    let (start, end) = match &tuning.range {
        Some(range) => {
            let clip = trim::resolve(range, probe, input, true)?;
            notes.extend(
                clip.notes
                    .into_iter()
                    .filter(|n| !n.contains("nothing was cut")),
            );
            (clip.report.start_ms, clip.report.end_ms)
        }
        None => (0, length),
    };
    let len = end.map(|e| e - start);
    let frames_here = len.and_then(|l| frames_in(l, probe.frame_rate));
    let gif = probe.video_codec.as_deref() == Some("gif");

    let every = tuning.every.clone().or(gif.then_some(Every::Frame));
    let (mut pick, kind, expected) = match every {
        Some(Every::Interval(t)) if probe.frame_rate.is_some_and(|rate| t.ms < frame_ms(rate)) => {
            let rate = probe.frame_rate.expect("checked above");
            notes.push(format!(
                "--every {} is shorter than a frame at {} fps, so every frame is saved.",
                t.text,
                crate::video::show_rate(rate)
            ));
            (every_frame(&base), "frame", frames_here)
        }
        Some(Every::Interval(t)) => {
            let count = len.map(|l| l.div_ceil(t.ms).max(1));
            let pick = Pick {
                rate: Some(fraction(1000, t.ms)),
                frames: count,
                passthrough: false,
                ..base.clone()
            };
            (pick, "every", count)
        }
        Some(Every::Frame) => (every_frame(&base), "frame", frames_here),
        None => {
            let Some(len) = len else {
                return Err(ConvError::new(
                    ErrorCode::InvalidInvocation,
                    format!(
                        "cannot read the length of {}, so conv cannot spread stills across \
                         it; pass --every 2s for one every 2 seconds, or --start TIME for one",
                        input.display()
                    ),
                ));
            };
            let count = frames_here.map_or(SPREAD, |f| f.min(SPREAD));
            // One in the middle of each of `count` equal parts: the first
            // `interval / 2` in, so the first is not the fade from black a
            // video so often opens with.
            let half = len / count / 2;
            let pick = Pick {
                seek_ms: half,
                len_ms: Some(len - half),
                rate: Some(fraction(count * 1000, len)),
                frames: Some(count),
                passthrough: false,
                ..base.clone()
            };
            let what = if tuning.range.is_some() {
                "range"
            } else {
                "video"
            };
            notes.push(format!(
                "{count} stills spread evenly across the {} {what}. --every 2s saves one \
                 every 2 seconds, --every frame saves every frame, and --start TIME alone \
                 saves the one at TIME.",
                trim::format_time(len)
            ));
            (pick, "spread", Some(count))
        }
    };
    // Relative to the range: the input `-ss` and `-t` place it.
    pick.seek_ms += start;
    if pick.len_ms.is_none() {
        pick.len_ms = len.filter(|_| end != length);
    }
    pick.digits = expected.map_or(4, |n| n.to_string().len().max(4));

    let mut r = report(true, kind, tuning, expected);
    r.start_ms = Some(start);
    r.end_ms = end;
    if let Some(n) = expected.filter(|&n| n > ASK_ABOVE) {
        r.needs_confirmation = true;
        let how = match kind {
            "frame" => "every frame".to_string(),
            _ => format!(
                "one every {}",
                tuning.every.as_ref().map_or("", |e| e.text())
            ),
        };
        let of = len.map_or_else(String::new, |l| format!(" of {}", trim::format_time(l)));
        r.warning = Some(format!(
            "This saves about {} stills ({how}{of}) into {}/.",
            grouped(n),
            pick.name
        ));
    }
    Ok((pick, r))
}

fn every_frame(base: &Pick) -> Pick {
    Pick {
        passthrough: true,
        ..base.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trim::{parse_time, Range};

    fn video(len_ms: u64, rate: (u32, u32)) -> MediaProbe {
        MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            width: Some(1920),
            height: Some(1080),
            frame_rate: Some(rate),
            duration_ms: Some(len_ms),
            ..MediaProbe::default()
        }
    }

    fn gif(len_ms: u64) -> MediaProbe {
        MediaProbe {
            video_codec: Some("gif".into()),
            frame_rate: Some((10, 1)),
            ..video(len_ms, (10, 1))
        }
    }

    fn range(start: Option<&str>, end: Option<&str>) -> Option<Range> {
        Range::new(
            start.map(|s| parse_time(s).unwrap()),
            end.map(|s| parse_time(s).unwrap()),
            None,
        )
        .unwrap()
    }

    fn every(text: &str) -> Option<Every> {
        Some(parse_every(text).unwrap())
    }

    fn planned(
        from: Format,
        to: Format,
        output: &str,
        probe: Option<&MediaProbe>,
        tuning: &Tuning,
    ) -> crate::Result<crate::ConversionPlan> {
        plan(
            from,
            to,
            &[PathBuf::from(format!("clip.{}", from.ext()))],
            Path::new(output),
            probe,
            tuning,
        )
    }

    fn argv(p: &crate::ConversionPlan, step: usize) -> String {
        p.steps[step].argv.join(" ")
    }

    #[test]
    fn every_reads_a_length_or_frame() {
        assert_eq!(parse_every("frame"), Ok(Every::Frame));
        assert_eq!(parse_every("Frames"), Ok(Every::Frame));
        assert!(matches!(parse_every("2s"), Ok(Every::Interval(t)) if t.ms == 2000));
        assert!(matches!(parse_every("1:30"), Ok(Every::Interval(t)) if t.ms == 90_000));
        for bad in ["0", "-2", "soon", ""] {
            assert!(parse_every(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn start_alone_is_one_still_and_anything_else_a_series() {
        let one = Tuning {
            range: range(Some("12"), None),
            ..Tuning::default()
        };
        assert!(is_single(&one));
        assert!(!is_series(Format::Mp4, Format::Png, &one));
        for t in [
            Tuning::default(),
            Tuning {
                range: range(Some("12"), Some("20")),
                ..Tuning::default()
            },
            Tuning {
                range: range(None, Some("20")),
                ..Tuning::default()
            },
            Tuning {
                range: range(Some("12"), None),
                every: every("2s"),
                ..Tuning::default()
            },
        ] {
            assert!(is_series(Format::Mp4, Format::Png, &t), "{t:?}");
        }
        assert!(!is_series(Format::Mp4, Format::Gif, &Tuning::default()));
    }

    #[test]
    fn one_still_seeks_to_its_time() {
        let t = Tuning {
            range: range(Some("0:12"), None),
            ..Tuning::default()
        };
        let p = planned(
            Format::Mp4,
            Format::Png,
            "clip-12s.png",
            Some(&video(60_000, (30, 1))),
            &t,
        )
        .unwrap();
        assert_eq!(
            argv(&p, 0),
            "-ss 12 -i clip.mp4 -map 0:v:0 -frames:v 1 -update 1 -y clip-12s.png"
        );
        let s = p.stills.clone().unwrap();
        assert_eq!(
            (s.series, s.at_ms, s.expected),
            (false, Some(12_000), Some(1))
        );
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    }

    #[test]
    fn one_still_counts_back_from_the_end_and_stops_at_the_last_frame() {
        let src = video(10_000, (25, 1));
        let at = |start: &str| {
            let t = Tuning {
                range: range(Some(start), None),
                ..Tuning::default()
            };
            planned(Format::Mp4, Format::Png, "o.png", Some(&src), &t)
                .map(|p| p.stills.clone().unwrap().at_ms.unwrap())
        };
        assert_eq!(at("-2").unwrap(), 8_000);
        // Inside the last frame: the last frame.
        assert_eq!(at("9.99").unwrap(), 9_960);
        let e = at("10").unwrap_err();
        assert!(e.message.contains("past the end"), "{}", e.message);
        assert_eq!(at("-20").unwrap(), 0);
    }

    #[test]
    fn a_lossy_still_is_drawn_by_ffmpeg_and_encoded_by_imagemagick() {
        let t = Tuning {
            range: range(Some("5"), None),
            quality: Some(80),
            ..Tuning::default()
        };
        let p = planned(
            Format::Mov,
            Format::Jpg,
            "clip-5s.jpg",
            Some(&video(60_000, (30, 1))),
            &t,
        )
        .unwrap();
        assert_eq!(
            argv(&p, 0),
            "-ss 5 -i clip.mov -map 0:v:0 -frames:v 1 -update 1 -y clip-5s.convkit-step0.png"
        );
        assert_eq!(p.steps[1].program, "magick");
        assert_eq!(
            argv(&p, 1),
            "clip-5s.convkit-step0.png -background white -alpha remove -alpha off \
             -quality 80 clip-5s.jpg"
        );
    }

    #[test]
    fn a_video_with_no_time_gives_ten_stills_from_the_middle_of_ten_parts() {
        let p = planned(
            Format::Mp4,
            Format::Png,
            "clip",
            Some(&video(100_000, (30, 1))),
            &Tuning::default(),
        )
        .unwrap();
        assert_eq!(
            argv(&p, 0),
            "-ss 5 -t 95 -i clip.mp4 -map 0:v:0 -vf fps=1/10 -frames:v 10 -y \
                 clip/clip-%04d.png"
        );
        let s = p.stills.clone().unwrap();
        assert_eq!((s.pick, s.expected), ("spread", Some(10)));
        assert!(!s.needs_confirmation);
        assert!(
            p.warnings[0].starts_with("10 stills spread evenly across the 1:40 video."),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn a_tiny_clip_spreads_no_more_stills_than_it_has_frames() {
        let p = planned(
            Format::Mp4,
            Format::Png,
            "clip",
            Some(&video(200, (30, 1))),
            &Tuning::default(),
        )
        .unwrap();
        assert_eq!(p.stills.clone().unwrap().expected, Some(6));
        assert!(argv(&p, 0).contains("-frames:v 6"), "{}", argv(&p, 0));
    }

    #[test]
    fn every_interval_counts_its_ticks_within_the_range() {
        let t = Tuning {
            range: range(Some("1:00"), Some("2:00")),
            every: every("2s"),
            ..Tuning::default()
        };
        let p = planned(
            Format::Mkv,
            Format::Webp,
            "clip-1m00s-2m00s",
            Some(&video(600_000, (24, 1))),
            &t,
        )
        .unwrap();
        assert_eq!(
            argv(&p, 0),
            "-ss 60 -t 60 -i clip.mkv -map 0:v:0 -vf fps=1/2 -frames:v 30 -y \
                 clip-1m00s-2m00s.convkit-step0/clip-1m00s-2m00s-%04d.png"
        );
        assert_eq!(
            argv(&p, 1),
            "mogrify -path clip-1m00s-2m00s -format webp -quality 92 \
                 clip-1m00s-2m00s.convkit-step0/*.png"
        );
        let s = p.stills.clone().unwrap();
        assert_eq!(
            (s.start_ms, s.end_ms, s.expected),
            (Some(60_000), Some(120_000), Some(30))
        );
    }

    #[test]
    fn an_interval_from_a_start_runs_to_the_end_with_no_length_limit() {
        let t = Tuning {
            range: range(Some("50"), None),
            every: every("5s"),
            ..Tuning::default()
        };
        let p = planned(
            Format::Mp4,
            Format::Png,
            "c",
            Some(&video(60_000, (30, 1))),
            &t,
        )
        .unwrap();
        assert!(
            argv(&p, 0).starts_with("-ss 50 -i clip.mp4 -map 0:v:0 -vf fps=1/5 -frames:v 2 "),
            "{}",
            argv(&p, 0)
        );
    }

    #[test]
    fn every_frame_passes_frames_through_and_a_gif_does_so_by_default() {
        let p = planned(
            Format::Gif,
            Format::Png,
            "anim",
            Some(&gif(3_000)),
            &Tuning::default(),
        )
        .unwrap();
        assert_eq!(
            argv(&p, 0),
            "-i clip.gif -map 0:v:0 -fps_mode passthrough -y anim/anim-%04d.png"
        );
        let s = p.stills.clone().unwrap();
        assert_eq!((s.pick, s.expected), ("frame", Some(30)));
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    }

    #[test]
    fn an_interval_shorter_than_a_frame_saves_every_frame_and_says_so() {
        let t = Tuning {
            every: every("0.01"),
            ..Tuning::default()
        };
        let p = planned(
            Format::Mp4,
            Format::Png,
            "c",
            Some(&video(2_000, (30, 1))),
            &t,
        )
        .unwrap();
        assert!(argv(&p, 0).contains("-fps_mode passthrough"));
        assert!(
            p.warnings[0].contains("shorter than a frame"),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn a_large_series_asks_first_and_says_how_many() {
        let t = Tuning {
            every: every("1s"),
            ..Tuning::default()
        };
        let p = planned(
            Format::Mp4,
            Format::Jpg,
            "film",
            Some(&video(7_200_000, (24, 1))),
            &t,
        )
        .unwrap();
        let s = p.stills.clone().unwrap();
        assert!(s.needs_confirmation);
        assert_eq!(s.expected, Some(7_200));
        assert_eq!(
            s.warning.as_deref(),
            Some("This saves about 7,200 stills (one every 1s of 2:00:00) into film/.")
        );
        // The number has room for every one of them.
        assert!(argv(&p, 0).contains("film-%04d.png"));
        let t = Tuning {
            every: every("frame"),
            ..Tuning::default()
        };
        let p = planned(
            Format::Mp4,
            Format::Png,
            "film",
            Some(&video(7_200_000, (24, 1))),
            &t,
        )
        .unwrap();
        let s = p.stills.clone().unwrap();
        assert_eq!(s.expected, Some(172_800));
        assert!(argv(&p, 0).contains("film-%06d.png"), "{}", argv(&p, 0));
        assert!(s.warning.unwrap().contains("(every frame of 2:00:00)"));
        // At the line: not asked.
        let t = Tuning {
            every: every("frame"),
            ..Tuning::default()
        };
        let p = planned(
            Format::Mp4,
            Format::Png,
            "c",
            Some(&video(40_000, (25, 1))),
            &t,
        )
        .unwrap();
        assert_eq!(p.stills.as_ref().unwrap().expected, Some(1000));
        assert!(!p.stills.clone().unwrap().needs_confirmation);
    }

    #[test]
    fn an_hdr_source_is_tonemapped_before_anything_else() {
        let hdr = MediaProbe {
            color_transfer: Some("arib-std-b67".into()),
            ..video(10_000, (30, 1))
        };
        let t = Tuning {
            range: range(Some("1"), None),
            resize: Some("640x".into()),
            ..Tuning::default()
        };
        let p = planned(Format::Mov, Format::Png, "o.png", Some(&hdr), &t).unwrap();
        assert!(
            argv(&p, 0).contains(&format!(r"-vf {TONEMAP},scale=w=min(640\,iw):h=-2 ")),
            "{}",
            argv(&p, 0)
        );
        assert!(p.warnings.iter().any(|w| w == HDR_NOTE), "{:?}", p.warnings);
    }

    #[test]
    fn a_turn_and_a_size_follow_the_pick() {
        let t = Tuning {
            every: every("2s"),
            rotate: Some(crate::rotate::Turn::Right),
            resize: Some("50%".into()),
            ..Tuning::default()
        };
        let p = planned(
            Format::Mp4,
            Format::Png,
            "c",
            Some(&video(10_000, (30, 1))),
            &t,
        )
        .unwrap();
        assert!(
            argv(&p, 0).contains(r"-vf fps=1/2,transpose=clock,scale=w=min(iw*50/100\,iw)"),
            "{}",
            argv(&p, 0)
        );
    }

    #[test]
    fn flags_a_still_has_no_use_for_are_refused() {
        let src = video(10_000, (30, 1));
        for (t, flag) in [
            (
                Tuning {
                    fps: Some("10".into()),
                    ..Tuning::default()
                },
                "--fps",
            ),
            (
                Tuning {
                    quality: Some(50),
                    ..Tuning::default()
                },
                "--quality",
            ),
            (
                Tuning {
                    strip_metadata: true,
                    ..Tuning::default()
                },
                "--strip-metadata",
            ),
        ] {
            let e = planned(Format::Mp4, Format::Png, "c", Some(&src), &t).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidInvocation);
            assert!(e.message.starts_with(flag), "{}", e.message);
        }
    }

    #[test]
    fn a_file_with_no_picture_is_refused() {
        let audio = MediaProbe {
            audio_codecs: vec!["aac".into()],
            duration_ms: Some(10_000),
            ..MediaProbe::default()
        };
        let e = planned(
            Format::Mp4,
            Format::Png,
            "c",
            Some(&audio),
            &Tuning::default(),
        )
        .unwrap_err();
        assert!(e.message.contains("no video stream"), "{}", e.message);
    }

    #[test]
    fn a_spread_needs_the_length_and_an_interval_does_not() {
        let unknown = MediaProbe {
            duration_ms: None,
            ..video(0, (30, 1))
        };
        let e = planned(
            Format::Webm,
            Format::Png,
            "c",
            Some(&unknown),
            &Tuning::default(),
        )
        .unwrap_err();
        assert!(e.message.contains("--every 2s"), "{}", e.message);
        let t = Tuning {
            every: every("2s"),
            ..Tuning::default()
        };
        let p = planned(Format::Webm, Format::Png, "c", Some(&unknown), &t).unwrap();
        assert!(argv(&p, 0).contains("-vf fps=1/2 -y"), "{}", argv(&p, 0));
        assert_eq!(p.stills.clone().unwrap().expected, None);
    }

    #[test]
    fn a_percent_in_a_name_is_not_read_as_a_pattern() {
        let pick = Pick::template("50%off".into());
        assert_eq!(
            pick.pattern(Path::new("a%b/50%off"), "png"),
            "a%%b/50%%off/50%%off-%04d.png"
        );
    }

    #[test]
    fn a_folder_holds_a_series_only_in_its_own_name_and_format() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("clip");
        assert_eq!(occupied(&folder, "png"), None);
        std::fs::create_dir(&folder).unwrap();
        for name in [
            "notes.txt",
            "clip-0001.jpg",
            "clip-final.png",
            "other-0001.png",
        ] {
            std::fs::write(folder.join(name), b"x").unwrap();
        }
        assert_eq!(occupied(&folder, "png"), None);
        assert_eq!(occupied(&folder, "jpg"), Some(folder.join("clip-0001.jpg")));
        std::fs::write(folder.join("CLIP-0002.PNG"), b"x").unwrap();
        assert_eq!(occupied(&folder, "png"), Some(folder.join("CLIP-0002.PNG")));
    }

    #[test]
    fn counts_are_grouped_by_thousands() {
        assert_eq!(grouped(7), "7");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(172_800), "172,800");
        assert_eq!(grouped(1_234_567), "1,234,567");
    }
}
