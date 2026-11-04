//! `conv merge` for video and audio: files joined one after another into
//! one, on ffmpeg.
//!
//! Files that match -- the same codecs, encoder settings, picture size,
//! frame rate, rotation and sound -- in a container the output can hold
//! are joined by ffmpeg's concat demuxer with every stream copied: nothing
//! is re-encoded, and the join takes about as long as copying the files.
//! Files that don't are decoded and encoded once, together, through the
//! concat filter, at the first file's picture size, frame rate and sound.
//! A clip of another size is scaled to fit and padded with black, never
//! stretched or cropped; a clip without sound gets silence for its length,
//! so the sound stays in step with the picture after every join.
//!
//! Like the rest of convkit-core, this never prints.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{name, names_are};
use crate::error::{ConvError, ErrorCode, Result};
use crate::exec::{self, BackendOutput, Event};
use crate::pdf::path_arg;
use crate::pdf::range::join_and;
use crate::procutil::backend_command;
use crate::resolve::ResolvedBackend;
use crate::{registry, winpath, Backend, Format, Kind};

/// What a join needs to know about one input, from one ffprobe call.
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub path: PathBuf,
    /// The container's duration, else the longest stream's.
    pub duration_ms: u64,
    /// The first video stream that is not a cover picture.
    pub video: Option<Picture>,
    /// The first audio stream.
    pub audio: Option<Sound>,
    pub audio_tracks: usize,
    pub subtitles: usize,
    pub chapters: usize,
    /// Where the cover picture (an attached picture) is among the file's
    /// video streams, for `-map N:v:K`.
    pub cover: Option<usize>,
    /// The container's tags, keys lower-cased.
    pub tags: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Picture {
    /// The stream's index in the file: the concat demuxer joins streams by
    /// index, so a copy needs it to be the same in every file.
    pub index: usize,
    /// Its place among the file's video streams, for `-map N:v:K`.
    pub position: usize,
    pub codec: String,
    pub profile: Option<String>,
    pub pix_fmt: Option<String>,
    /// Stored, before the rotation and the pixel shape are applied.
    pub width: u32,
    pub height: u32,
    pub sar: (u32, u32),
    /// The display matrix's turn in degrees, 0, 90, 180 or 270.
    pub rotation: u32,
    /// The rate the clip was filmed at (`nominal_rate`).
    pub rate: Option<(u32, u32)>,
    /// PQ or HLG.
    pub hdr: bool,
    pub extradata: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sound {
    pub index: usize,
    pub position: usize,
    pub codec: String,
    pub profile: Option<String>,
    pub sample_rate: u32,
    pub channels: u32,
    pub layout: Option<String>,
    pub extradata: Option<String>,
}

impl Picture {
    /// The size the picture is shown at: square pixels, turned upright.
    pub fn display_size(&self) -> (u32, u32) {
        let (n, d) = self.sar;
        let w = if n > 0 && d > 0 && n != d {
            (u64::from(self.width) * u64::from(n) / u64::from(d)) as u32
        } else {
            self.width
        };
        if self.rotation % 180 == 90 {
            (self.height, w)
        } else {
            (w, self.height)
        }
    }
}

/// The ffprobe arguments that read `input`. `-show_data_hash` adds each
/// stream's `extradata_hash`: two H.264 streams of the same size and
/// profile can still carry different parameter sets, and an mp4 holds one
/// set per track, so a copy is only safe when they are byte for byte the
/// same.
pub fn probe_args(input: &Path) -> Vec<String> {
    let mut v: Vec<String> = [
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_streams",
        "-show_format",
        "-show_chapters",
        "-show_data_hash",
        "sha256",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    v.push(path_arg(input));
    v
}

/// Reads `input` with the ffprobe at `ffprobe`.
pub fn probe(ffprobe: &Path, input: &Path) -> Result<Clip> {
    if !input.is_file() {
        return Err(ConvError::new(
            ErrorCode::InputNotFound,
            format!("input not found: {}", input.display()),
        ));
    }
    let out = backend_command(ffprobe)
        .args(probe_args(input))
        .output()
        .map_err(|e| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("failed to run {}: {e}", ffprobe.display()),
            )
        })?;
    let json = String::from_utf8_lossy(&out.stdout);
    parse(input, &json).ok_or_else(|| {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let reason = stderr
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .map(|l| {
                let prefix = format!("{}: ", path_arg(input));
                format!(": {}", l.strip_prefix(&prefix).unwrap_or(l))
            })
            .unwrap_or_default();
        ConvError {
            backend: Some(Backend::Ffprobe),
            ..ConvError::new(
                ErrorCode::ConversionFailed,
                format!(
                    "{} could not be read as video or audio{reason}",
                    name(input)
                ),
            )
        }
    })
}

fn seconds_ms(v: &Value) -> Option<u64> {
    let s: f64 = v.as_str()?.parse().ok()?;
    (s.is_finite() && s >= 0.0).then(|| (s * 1000.0).round() as u64)
}

fn ratio(v: &Value) -> Option<(u32, u32)> {
    let (n, d) = v.as_str()?.split_once(['/', ':'])?;
    let (n, d) = (n.parse::<u32>().ok()?, d.parse::<u32>().ok()?);
    (n > 0 && d > 0).then_some((n, d))
}

fn text(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

fn as_f64((n, d): (u32, u32)) -> f64 {
    f64::from(n) / f64::from(d)
}

/// The rates footage is filmed at, which a measured average is snapped to.
const STANDARD_RATES: &[(u32, u32)] = &[
    (24000, 1001),
    (24, 1),
    (25, 1),
    (30000, 1001),
    (30, 1),
    (48, 1),
    (50, 1),
    (60000, 1001),
    (60, 1),
    (90, 1),
    (100, 1),
    (120000, 1001),
    (120, 1),
    (240, 1),
];

/// The rate a clip was filmed at, from ffprobe's two. A phone clip has a
/// variable frame rate: `r_frame_rate` is then either its nominal rate
/// (30/1, with frames dropped in the dark so the average is lower) or the
/// time base's lowest common multiple (600/1, with the average near 30).
/// The first is kept while it is within 1.5 times the average, else the
/// average, and either is snapped to a standard rate within 1.5%, so a
/// measured 29.98 becomes 29.97 and the joined file has a rate a player
/// expects.
pub fn nominal_rate(r: Option<(u32, u32)>, avg: Option<(u32, u32)>) -> Option<(u32, u32)> {
    let picked = match (r, avg) {
        (Some(r), Some(a)) if as_f64(r) <= 240.0 && as_f64(r) <= as_f64(a) * 1.5 => r,
        (Some(_), Some(a)) => a,
        (Some(r), None) => r,
        (None, a) => a?,
    };
    let v = as_f64(picked);
    let nearest = STANDARD_RATES.iter().copied().min_by(|x, y| {
        (as_f64(*x) - v)
            .abs()
            .partial_cmp(&(as_f64(*y) - v).abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    })?;
    Some(if (as_f64(nearest) - v).abs() / v < 0.015 {
        nearest
    } else {
        picked
    })
}

/// Two rates a copy can join: within 2% of each other, so a phone's 29.97
/// and its 30 are one rate.
fn same_rate(a: Option<(u32, u32)>, b: Option<(u32, u32)>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            let (a, b) = (as_f64(a), as_f64(b));
            (a - b).abs() / a.max(b) < 0.02
        }
        (None, None) => true,
        _ => false,
    }
}

/// Turns ffprobe's JSON for `path` into a `Clip`. `None` when it is not
/// ffprobe's report of a media file at all.
pub fn parse(path: &Path, json: &str) -> Option<Clip> {
    let v: Value = serde_json::from_str(json).ok()?;
    let streams = v.get("streams")?.as_array()?;
    let format = &v["format"];
    let mut clip = Clip {
        path: path.to_path_buf(),
        duration_ms: 0,
        video: None,
        audio: None,
        audio_tracks: 0,
        subtitles: 0,
        chapters: v["chapters"].as_array().map_or(0, Vec::len),
        cover: None,
        tags: Vec::new(),
    };
    let (mut video_seen, mut audio_seen, mut longest) = (0usize, 0usize, 0u64);
    for (i, s) in streams.iter().enumerate() {
        let index = s["index"].as_u64().map_or(i, |n| n as usize);
        let codec = text(&s["codec_name"]).unwrap_or_else(|| "unknown".to_string());
        if let Some(d) = seconds_ms(&s["duration"]) {
            longest = longest.max(d);
        }
        match s["codec_type"].as_str() {
            Some("video") => {
                if s["disposition"]["attached_pic"].as_i64() == Some(1) {
                    clip.cover.get_or_insert(video_seen);
                } else if clip.video.is_none() {
                    let rotation = s["side_data_list"]
                        .as_array()
                        .and_then(|l| l.iter().find_map(|d| d["rotation"].as_i64()))
                        .map_or(0, |r| r.rem_euclid(360) as u32);
                    clip.video = Some(Picture {
                        index,
                        position: video_seen,
                        codec,
                        profile: text(&s["profile"]),
                        pix_fmt: text(&s["pix_fmt"]),
                        width: s["width"].as_u64().unwrap_or(0) as u32,
                        height: s["height"].as_u64().unwrap_or(0) as u32,
                        sar: ratio(&s["sample_aspect_ratio"]).unwrap_or((1, 1)),
                        rotation: if rotation % 90 == 0 { rotation } else { 0 },
                        rate: nominal_rate(ratio(&s["r_frame_rate"]), ratio(&s["avg_frame_rate"])),
                        hdr: matches!(
                            s["color_transfer"].as_str(),
                            Some("smpte2084") | Some("arib-std-b67")
                        ),
                        extradata: text(&s["extradata_hash"]),
                    });
                }
                video_seen += 1;
            }
            Some("audio") => {
                if clip.audio.is_none() {
                    clip.audio = Some(Sound {
                        index,
                        position: audio_seen,
                        codec,
                        profile: text(&s["profile"]),
                        sample_rate: s["sample_rate"]
                            .as_str()
                            .and_then(|r| r.parse().ok())
                            .unwrap_or(0),
                        channels: s["channels"].as_u64().unwrap_or(0) as u32,
                        layout: text(&s["channel_layout"]),
                        extradata: text(&s["extradata_hash"]),
                    });
                }
                audio_seen += 1;
            }
            Some("subtitle") => clip.subtitles += 1,
            _ => {}
        }
    }
    clip.audio_tracks = audio_seen;
    clip.duration_ms = seconds_ms(&format["duration"]).unwrap_or(longest);
    // An Ogg file keeps its tags on the audio stream.
    let tags = format["tags"].as_object().or_else(|| {
        streams
            .iter()
            .find(|s| s["codec_type"] == "audio")
            .and_then(|s| s["tags"].as_object())
    });
    if let Some(tags) = tags {
        clip.tags = tags
            .iter()
            .filter_map(|(k, v)| Some((k.to_ascii_lowercase(), v.as_str()?.to_string())))
            .collect();
    }
    Some(clip)
}

/// What the joined file is: the size, rate and sound every clip is made to
/// fit when they are encoded, or the first's when they are copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    pub size: Option<(u32, u32)>,
    pub rate: Option<(u32, u32)>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
}

/// The knobs a merge of clips takes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// `--resize`: the size every clip is fitted within, never larger than
    /// the first clip.
    pub resize: Option<String>,
    /// `--crf`: the encode's quality, which asks for an encode.
    pub crf: Option<u8>,
    /// The quality an encode has when the clips need one, without asking
    /// for one: a join that is encoded again afterwards keeps more.
    pub encode_crf: Option<u8>,
}

/// One argument of a planned ffmpeg call, before the run knows where its
/// files go.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Lit(String),
    /// The concat demuxer's list of files.
    List,
    /// Input `n`'s copy holding only the tracks joined.
    Part(usize),
    Output,
}

fn lit(v: &[&str]) -> Vec<Tok> {
    v.iter().map(|s| Tok::Lit(s.to_string())).collect()
}

/// A planned join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipsPlan {
    pub to: Format,
    pub inputs: Vec<PathBuf>,
    pub output: PathBuf,
    /// Every stream copied, nothing re-encoded.
    pub copied: bool,
    pub duration_ms: u64,
    pub shape: Shape,
    /// Printed as `note` lines; JSON `warnings`.
    pub notes: Vec<String>,
    /// Printed as `warning` lines on stderr; JSON `notes`.
    pub warnings: Vec<String>,
    /// Copies of each input holding only the tracks joined, made first when
    /// the inputs keep their tracks in different orders.
    parts: Vec<Vec<Tok>>,
    main: Vec<Tok>,
}

/// Where a run's files go.
struct Places {
    list: PathBuf,
    parts: Vec<PathBuf>,
    output: PathBuf,
}

impl ClipsPlan {
    fn places(&self, scratch: Option<&Path>) -> Places {
        let ext = self.to.ext();
        let n = self.parts.len();
        match scratch {
            Some(s) => Places {
                list: s.join("list.txt"),
                parts: (1..=n).map(|i| s.join(format!("part-{i}.{ext}"))).collect(),
                output: s.join(self.output.file_name().unwrap_or_default()),
            },
            None => Places {
                list: self.output.with_extension("convkit-list.txt"),
                parts: (1..=n)
                    .map(|i| {
                        self.output
                            .with_extension(format!("convkit-part-{i}.{ext}"))
                    })
                    .collect(),
                output: self.output.clone(),
            },
        }
    }

    /// ffmpeg's arguments for each call, writing the final output: what
    /// `--dry-run` prints.
    pub fn commands(&self) -> Vec<Vec<String>> {
        self.commands_into(None)
    }

    fn commands_into(&self, scratch: Option<&Path>) -> Vec<Vec<String>> {
        let places = self.places(scratch);
        let render = |toks: &[Tok]| -> Vec<String> {
            toks.iter()
                .map(|t| match t {
                    Tok::Lit(s) => s.clone(),
                    Tok::List => path_arg(&places.list),
                    Tok::Part(i) => path_arg(&places.parts[*i]),
                    Tok::Output => path_arg(&places.output),
                })
                .collect()
        };
        self.parts
            .iter()
            .map(|p| render(p))
            .chain(std::iter::once(render(&self.main)))
            .collect()
    }

    /// The concat demuxer's list, and where `--dry-run` says it goes.
    /// `None` when the clips are encoded in one go, which reads them
    /// directly.
    pub fn list(&self) -> Option<(PathBuf, String)> {
        let places = self.places(None);
        self.list_text(&places).map(|t| (places.list, t))
    }

    fn list_text(&self, places: &Places) -> Option<String> {
        let files: Vec<&Path> = if !self.parts.is_empty() {
            places.parts.iter().map(PathBuf::as_path).collect()
        } else if self.copied {
            self.inputs.iter().map(PathBuf::as_path).collect()
        } else {
            return None;
        };
        Some(files.into_iter().map(list_line).collect())
    }
}

/// One line of a concat list. Absolute, since the demuxer reads a relative
/// path against the list's own folder, and in single quotes, inside which
/// only a quote needs escaping (`'\''`), backslashes included, so Windows
/// paths read as they are.
fn list_line(p: &Path) -> String {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    format!("file '{}'\n", abs.to_string_lossy().replace('\'', r"'\''"))
}

fn invalid(message: String) -> ConvError {
    ConvError::new(ErrorCode::InvalidInvocation, message)
}

/// A codec as people know it.
fn codec_label(codec: &str) -> String {
    match codec {
        "h264" => "H.264".into(),
        "hevc" => "HEVC".into(),
        "vp8" => "VP8".into(),
        "vp9" => "VP9".into(),
        "av1" => "AV1".into(),
        "mpeg4" => "MPEG-4".into(),
        "prores" => "ProRes".into(),
        "aac" => "AAC".into(),
        "mp3" => "MP3".into(),
        "opus" => "Opus".into(),
        "vorbis" => "Vorbis".into(),
        "flac" => "FLAC".into(),
        "alac" => "ALAC".into(),
        "ac3" => "AC-3".into(),
        "eac3" => "E-AC-3".into(),
        c if c.starts_with("pcm_") => "PCM".into(),
        c => c.to_ascii_uppercase(),
    }
}

/// `44.1 kHz`, `48 kHz`.
fn khz(rate: u32) -> String {
    let k = f64::from(rate) / 1000.0;
    if (k - k.round()).abs() < 1e-9 {
        format!("{} kHz", k.round() as u64)
    } else {
        format!("{} kHz", format!("{k:.2}").trim_end_matches('0'))
    }
}

fn channels_label(n: u32) -> String {
    match n {
        1 => "mono".into(),
        2 => "stereo".into(),
        6 => "5.1".into(),
        8 => "7.1".into(),
        n => format!("{n}-channel"),
    }
}

/// The channel layout the joined sound gets for `channels` channels: one
/// ffmpeg names, so a clip with one of its odd variants (`5.1(side)`, which
/// libopus refuses) is mixed into it. MP3 holds at most two.
fn layout_for(channels: u32, to: Format) -> &'static str {
    match (channels, to) {
        (1, _) => "mono",
        (_, Format::Mp3) | (2, _) => "stereo",
        (3..=6, _) => "5.1",
        (7..=8, _) => "7.1",
        _ => "stereo",
    }
}

fn layout_channels(layout: &str) -> u32 {
    match layout {
        "mono" => 1,
        "5.1" => 6,
        "7.1" => 8,
        _ => 2,
    }
}

/// How `b`'s picture differs from `a`'s, in what a person sees, and
/// whether it differs in a way only a copy cares about.
fn picture_differences(a: &Picture, b: &Picture) -> (Vec<String>, bool) {
    let mut seen = Vec::new();
    let (bw, bh) = b.display_size();
    if a.display_size() != (bw, bh) {
        seen.push(format!("{bw}x{bh}"));
    }
    if !same_rate(a.rate, b.rate) {
        if let Some(r) = b.rate {
            seen.push(format!("{} fps", crate::video::show_rate(r)));
        }
    }
    if a.codec != b.codec {
        seen.push(codec_label(&b.codec));
    }
    if a.hdr != b.hdr {
        seen.push(if b.hdr { "HDR" } else { "not HDR" }.to_string());
    } else if a.pix_fmt != b.pix_fmt {
        let ten = b.pix_fmt.as_deref().is_some_and(|p| p.contains("10"));
        seen.push(if ten { "10-bit" } else { "8-bit" }.to_string());
    }
    let unseen = a.width != b.width
        || a.height != b.height
        || a.sar != b.sar
        || a.rotation != b.rotation
        || a.profile != b.profile
        || a.extradata != b.extradata;
    (seen, unseen)
}

/// How `b`'s sound differs from `a`'s, as `picture_differences`.
fn sound_differences(a: &Sound, b: &Sound) -> (Vec<String>, bool) {
    let mut seen = Vec::new();
    if a.codec != b.codec {
        seen.push(codec_label(&b.codec));
    }
    if a.sample_rate != b.sample_rate {
        seen.push(khz(b.sample_rate));
    }
    if a.channels != b.channels {
        seen.push(channels_label(b.channels));
    }
    let unseen = a.profile != b.profile || a.layout != b.layout || a.extradata != b.extradata;
    (seen, unseen)
}

/// `b.mov is 1280x720 and HEVC`, `c.mp4 has no sound`.
fn difference_text(clip: &str, seen: &[String], no_sound: bool, unseen: bool) -> Option<String> {
    match (seen.is_empty(), no_sound) {
        (false, true) => Some(format!("{clip} is {}, with no sound", join_and(seen))),
        (true, true) => Some(format!("{clip} has no sound")),
        (false, false) => Some(format!("{clip} is {}", join_and(seen))),
        (true, false) if unseen => Some(format!("{clip} was encoded with other settings")),
        (true, false) => None,
    }
}

/// The most videos one encode opens at once.
const GROUP: usize = 8;

/// The most clips a note names before it counts the rest.
const NAMED: usize = 3;

fn listed(items: &[String]) -> String {
    if items.len() <= NAMED {
        items.join("; ")
    } else {
        format!(
            "{}; and {} more",
            items[..NAMED].join("; "),
            items.len() - NAMED
        )
    }
}

/// Tags no clip owns alone: those every input has with the same value.
/// What only one clip says (its title, its track number, when and where it
/// was filmed) would otherwise be said of the whole file. The muxer's own
/// bookkeeping is left to the muxer.
fn shared_tags(clips: &[Clip]) -> Vec<(String, String)> {
    const OWN: &[&str] = &[
        "encoder",
        "major_brand",
        "minor_version",
        "compatible_brands",
    ];
    let Some((first, rest)) = clips.split_first() else {
        return Vec::new();
    };
    first
        .tags
        .iter()
        .filter(|(k, _)| !OWN.contains(&k.as_str()))
        .filter(|tag| rest.iter().all(|c| c.tags.contains(tag)))
        .cloned()
        .collect()
}

fn tag_args(tags: &[(String, String)]) -> Vec<Tok> {
    let mut v = lit(&["-map_metadata", "-1", "-map_chapters", "-1"]);
    for (k, val) in tags {
        v.push(Tok::Lit("-metadata".into()));
        v.push(Tok::Lit(format!("{k}={val}")));
    }
    v
}

fn fast_start(to: Format) -> Vec<Tok> {
    if matches!(to, Format::Mp4 | Format::Mov | Format::M4a) {
        lit(&["-movflags", "+faststart"])
    } else {
        Vec::new()
    }
}

/// Whether `to` holds a cover picture as conv's own conversions keep one.
fn holds_cover(to: Format) -> bool {
    matches!(to, Format::Mp3 | Format::M4a | Format::Flac)
}

/// A target whose encode loses nothing.
fn lossless(to: Format) -> bool {
    matches!(to, Format::Wav | Format::Flac)
}

/// The codecs `to` holds as they are: its video and its audio.
fn holds(to: Format) -> (&'static [&'static str], &'static [&'static str]) {
    match registry::compat_tables(to) {
        Some(t) => t,
        None => (&[], crate::media::copyable_audio_for(to).unwrap_or(&[])),
    }
}

fn video_codec_args(to: Format, crf: Option<u8>) -> Vec<Tok> {
    if to == Format::Webm {
        let crf = crf.map_or(registry::WEBM_CRF.to_string(), |c| c.to_string());
        let mut v = lit(&["-c:v", "libvpx-vp9", "-crf"]);
        v.push(Tok::Lit(crf));
        v.extend(lit(&[
            "-b:v",
            "0",
            "-row-mt",
            "1",
            "-threads",
            "0",
            "-deadline",
            "good",
            "-cpu-used",
            registry::VP9_CPU_USED,
        ]));
        v
    } else {
        let crf = crf.map_or(registry::CRF.to_string(), |c| c.to_string());
        let mut v = lit(&["-c:v", "libx264", "-crf"]);
        v.push(Tok::Lit(crf));
        v
    }
}

/// The encoder for the joined sound, at the rates conv's own conversions
/// use for `to`.
fn audio_codec_args(to: Format) -> Vec<Tok> {
    match to {
        Format::Webm | Format::Opus | Format::Ogg => {
            lit(&["-c:a", "libopus", "-b:a", registry::WEBM_AUDIO_BITRATE])
        }
        Format::Mp3 => lit(&["-c:a", "libmp3lame", "-q:a", "2"]),
        Format::M4a => lit(&["-c:a", "aac", "-b:a", "192k"]),
        Format::Wav => lit(&["-c:a", "pcm_s16le"]),
        Format::Flac => lit(&["-c:a", "flac"]),
        _ => lit(&["-c:a", "aac", "-b:a", registry::AUDIO_BITRATE]),
    }
}

/// The sample rate the joined sound gets: the first sound's, except where
/// the encoder takes no such rate. libopus works at 48 kHz, and LAME tops
/// out at 48 kHz.
fn sample_rate_for(rate: u32, to: Format) -> u32 {
    match to {
        Format::Webm | Format::Opus | Format::Ogg => 48_000,
        Format::Mp3 => rate.min(48_000),
        _ => rate,
    }
}

/// What the person typed for `--crf` must suit the encoder `to` uses.
fn check_crf(to: Format, crf: Option<u8>) -> Result<()> {
    match crf {
        Some(c) if to != Format::Webm && c > 51 => Err(invalid(format!(
            "--crf goes up to 51 for {}; only webm goes to 63",
            to.ext()
        ))),
        _ => Ok(()),
    }
}

/// Plans joining `clips` (one per input, in order) into `output`, a `to`.
pub fn plan(clips: &[Clip], to: Format, output: &Path, opts: &Options) -> Result<ClipsPlan> {
    let Some(first) = clips.first() else {
        return Err(invalid("no files to merge".into()));
    };
    let video_out = to.kind() == Kind::Video;
    if !video_out && to.kind() != Kind::Audio {
        return Err(invalid(format!(
            "{} is not a video or audio format",
            name(output)
        )));
    }
    if !video_out {
        if let Some(flag) = [
            (opts.resize.is_some(), "--resize"),
            (opts.crf.is_some(), "--crf"),
        ]
        .iter()
        .find_map(|(given, flag)| given.then_some(*flag))
        {
            return Err(invalid(format!(
                "{flag} sets the picture, and {} holds only sound",
                name(output)
            )));
        }
    }
    check_crf(to, opts.crf)?;
    for c in clips {
        if c.video.is_none() && c.audio.is_none() {
            return Err(invalid(format!(
                "{} has no picture or sound",
                name(&c.path)
            )));
        }
        if video_out && c.video.is_none() {
            return Err(invalid(format!(
                "{} has no picture, so it can't be part of a video; merge into an audio \
                 file such as {} instead",
                name(&c.path),
                output.with_extension("m4a").display()
            )));
        }
        if c.duration_ms == 0 {
            return Err(invalid(format!(
                "{} has no length conv can read",
                name(&c.path)
            )));
        }
    }
    let with_sound: Vec<&Clip> = clips.iter().filter(|c| c.audio.is_some()).collect();
    if !video_out && with_sound.is_empty() {
        return Err(invalid("none of the files has sound".into()));
    }
    let what = if video_out { "clips" } else { "files" };
    let out_name = name(output);
    let ref_sound = with_sound.first().and_then(|c| c.audio.as_ref());

    // What stands in the way of a copy.
    let mut differences: Vec<String> = Vec::new();
    for c in &clips[1..] {
        let (mut seen, mut unseen) = (Vec::new(), false);
        if video_out {
            if let (Some(a), Some(b)) = (&first.video, &c.video) {
                let (s, u) = picture_differences(a, b);
                seen.extend(s);
                unseen |= u;
            }
        }
        let no_sound = ref_sound.is_some() && c.audio.is_none();
        if let (Some(a), Some(b)) = (ref_sound, &c.audio) {
            let (s, u) = sound_differences(a, b);
            seen.extend(s);
            unseen |= u;
        }
        // A FLAC is encoded whatever, and its header (length, checksum)
        // always differs, so only what a person hears is worth saying.
        let unseen = unseen && to != Format::Flac;
        if let Some(t) = difference_text(&name(&c.path), &seen, no_sound, unseen) {
            differences.push(t);
        }
    }
    // A first clip without sound differs from the rest, which have some.
    if ref_sound.is_some() && first.audio.is_none() {
        differences.insert(0, format!("{} has no sound", name(&first.path)));
    }
    let (holds_video, holds_audio) = holds(to);
    let mut cannot_hold: Vec<String> = Vec::new();
    let mut foreign: Vec<String> = Vec::new();
    let add = |list: &mut Vec<String>, s: String| {
        if !list.contains(&s) {
            list.push(s);
        }
    };
    for c in clips {
        let from = Format::from_path(&c.path).unwrap_or(to);
        if let Some(v) = c.video.as_ref().filter(|_| video_out) {
            if !holds_video.contains(&v.codec.as_str()) {
                add(&mut foreign, format!("{} video", codec_label(&v.codec)));
            } else if let Some(w) = registry::why_not_remuxed(from, to, &v.codec) {
                add(&mut cannot_hold, w.trim_end_matches('.').to_string());
            }
        }
        if let Some(a) = &c.audio {
            if !holds_audio.contains(&a.codec.as_str()) {
                add(&mut foreign, format!("{} sound", codec_label(&a.codec)));
            }
        }
    }
    if !foreign.is_empty() {
        let held = match foreign.as_slice() {
            [one] => one.clone(),
            [init @ .., last] => format!("{} or {last}", init.join(", ")),
            [] => unreachable!(),
        };
        cannot_hold.insert(0, format!("{} can't hold {held}", to.ext()));
    }
    // A FLAC encode loses nothing, and a copy would carry the first file's
    // header (its length and checksum) for the whole.
    let copied = differences.is_empty()
        && cannot_hold.is_empty()
        && opts.resize.is_none()
        && opts.crf.is_none()
        && to != Format::Flac;

    let mut notes = Vec::new();
    let mut warnings = Vec::new();
    if clips.len() == 1 && copied {
        notes.push(format!(
            "Only one input, so {out_name} is a copy of {}.",
            name(&first.path)
        ));
    }

    // Tracks only the first of each kind is joined.
    let extra_audio: Vec<String> = clips
        .iter()
        .filter(|c| c.audio_tracks > 1)
        .map(|c| name(&c.path))
        .collect();
    if !extra_audio.is_empty() {
        notes.push(format!(
            "Only the first sound track of {} is kept.",
            join_and(&extra_audio)
        ));
    }
    let subtitled: Vec<String> = clips
        .iter()
        .filter(|c| c.subtitles > 0)
        .map(|c| name(&c.path))
        .collect();
    if !subtitled.is_empty() && video_out {
        notes.push(format!(
            "Subtitles in {} are left out.",
            join_and(&subtitled)
        ));
    }
    let chaptered: Vec<String> = clips
        .iter()
        .filter(|c| c.chapters > 0)
        .map(|c| name(&c.path))
        .collect();
    if !chaptered.is_empty() {
        notes.push(format!(
            "Chapters in {} are left out, since they would mark only part of {out_name}.",
            join_and(&chaptered)
        ));
    }
    // The first cover picture, for an audio file that holds one.
    let cover = if video_out {
        None
    } else {
        clips
            .iter()
            .enumerate()
            .find_map(|(i, c)| c.cover.map(|k| (i, k)))
    };
    let cover = match cover {
        Some(_) if crate::media::is_ogg(to) => {
            notes.push(registry::COVER_ART_DROPPED_NOTE.to_string());
            None
        }
        Some(c) if holds_cover(to) => Some(c),
        _ => None,
    };
    let tags = shared_tags(clips);
    let duration_ms: u64 = clips.iter().map(|c| c.duration_ms).sum();

    if copied {
        let v_index = first.video.as_ref().filter(|_| video_out).map(|v| v.index);
        let a_index = first.audio.as_ref().map(|a| a.index);
        let one_order = clips.iter().all(|c| {
            c.video.as_ref().filter(|_| video_out).map(|v| v.index) == v_index
                && c.audio.as_ref().map(|a| a.index) == a_index
        });
        for c in clips {
            if c.path.to_string_lossy().contains(['\n', '\r']) {
                return Err(invalid(format!(
                    "{} has a line break in its name, which ffmpeg's list of files can't \
                     hold; rename it",
                    c.path.display()
                )));
            }
        }
        // Copies holding just the joined tracks, video first, when the
        // inputs keep them in different orders: the concat demuxer joins
        // the first file's track 1 to every file's track 1.
        let parts: Vec<Vec<Tok>> = if one_order {
            Vec::new()
        } else {
            clips
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let mut v = vec![Tok::Lit("-i".into()), Tok::Lit(path_arg(&c.path))];
                    if let Some(p) = c.video.as_ref().filter(|_| video_out) {
                        v.push(Tok::Lit("-map".into()));
                        v.push(Tok::Lit(format!("0:v:{}", p.position)));
                    }
                    if let Some(a) = &c.audio {
                        v.push(Tok::Lit("-map".into()));
                        v.push(Tok::Lit(format!("0:a:{}", a.position)));
                    }
                    v.extend(lit(&["-c", "copy", "-map_metadata", "-1", "-y"]));
                    v.push(Tok::Part(i));
                    v
                })
                .collect()
        };
        let mut main = lit(&["-f", "concat", "-safe", "0", "-i"]);
        main.push(Tok::List);
        if let Some((i, _)) = cover {
            main.push(Tok::Lit("-i".into()));
            main.push(Tok::Lit(path_arg(&clips[i].path)));
        }
        let (v_pos, a_pos) = if parts.is_empty() {
            (
                first.video.as_ref().map_or(0, |v| v.position),
                first.audio.as_ref().map_or(0, |a| a.position),
            )
        } else {
            (0, 0)
        };
        if video_out {
            main.extend(lit(&["-map", &format!("0:v:{v_pos}")]));
        }
        if first.audio.is_some() {
            main.extend(lit(&["-map", &format!("0:a:{a_pos}")]));
        }
        if let Some((_, k)) = cover {
            main.extend(lit(&["-map", &format!("1:v:{k}")]));
            main.extend(lit(&["-disposition:v:0", "attached_pic"]));
        }
        main.extend(lit(&["-c", "copy"]));
        main.extend(tag_args(&tags));
        main.extend(fast_start(to));
        main.extend(lit(&["-y"]));
        main.push(Tok::Output);
        let shape = Shape {
            size: first.video.as_ref().map(Picture::display_size),
            rate: first.video.as_ref().and_then(|v| v.rate),
            sample_rate: first.audio.as_ref().map(|a| a.sample_rate),
            channels: first.audio.as_ref().map(|a| a.channels),
        };
        return Ok(ClipsPlan {
            to,
            inputs: clips.iter().map(|c| c.path.clone()).collect(),
            output: output.to_path_buf(),
            copied,
            duration_ms,
            shape,
            notes,
            warnings,
            parts,
            main,
        });
    }

    // Encoded: every clip made to fit the first's picture and sound.
    let size = first.video.as_ref().filter(|_| video_out).map(|v| {
        let natural = v.display_size();
        let (w, h) = match &opts.resize {
            Some(g) => crate::video::fit_within(g, natural),
            None => natural,
        };
        ((w & !1).max(2), (h & !1).max(2))
    });
    let rate = size.map(|_| first.video.as_ref().and_then(|v| v.rate).unwrap_or((30, 1)));
    let sound = ref_sound.map(|a| {
        let layout = layout_for(a.channels.max(1), to);
        (sample_rate_for(a.sample_rate.max(8000), to), layout)
    });

    let hdr: Vec<&Clip> = clips
        .iter()
        .filter(|c| c.video.as_ref().is_some_and(|v| v.hdr))
        .collect();
    let tone_map = video_out && !hdr.is_empty() && hdr.len() < clips.len();

    // Each clip's filters, as `[N:v:K]` and `[N:a:K]` of its input N.
    let mut pictures: Vec<Option<String>> = Vec::with_capacity(clips.len());
    let mut sounds: Vec<Option<String>> = Vec::with_capacity(clips.len());
    let mut enlarged: Vec<String> = Vec::new();
    let mut barred: Vec<String> = Vec::new();
    let mut silent: Vec<String> = Vec::new();
    for c in clips {
        pictures.push(None);
        sounds.push(None);
        if let (Some((w, h)), Some(r), Some(v)) = (size, rate, &c.video) {
            let (cw, ch) = v.display_size();
            if cw > 0 && ch > 0 {
                let k = (f64::from(w) / f64::from(cw)).min(f64::from(h) / f64::from(ch));
                if k > 1.001 && opts.resize.is_none() {
                    enlarged.push(name(&c.path));
                }
                let (src, dst) = (f64::from(cw) / f64::from(ch), f64::from(w) / f64::from(h));
                if (src - dst).abs() / dst > 0.01 {
                    barred.push(name(&c.path));
                }
            }
            // ffmpeg turns each clip upright by its rotation before the
            // filters see it. `fps` first, so a 60 fps clip joined to a 30
            // fps one is scaled at 30. A variable-rate phone clip comes out
            // at the steady rate too.
            let tone = if tone_map && v.hdr {
                "scale=out_color_matrix=bt709:out_primaries=bt709:out_transfer=bt709:\
                 intent=perceptual,"
            } else {
                ""
            };
            *pictures.last_mut().unwrap() = Some(format!(
                "[N:v:{p}]{tone}fps={n}/{d},scale={w}:{h}:force_original_aspect_ratio=decrease,\
                 pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,format=yuv420p",
                p = v.position,
                n = r.0,
                d = r.1,
            ));
        }
        if let Some((sr, layout)) = sound {
            *sounds.last_mut().unwrap() = Some(match &c.audio {
                Some(a) => format!(
                    "[N:a:{p}]aresample={sr},aformat=sample_rates={sr}:channel_layouts={layout}",
                    p = a.position,
                ),
                None => {
                    silent.push(name(&c.path));
                    format!(
                        "anullsrc=r={sr}:cl={layout}:d={}.{:03}",
                        c.duration_ms / 1000,
                        c.duration_ms % 1000
                    )
                }
            });
        }
    }
    let (v_out, a_out) = (size.is_some(), sound.is_some());
    // One encode of `group` (indices into `clips`) into `out`.
    let encode = |group: &[usize], out: Tok, last: bool| -> Vec<Tok> {
        let mut argv: Vec<Tok> = Vec::new();
        for &i in group {
            argv.push(Tok::Lit("-i".into()));
            argv.push(Tok::Lit(path_arg(&clips[i].path)));
        }
        let mut graph: Vec<String> = Vec::new();
        let mut segments = String::new();
        for (j, &i) in group.iter().enumerate() {
            if let Some(chain) = &pictures[i] {
                graph.push(format!(
                    "{}[v{j}]",
                    chain.replacen("[N:", &format!("[{j}:"), 1)
                ));
                segments.push_str(&format!("[v{j}]"));
            }
            if let Some(chain) = &sounds[i] {
                graph.push(format!(
                    "{}[a{j}]",
                    chain.replacen("[N:", &format!("[{j}:"), 1)
                ));
                segments.push_str(&format!("[a{j}]"));
            }
        }
        graph.push(format!(
            "{segments}concat=n={}:v={}:a={}{}{}",
            group.len(),
            u8::from(v_out),
            u8::from(a_out),
            if v_out { "[v]" } else { "" },
            if a_out { "[a]" } else { "" },
        ));
        argv.push(Tok::Lit("-filter_complex".into()));
        argv.push(Tok::Lit(graph.join(";")));
        if v_out {
            argv.extend(lit(&["-map", "[v]"]));
        }
        if a_out {
            argv.extend(lit(&["-map", "[a]"]));
        }
        if let Some((i, k)) = cover.filter(|_| last) {
            argv.extend(lit(&["-map", &format!("{i}:v:{k}")]));
            argv.extend(lit(&["-c:v", "copy", "-disposition:v:0", "attached_pic"]));
        }
        if v_out {
            argv.extend(video_codec_args(to, opts.crf.or(opts.encode_crf)));
            // Each clip is already at the rate; this fills the gap a clip
            // that starts its picture late would leave at its join.
            argv.extend(lit(&["-fps_mode", "cfr"]));
        }
        if a_out {
            argv.extend(audio_codec_args(to));
        }
        if last {
            argv.extend(tag_args(&tags));
            argv.extend(fast_start(to));
        } else {
            argv.extend(lit(&["-map_metadata", "-1"]));
        }
        argv.extend(lit(&["-y"]));
        argv.push(out);
        argv
    };
    let all: Vec<usize> = (0..clips.len()).collect();
    // Many videos are encoded a group at a time, and the groups, made with
    // the same settings, joined by copy. ffmpeg opens every input of an
    // encode at once, each with its decoder and a queue of decoded frames:
    // about 10 MB a 1080p H.264 clip on top of the encoder's 700 MB,
    // measured, and more for 4K HEVC. The groups keep that, and the files
    // held open, bounded however many clips there are.
    let (parts, main) = if video_out && clips.len() > GROUP {
        let parts: Vec<Vec<Tok>> = all
            .chunks(GROUP)
            .enumerate()
            .map(|(g, group)| encode(group, Tok::Part(g), false))
            .collect();
        let mut main = lit(&["-f", "concat", "-safe", "0", "-i"]);
        main.push(Tok::List);
        main.extend(lit(&["-map", "0:v:0"]));
        if a_out {
            main.extend(lit(&["-map", "0:a:0"]));
        }
        main.extend(lit(&["-c", "copy"]));
        main.extend(tag_args(&tags));
        main.extend(fast_start(to));
        main.extend(lit(&["-y"]));
        main.push(Tok::Output);
        (parts, main)
    } else {
        (Vec::new(), encode(&all, Tok::Output, true))
    };

    // The note that says why, and to what.
    let verb = if lossless(to) {
        "Converted"
    } else {
        "Re-encoded"
    };
    let target = match (size, rate, sound) {
        (Some((w, h)), Some(r), _) => format!("{w}x{h} at {} fps", crate::video::show_rate(r)),
        (_, _, Some((sr, layout))) => format!("{} {layout}", khz(sr)),
        _ => String::new(),
    };
    let like = if video_out {
        Some(first)
    } else {
        with_sound.first().copied()
    };
    let as_first = match like {
        Some(c) if opts.resize.is_none() => format!(", as {} is", name(&c.path)),
        _ => String::new(),
    };
    let mut why: Vec<String> = Vec::new();
    if opts.resize.is_some() {
        why.push("--resize sets the size".into());
    }
    if let Some(c) = opts.crf {
        why.push(format!("--crf {c} sets the quality"));
    }
    why.extend(cannot_hold.iter().cloned());
    let mut sentence = String::new();
    if !why.is_empty() {
        sentence.push_str(&join_and(&why));
    }
    if !differences.is_empty() {
        if !sentence.is_empty() {
            sentence.push_str(", and ");
        }
        sentence.push_str(&format!("the {what} differ: {}", listed(&differences)));
    }
    if !sentence.is_empty() {
        notes.insert(
            0,
            format!("{verb} to {target}{as_first}, since {sentence}."),
        );
    }
    if !barred.is_empty() {
        let (names, verb) = names_are(&barred);
        notes.push(format!(
            "{names} {verb} another shape, so black bars fill out the frame."
        ));
    }
    if !enlarged.is_empty() {
        let (names, verb) = names_are(&enlarged);
        let it = if enlarged.len() == 1 {
            "it is"
        } else {
            "they are"
        };
        notes.push(format!(
            "{names} {verb} smaller, so {it} enlarged to fit, which adds no detail."
        ));
    }
    if !silent.is_empty() && sound.is_some() {
        let (names, _) = names_are(&silent);
        let (has, it) = if silent.len() == 1 {
            ("has", "it is")
        } else {
            ("have", "they are")
        };
        notes.push(format!(
            "{names} {has} no sound, so {it} silent in {out_name}."
        ));
    }
    if tone_map {
        let names: Vec<String> = hdr.iter().map(|c| name(&c.path)).collect();
        let (names, verb) = names_are(&names);
        let it = if hdr.len() == 1 { "it is" } else { "they are" };
        warnings.push(format!(
            "{names} {verb} HDR and the rest are not, so {it} turned to standard range to \
             match; colors may look flatter."
        ));
    }

    Ok(ClipsPlan {
        to,
        inputs: clips.iter().map(|c| c.path.clone()).collect(),
        output: output.to_path_buf(),
        copied,
        duration_ms,
        shape: Shape {
            size,
            rate,
            sample_rate: sound.map(|(sr, _)| sr),
            channels: sound.map(|(_, l)| layout_channels(l)),
        },
        notes,
        warnings,
        parts,
        main,
    })
}

/// Reads every input with ffprobe (each distinct path once) and plans the
/// join.
pub fn plan_merge(
    ffprobe: &Path,
    inputs: &[PathBuf],
    to: Format,
    output: &Path,
    opts: &Options,
) -> Result<ClipsPlan> {
    let mut clips: Vec<Clip> = Vec::with_capacity(inputs.len());
    for input in inputs {
        let clip = match clips.iter().find(|c| &c.path == input) {
            Some(c) => c.clone(),
            None => probe(ffprobe, input)?,
        };
        clips.push(clip);
    }
    plan(&clips, to, output, opts)
}

/// A finished join.
#[derive(Debug, Clone)]
pub struct ClipsOutcome {
    pub output: PathBuf,
    pub bytes: u64,
    pub duration_ms: u64,
    pub inputs: Vec<PathBuf>,
    /// Every stream copied, nothing re-encoded.
    pub copied: bool,
    pub shape: Shape,
    /// Printed as `note` lines; JSON `warnings`.
    pub notes: Vec<String>,
    /// Printed as `warning` lines on stderr; JSON `notes`.
    pub warnings: Vec<String>,
    pub backend_output: Vec<BackendOutput>,
    /// ffmpeg's version, for JSON `backends`.
    pub version: String,
}

/// Lines a copy prints at every join, which are not trouble to report. The
/// last packets of one file and the first of the next overlap by the
/// encoder's padding, a few milliseconds of sound, and ffmpeg fixes the
/// timestamps itself. And the concat demuxer reads every track of a phone
/// clip, timecode and Apple's metadata tracks included, which it has no
/// decoder for; those are not joined.
fn routine_at_a_join(line: &str) -> bool {
    line.contains("Non-monotonic DTS")
        || line.contains("non monotonically increasing dts")
        || (line.contains("Could not find codec parameters") && line.ends_with("unknown codec"))
}

/// Runs `plan` with `ffmpeg`. Refuses before running anything when the
/// output exists (unless `overwrite`) or names a reserved Windows device.
/// Everything is written into a scratch folder beside the output, removed
/// on every exit path, and the output moves into place only once it is
/// complete and not empty.
pub fn run(
    plan: &ClipsPlan,
    ffmpeg: &ResolvedBackend,
    overwrite: bool,
    on_event: &mut dyn FnMut(Event),
) -> Result<ClipsOutcome> {
    winpath::check_output_name(&plan.output)?;
    if plan.output.is_dir() {
        return Err(ConvError::new(
            ErrorCode::OutputExists,
            format!("{} is a folder", plan.output.display()),
        ));
    }
    if plan.output.exists() && !overwrite {
        return Err(ConvError::new(
            ErrorCode::OutputExists,
            format!("{} exists; pass -y to overwrite", plan.output.display()),
        ));
    }
    let dest_dir = plan
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let scratch = exec::make_scratch_dir(dest_dir)?;
    let _guard = exec::ScratchGuard::new(scratch.clone());
    let places = plan.places(Some(&scratch));
    if let Some(list) = plan.list_text(&places) {
        std::fs::write(&places.list, list).map_err(|e| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("could not write the list of files: {e}"),
            )
        })?;
    }

    let commands = plan.commands_into(Some(&scratch));
    let total = commands.len();
    let mut backend_output = Vec::new();
    let mut warnings = plan.warnings.clone();
    for (index, argv) in commands.iter().enumerate() {
        on_event(Event::StepStarted {
            index,
            total,
            backend: Backend::Ffmpeg,
        });
        let mut cmd = backend_command(&ffmpeg.path);
        cmd.args(argv);
        on_event(Event::StepSpawned {
            index,
            program: ffmpeg.path.clone(),
            argv: argv.clone(),
        });
        let out = cmd.output().map_err(|e| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("failed to run {}: {e}", ffmpeg.path.display()),
            )
        })?;
        let report = hide_scratch(plan, &places, &String::from_utf8_lossy(&out.stderr));
        let tail = exec::tail_str(&report, 16 * 1024).to_string();
        if !report.trim().is_empty() {
            on_event(Event::StepReport {
                index,
                backend: Backend::Ffmpeg,
                report: tail.clone(),
            });
            backend_output.push(BackendOutput {
                backend: Backend::Ffmpeg,
                stderr: tail,
            });
        }
        let kept: String = report
            .lines()
            .filter(|l| !routine_at_a_join(l))
            .map(|l| format!("{l}\n"))
            .collect();
        let wrote = if index + 1 == total {
            &places.output
        } else {
            &places.parts[index]
        };
        let made = std::fs::metadata(wrote).is_ok_and(|m| m.len() > 0);
        if !out.status.success() || !made {
            let salient = exec::classify_backend_noise(Backend::Ffmpeg, &kept);
            let detail = if salient.is_empty() {
                let lines: Vec<&str> = kept.lines().rev().take(3).collect();
                lines.into_iter().rev().collect::<Vec<_>>().join("; ")
            } else {
                salient.into_iter().take(3).collect::<Vec<_>>().join("; ")
            };
            return Err(ConvError {
                backend: Some(Backend::Ffmpeg),
                ..ConvError::new(
                    ErrorCode::ConversionFailed,
                    format!("ffmpeg failed ({}): {detail}", out.status),
                )
            });
        }
        for w in exec::classify_backend_noise(Backend::Ffmpeg, &kept) {
            if !warnings.contains(&w) {
                warnings.push(w);
            }
        }
        on_event(Event::StepFinished { index });
    }

    let bytes = std::fs::metadata(&places.output).map_or(0, |m| m.len());
    std::fs::rename(&places.output, &plan.output).map_err(|e| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("could not move {} into place: {e}", name(&plan.output)),
        )
    })?;
    Ok(ClipsOutcome {
        output: plan.output.clone(),
        bytes,
        duration_ms: plan.duration_ms,
        inputs: plan.inputs.clone(),
        copied: plan.copied,
        shape: plan.shape,
        notes: plan.notes.clone(),
        warnings,
        backend_output,
        version: ffmpeg.version.clone(),
    })
}

/// ffmpeg's report with the scratch paths it names put back as what the
/// person knows: the output's own name, each part's input. Longest first,
/// so a path is never half replaced.
fn hide_scratch(plan: &ClipsPlan, places: &Places, report: &str) -> String {
    let mut pairs: Vec<(String, String)> = vec![
        (places.output.display().to_string(), name(&plan.output)),
        (
            places.list.display().to_string(),
            "the list of files".to_string(),
        ),
    ];
    // A copy's parts are one per input; an encode's, one per group.
    let per_input = places.parts.len() == plan.inputs.len();
    for (i, part) in places.parts.iter().enumerate() {
        let shown = if per_input {
            name(&plan.inputs[i])
        } else {
            format!("part {} of the join", i + 1)
        };
        pairs.push((part.display().to_string(), shown));
    }
    pairs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    let mut out = report.to_string();
    for (from, to) in pairs {
        out = out.replace(&from, &to);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pic(w: u32, h: u32, rate: (u32, u32)) -> Picture {
        Picture {
            index: 0,
            position: 0,
            codec: "h264".into(),
            profile: Some("High".into()),
            pix_fmt: Some("yuv420p".into()),
            width: w,
            height: h,
            sar: (1, 1),
            rotation: 0,
            rate: Some(rate),
            hdr: false,
            extradata: Some("SHA256:v".into()),
        }
    }

    fn sound(rate: u32, channels: u32) -> Sound {
        Sound {
            index: 1,
            position: 0,
            codec: "aac".into(),
            profile: Some("LC".into()),
            sample_rate: rate,
            channels,
            layout: Some(channels_label(channels)),
            extradata: Some(format!("SHA256:a{rate}{channels}")),
        }
    }

    fn clip(path: &str, video: Option<Picture>, audio: Option<Sound>) -> Clip {
        Clip {
            path: PathBuf::from(path),
            duration_ms: 2000,
            video,
            audio,
            audio_tracks: 1,
            subtitles: 0,
            chapters: 0,
            cover: None,
            tags: Vec::new(),
        }
    }

    fn hd(path: &str) -> Clip {
        clip(path, Some(pic(1920, 1080, (30, 1))), Some(sound(48000, 2)))
    }

    fn song(path: &str) -> Clip {
        let mut s = sound(44100, 2);
        s.codec = "mp3".into();
        s.index = 0;
        clip(path, None, Some(s))
    }

    fn plan_of(clips: &[Clip], out: &str) -> ClipsPlan {
        let to = Format::from_path(Path::new(out)).unwrap();
        plan(clips, to, Path::new(out), &Options::default()).unwrap()
    }

    fn main_argv(p: &ClipsPlan) -> Vec<String> {
        p.commands().pop().unwrap()
    }

    fn has(argv: &[String], pair: [&str; 2]) -> bool {
        argv.windows(2).any(|w| w == pair)
    }

    #[test]
    fn a_phone_rate_is_its_nominal_rate() {
        // Frames dropped in the dark: the average is lower than the rate.
        assert_eq!(nominal_rate(Some((30, 1)), Some((1530, 59))), Some((30, 1)));
        // The time base's multiple: the average is the truth, snapped.
        assert_eq!(
            nominal_rate(Some((600, 1)), Some((17988, 600))),
            Some((30000, 1001))
        );
        assert_eq!(
            nominal_rate(Some((24000, 1001)), Some((24000, 1001))),
            Some((24000, 1001))
        );
        // Nothing standard nearby: kept as it is.
        assert_eq!(nominal_rate(None, Some((15, 1))), Some((15, 1)));
        assert_eq!(nominal_rate(None, None), None);
    }

    #[test]
    fn ffprobes_report_becomes_a_clip() {
        let json = r#"{
          "streams": [
            {"index": 0, "codec_type": "video", "codec_name": "hevc", "profile": "Main 10",
             "pix_fmt": "yuv420p10le", "width": 1920, "height": 1080,
             "sample_aspect_ratio": "1:1", "r_frame_rate": "30/1", "avg_frame_rate": "1530/59",
             "color_transfer": "arib-std-b67", "extradata_hash": "SHA256:x",
             "side_data_list": [{"side_data_type": "Display Matrix", "rotation": -90}],
             "duration": "1.966667", "disposition": {"attached_pic": 0}},
            {"index": 1, "codec_type": "audio", "codec_name": "aac", "profile": "LC",
             "sample_rate": "44100", "channels": 1, "channel_layout": "mono",
             "duration": "2.000000", "extradata_hash": "SHA256:y"},
            {"index": 2, "codec_type": "data", "codec_name": "none"},
            {"index": 3, "codec_type": "audio", "codec_name": "aac", "sample_rate": "48000"}
          ],
          "format": {"duration": "2.010000", "tags": {"title": "Trip", "Encoder": "x"}},
          "chapters": [{"id": 0}]
        }"#;
        let c = parse(Path::new("IMG_1.MOV"), json).unwrap();
        let v = c.video.as_ref().unwrap();
        assert_eq!((v.codec.as_str(), v.width, v.height), ("hevc", 1920, 1080));
        assert_eq!(v.rotation, 270);
        assert_eq!(v.display_size(), (1080, 1920));
        assert_eq!(v.rate, Some((30, 1)));
        assert!(v.hdr);
        let a = c.audio.as_ref().unwrap();
        assert_eq!((a.sample_rate, a.channels, a.index), (44100, 1, 1));
        assert_eq!((c.audio_tracks, c.chapters, c.duration_ms), (2, 1, 2010));
        assert_eq!(
            c.tags,
            vec![
                ("encoder".to_string(), "x".to_string()),
                ("title".to_string(), "Trip".to_string())
            ]
        );
        assert!(parse(Path::new("x"), "not json").is_none());
    }

    #[test]
    fn a_cover_picture_is_not_the_video() {
        let json = r#"{"streams": [
            {"index": 0, "codec_type": "audio", "codec_name": "mp3", "sample_rate": "44100", "channels": 2},
            {"index": 1, "codec_type": "video", "codec_name": "mjpeg", "disposition": {"attached_pic": 1}}
          ], "format": {"duration": "3.0"}}"#;
        let c = parse(Path::new("t.mp3"), json).unwrap();
        assert!(c.video.is_none());
        assert_eq!(c.cover, Some(0));
    }

    #[test]
    fn matching_clips_are_copied_through_the_concat_demuxer() {
        let p = plan_of(&[hd("a.mp4"), hd("b.mov")], "out.mp4");
        assert!(p.copied);
        assert!(p.notes.is_empty(), "{:?}", p.notes);
        assert_eq!(p.duration_ms, 4000);
        assert_eq!(
            main_argv(&p),
            [
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
                "out.convkit-list.txt",
                "-map",
                "0:v:0",
                "-map",
                "0:a:0",
                "-c",
                "copy",
                "-map_metadata",
                "-1",
                "-map_chapters",
                "-1",
                "-movflags",
                "+faststart",
                "-y",
                "out.mp4"
            ]
        );
        let (path, list) = p.list().unwrap();
        assert_eq!(path, PathBuf::from("out.convkit-list.txt"));
        let abs = std::path::absolute("a.mp4").unwrap();
        assert!(
            list.starts_with(&format!("file '{}'\n", abs.display())),
            "{list}"
        );
        assert_eq!(list.lines().count(), 2);
    }

    #[test]
    fn a_quote_in_a_name_is_escaped_in_the_list() {
        let line = list_line(Path::new("/v/it's.mp4"));
        assert_eq!(line, "file '/v/it'\\''s.mp4'\n");
    }

    #[test]
    fn two_phone_clips_with_slightly_different_rates_still_copy() {
        let mut b = hd("b.mov");
        b.video.as_mut().unwrap().rate = Some((30000, 1001));
        assert!(plan_of(&[hd("a.mov"), b], "out.mov").copied);
    }

    #[test]
    fn clips_that_differ_are_encoded_to_the_first_and_the_note_says_how() {
        let mut b = hd("b.mov");
        *b.video.as_mut().unwrap() = Picture {
            codec: "hevc".into(),
            ..pic(1280, 720, (60, 1))
        };
        let c = clip("c.mp4", Some(pic(1920, 1080, (30, 1))), None);
        let p = plan_of(&[hd("a.mp4"), b, c], "out.mp4");
        assert!(!p.copied);
        assert_eq!(p.shape.size, Some((1920, 1080)));
        assert_eq!(p.shape.rate, Some((30, 1)));
        assert_eq!(
            p.notes[0],
            "Re-encoded to 1920x1080 at 30 fps, as a.mp4 is, since the clips differ: \
             b.mov is 1280x720, 60 fps and HEVC; c.mp4 has no sound."
        );
        assert!(
            p.notes.contains(
                &"b.mov is smaller, so it is enlarged to fit, which adds no detail.".to_string()
            ),
            "{:?}",
            p.notes
        );
        assert!(
            p.notes
                .contains(&"c.mp4 has no sound, so it is silent in out.mp4.".to_string()),
            "{:?}",
            p.notes
        );
        let argv = main_argv(&p);
        let graph = &argv[argv.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        assert!(graph.contains("[1:v:0]fps=30/1,scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,setsar=1,format=yuv420p[v1]"), "{graph}");
        assert!(
            graph.contains("anullsrc=r=48000:cl=stereo:d=2.000[a2]"),
            "{graph}"
        );
        assert!(
            graph.ends_with("[v0][a0][v1][a1][v2][a2]concat=n=3:v=1:a=1[v][a]"),
            "{graph}"
        );
        assert!(has(&argv, ["-c:v", "libx264"]) && has(&argv, ["-crf", "20"]));
        assert!(has(&argv, ["-c:a", "aac"]) && has(&argv, ["-fps_mode", "cfr"]));
        assert!(p.list().is_none());
    }

    #[test]
    fn a_portrait_clip_among_landscape_ones_gets_bars() {
        let mut b = hd("b.mov");
        b.video.as_mut().unwrap().rotation = 90;
        let p = plan_of(&[hd("a.mp4"), b], "out.mp4");
        assert!(!p.copied);
        assert!(p.notes[0].contains("b.mov is 1080x1920"), "{}", p.notes[0]);
        assert!(p
            .notes
            .contains(&"b.mov is another shape, so black bars fill out the frame.".to_string()));
    }

    #[test]
    fn settings_only_a_copy_cares_about_are_named_as_such() {
        let mut b = hd("b.mp4");
        b.video.as_mut().unwrap().extradata = Some("SHA256:other".into());
        let p = plan_of(&[hd("a.mp4"), b], "out.mp4");
        assert!(!p.copied);
        assert_eq!(
            p.notes[0],
            "Re-encoded to 1920x1080 at 30 fps, as a.mp4 is, since the clips differ: \
             b.mp4 was encoded with other settings."
        );
    }

    #[test]
    fn a_container_that_cannot_hold_the_codec_is_the_reason() {
        let mut a = hd("a.webm");
        a.video.as_mut().unwrap().codec = "vp9".into();
        a.audio.as_mut().unwrap().codec = "opus".into();
        let p = plan_of(&[a], "out.mp4");
        assert!(!p.copied);
        assert_eq!(
            p.notes[0],
            "Re-encoded to 1920x1080 at 30 fps, as a.webm is, since mp4 can't hold VP9 video \
             or Opus sound."
        );
    }

    #[test]
    fn resize_and_crf_force_an_encode_within_the_first_clips_size() {
        let opts = Options {
            resize: Some("1280x".into()),
            crf: Some(28),
            ..Default::default()
        };
        let p = plan(
            &[hd("a.mp4"), hd("b.mp4")],
            Format::Mp4,
            Path::new("out.mp4"),
            &opts,
        )
        .unwrap();
        assert!(!p.copied);
        assert_eq!(p.shape.size, Some((1280, 720)));
        assert_eq!(
            p.notes[0],
            "Re-encoded to 1280x720 at 30 fps, since --resize sets the size and --crf 28 sets \
             the quality."
        );
        assert!(has(&main_argv(&p), ["-crf", "28"]));
        // Never larger than the first clip.
        let big = Options {
            resize: Some("4000x".into()),
            ..Default::default()
        };
        let p = plan(&[hd("a.mp4")], Format::Mp4, Path::new("o.mp4"), &big).unwrap();
        assert_eq!(p.shape.size, Some((1920, 1080)));
    }

    #[test]
    fn knobs_that_do_not_apply_are_refused() {
        let e = plan(
            &[song("a.mp3")],
            Format::Mp3,
            Path::new("o.mp3"),
            &Options {
                crf: Some(20),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            e.message,
            "--crf sets the picture, and o.mp3 holds only sound"
        );
        let e = plan(
            &[hd("a.mp4")],
            Format::Mp4,
            Path::new("o.mp4"),
            &Options {
                crf: Some(60),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            e.message,
            "--crf goes up to 51 for mp4; only webm goes to 63"
        );
    }

    #[test]
    fn a_file_without_a_picture_cannot_join_a_video() {
        let e = plan(
            &[hd("a.mp4"), song("b.mp3")],
            Format::Mp4,
            Path::new("o.mp4"),
            &Options::default(),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(
            e.message.starts_with("b.mp3 has no picture"),
            "{}",
            e.message
        );
    }

    #[test]
    fn songs_are_copied_and_keep_the_tags_they_share() {
        let mut a = song("01.mp3");
        a.tags = vec![
            ("artist".into(), "Band".into()),
            ("title".into(), "One".into()),
            ("encoder".into(), "LAME".into()),
        ];
        a.cover = Some(0);
        let mut b = song("02.mp3");
        b.tags = vec![
            ("title".into(), "Two".into()),
            ("artist".into(), "Band".into()),
            ("encoder".into(), "LAME".into()),
        ];
        let p = plan_of(&[a, b], "album.mp3");
        assert!(p.copied);
        let argv = main_argv(&p);
        assert!(has(&argv, ["-metadata", "artist=Band"]), "{argv:?}");
        assert!(!argv.iter().any(|a| a.starts_with("title=")), "{argv:?}");
        assert!(!argv.iter().any(|a| a.starts_with("encoder=")), "{argv:?}");
        // The first file's cover, from its own input.
        assert!(
            has(&argv, ["-i", "01.mp3"]) && has(&argv, ["-map", "1:v:0"]),
            "{argv:?}"
        );
        assert!(has(&argv, ["-disposition:v:0", "attached_pic"]));
    }

    #[test]
    fn the_sound_of_videos_joins_songs_through_parts_when_track_orders_differ() {
        // A video keeps its sound as track 2, an mp3 as track 1.
        let p = plan_of(&[hd("talk.mp4"), song("outro.mp3")], "all.m4a");
        assert!(!p.copied, "MP3 sound in an m4a is re-encoded");
        let mut b = hd("b.mp4");
        b.audio.as_mut().unwrap().index = 2;
        let p = plan_of(&[hd("a.mp4"), b], "out.mp4");
        assert!(p.copied);
        let cmds = p.commands();
        assert_eq!(cmds.len(), 3, "a part per input, then the join");
        assert_eq!(
            cmds[1],
            [
                "-i",
                "b.mp4",
                "-map",
                "0:v:0",
                "-map",
                "0:a:0",
                "-c",
                "copy",
                "-map_metadata",
                "-1",
                "-y",
                "out.convkit-part-2.mp4"
            ]
        );
        let (_, list) = p.list().unwrap();
        assert!(list.contains("out.convkit-part-1.mp4"), "{list}");
    }

    #[test]
    fn songs_at_different_rates_are_resampled_to_the_first() {
        let mut b = song("b.mp3");
        b.audio.as_mut().unwrap().sample_rate = 48000;
        let p = plan_of(&[song("a.mp3"), b], "out.mp3");
        assert!(!p.copied);
        assert_eq!(
            p.notes[0],
            "Re-encoded to 44.1 kHz stereo, as a.mp3 is, since the files differ: b.mp3 is 48 kHz."
        );
        let argv = main_argv(&p);
        let graph = &argv[argv.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        assert_eq!(
            graph,
            "[0:a:0]aresample=44100,aformat=sample_rates=44100:channel_layouts=stereo[a0];\
             [1:a:0]aresample=44100,aformat=sample_rates=44100:channel_layouts=stereo[a1];\
             [a0][a1]concat=n=2:v=0:a=1[a]"
        );
        assert!(has(&argv, ["-c:a", "libmp3lame"]));
    }

    #[test]
    fn flac_is_always_encoded_and_says_nothing_when_nothing_changes() {
        let f = |p: &str| {
            let mut c = song(p);
            c.audio.as_mut().unwrap().codec = "flac".into();
            c
        };
        let mut b = f("b.flac");
        b.audio.as_mut().unwrap().extradata = Some("SHA256:its own length".into());
        let p = plan_of(&[f("a.flac"), b], "out.flac");
        assert!(!p.copied);
        assert!(p.notes.is_empty(), "{:?}", p.notes);
        let mut b = f("b.flac");
        b.audio.as_mut().unwrap().channels = 1;
        let p = plan_of(&[f("a.flac"), b], "out.flac");
        assert!(
            p.notes[0].starts_with("Converted to 44.1 kHz stereo"),
            "{}",
            p.notes[0]
        );
    }

    #[test]
    fn opus_is_made_at_48_khz_and_ogg_drops_covers() {
        let mut a = song("a.mp3");
        a.cover = Some(0);
        let p = plan_of(&[a, song("b.mp3")], "out.ogg");
        assert_eq!(p.shape.sample_rate, Some(48000));
        assert!(p
            .notes
            .contains(&registry::COVER_ART_DROPPED_NOTE.to_string()));
        assert!(has(&main_argv(&p), ["-c:a", "libopus"]));
    }

    #[test]
    fn mixing_hdr_and_standard_clips_turns_the_hdr_ones() {
        let mut a = hd("a.mov");
        a.video.as_mut().unwrap().hdr = true;
        let p = plan_of(&[a, hd("b.mp4")], "out.mp4");
        assert!(!p.copied);
        let argv = main_argv(&p);
        let graph = &argv[argv.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        assert!(
            graph.starts_with("[0:v:0]scale=out_color_matrix=bt709"),
            "{graph}"
        );
        assert!(!graph.contains("[1:v:0]scale=out_color"), "{graph}");
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn a_single_clip_is_a_copy_with_a_note() {
        let p = plan_of(&[hd("a.mp4")], "out.mp4");
        assert!(p.copied);
        assert_eq!(p.notes, ["Only one input, so out.mp4 is a copy of a.mp4."]);
    }

    #[test]
    fn many_differing_clips_are_counted_after_three() {
        let mut clips = vec![hd("a.mp4")];
        for i in 0..5 {
            clips.push(clip(
                &format!("s{i}.mp4"),
                Some(pic(1920, 1080, (30, 1))),
                None,
            ));
        }
        let p = plan_of(&clips, "out.mp4");
        assert!(
            p.notes[0].ends_with("s2.mp4 has no sound; and 2 more."),
            "{}",
            p.notes[0]
        );
    }

    #[test]
    fn many_videos_are_encoded_a_group_at_a_time_and_joined_by_copy() {
        let mut clips = vec![hd("a.mp4")];
        for i in 0..9 {
            clips.push(clip(
                &format!("s{i}.mp4"),
                Some(pic(1280, 720, (30, 1))),
                Some(sound(48000, 2)),
            ));
        }
        let p = plan_of(&clips, "out.mp4");
        assert!(!p.copied);
        let cmds = p.commands();
        assert_eq!(cmds.len(), 3, "two groups, then the join");
        let inputs = |argv: &[String]| argv.iter().filter(|a| *a == "-i").count();
        assert_eq!((inputs(&cmds[0]), inputs(&cmds[1])), (8, 2));
        assert_eq!(cmds[0].last().unwrap(), "out.convkit-part-1.mp4");
        assert!(
            has(&cmds[1], ["-crf", "20"]),
            "the same settings in each group"
        );
        assert_eq!(
            cmds[2][..6],
            ["-f", "concat", "-safe", "0", "-i", "out.convkit-list.txt"]
        );
        assert!(has(&cmds[2], ["-c", "copy"]));
        let (_, list) = p.list().unwrap();
        assert_eq!(list.lines().count(), 2);
        // Sound alone is light, and stays one encode.
        let songs: Vec<Clip> = (0..10).map(|i| song(&format!("{i}.mp3"))).collect();
        let mut songs = songs;
        songs[1].audio.as_mut().unwrap().sample_rate = 48000;
        assert_eq!(plan_of(&songs, "out.mp3").commands().len(), 1);
    }

    #[test]
    fn scratch_paths_in_ffmpegs_report_name_the_users_files() {
        let mut b = hd("b.mp4");
        b.audio.as_mut().unwrap().index = 2;
        let p = plan_of(&[hd("a.mp4"), b], "dir/out.mp4");
        let s = Path::new("dir/.convkit-1-0");
        let places = p.places(Some(s));
        let report = format!(
            "{}: Invalid data\n{}: error",
            places.parts[1].display(),
            places.output.display()
        );
        assert_eq!(
            hide_scratch(&p, &places, &report),
            "b.mp4: Invalid data\nout.mp4: error"
        );
    }
}
