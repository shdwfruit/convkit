//! `--audio` and `--mix`: a new sound for a video.
//!
//! `--audio song.mp3` replaces the video's sound with the file's, and
//! `--mix` plays it under the sound the video has. The picture is
//! stream-copied wherever nothing else changes it, and a new sound the
//! target holds is copied too; a mix is always encoded.
//!
//! The video's length decides. A new sound that runs longer is stopped
//! where the video ends; one that runs shorter leaves the rest of the video
//! silent, or with its own sound alone in a mix. Nothing loops: a song
//! starting over is a choice, not a default, and silence is what the file
//! held. Both get a note. `--start` and `--end` cut the video, and the new
//! sound starts with the clip; `--speed` changes the video and its own
//! sound, and the new sound plays at its own speed.
//!
//! A mix sets the new sound by loudness rather than by a fixed volume:
//! both are measured with ffmpeg's `ebur128` before the run, and the new
//! sound is put `level` below the video's own, a quarter as loud by
//! default (12 dB under), which sits music under speech whatever either was
//! recorded at. A fixed gain cannot: phone speech sits near -25 LUFS and
//! mastered music near -10, so music turned down by a fixed 12 dB still
//! drowned the voice.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{ConvError, ErrorCode, Result};
use crate::probe::MediaProbe;
use crate::procutil::backend_command;
use crate::{registry, Backend, Format, Kind};

/// How loud the new sound is under the video's own in a mix, in percent of
/// the video's loudness: 25 is a quarter as loud, 12 dB under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level(pub u16);

/// A quarter as loud as the video's own sound: music under speech.
pub const DEFAULT_LEVEL: Level = Level(25);

/// Parses `25%` or `25`: the new sound's loudness against the video's own,
/// from 1% to 400%.
pub fn parse_level(text: &str) -> std::result::Result<Level, String> {
    let digits = text.trim().strip_suffix('%').unwrap_or(text.trim());
    let bad = || {
        format!(
            "invalid mix level '{text}'; expected a percentage of the video's own loudness, \
             1% to 400% (25% is a quarter as loud)"
        )
    };
    if digits.is_empty() || digits.len() > 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    match digits.parse::<u16>() {
        Ok(n) if (1..=400).contains(&n) => Ok(Level(n)),
        _ => Err(bad()),
    }
}

/// The new sound asked for: its file, and whether it is mixed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub path: PathBuf,
    /// `None` replaces the video's sound; `Some` mixes the new one under it.
    pub mix: Option<Level>,
}

/// What was read of the new sound before planning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Read {
    /// The first audio stream's codec.
    pub codec: Option<String>,
    pub duration_ms: Option<u64>,
    /// For a mix, the integrated loudness of the video's own sound and of
    /// the new one, in tenths of a LUFS, measured with `ebur128`. `None`
    /// where it could not be measured, or for a silent track.
    pub loudness: Option<(Option<i32>, Option<i32>)>,
}

/// What `--json` says about a new sound.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioReport {
    pub file: PathBuf,
    /// `replace` or `mix`.
    pub mode: &'static str,
    /// The level asked for, in percent, for a mix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<u16>,
    /// The gain the new sound is mixed at, in dB, once measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain_db: Option<f64>,
    /// Whether the new sound was copied as it is.
    pub copied: bool,
}

// `gain_db` is rounded from a clamped finite number, never NaN, so equality
// is reflexive.
impl Eq for AudioReport {}

/// A new sound resolved for one conversion: the argv it adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Soundtrack {
    pub path: PathBuf,
    /// Stop reading the new sound after this many milliseconds: the
    /// output's length, where the new sound runs longer.
    pub limit_ms: Option<u64>,
    /// The `volume` the new sound is mixed at (`-12.3dB`), for a mix.
    pub mix_gain: Option<String>,
    /// The new sound is copied as it is.
    pub copy: bool,
}

/// The pad a mix comes out of, which the mapping names.
const MIX_PAD: &str = "[convkit_mix]";

impl Soundtrack {
    /// The new sound's input: `-t` when it is to stop early, then `-i`.
    /// Input 1, after the video's.
    pub(crate) fn input_args(&self) -> Vec<String> {
        let mut argv = Vec::new();
        if let Some(ms) = self.limit_ms {
            argv.extend(["-t".to_string(), seconds(ms)]);
        }
        argv.extend(["-i".to_string(), self.path.to_string_lossy().into_owned()]);
        argv
    }

    /// The mix, as a graph over the video's first audio track, through
    /// `own` (a speed change's filters, which only the video's sound
    /// takes), and the new sound at its gain. `duration=first` ends it with
    /// the video's sound; `normalize=0` keeps each at the level set rather
    /// than halving both. `tail` is a filter the target needs after it
    /// (libopus's channel layouts).
    pub(crate) fn mix_graph(&self, own: Option<&str>, tail: Option<&str>) -> Option<String> {
        let gain = self.mix_gain.as_deref()?;
        let own = own.map_or_else(|| "anull".to_string(), str::to_string);
        let tail = tail.map_or_else(String::new, |t| format!(",{t}"));
        Some(format!(
            "[0:a:0]{own}[convkit_own];[1:a:0]volume={gain}[convkit_new];\
             [convkit_own][convkit_new]amix=inputs=2:duration=first:dropout_transition=0:\
             normalize=0{tail}{MIX_PAD}"
        ))
    }

    /// What the output's audio is mapped from.
    pub(crate) fn map(&self) -> [&'static str; 2] {
        if self.mix_gain.is_some() {
            ["-map", MIX_PAD]
        } else {
            ["-map", "1:a:0"]
        }
    }
}

/// Whole milliseconds as ffmpeg's seconds: `62`, `62.5`.
fn seconds(ms: u64) -> String {
    match ms % 1000 {
        0 => format!("{}", ms / 1000),
        f => format!("{}.{}", ms / 1000, format!("{f:03}").trim_end_matches('0')),
    }
}

/// `m:ss` for a note.
fn clock(ms: u64) -> String {
    let s = ms.div_ceil(1000);
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// A new sound resolved for one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub soundtrack: Soundtrack,
    pub notes: Vec<String>,
    pub report: AudioReport,
}

/// Refuses a new sound where it cannot go, then resolves it against
/// `probe`, the source as the output plays it (after any cut and speed
/// change). `None` without `--audio`.
pub(crate) fn resolve(
    tuning: &crate::Tuning,
    from: Format,
    to: Format,
    probe: Option<&MediaProbe>,
    input: &Path,
) -> Result<Option<Resolved>> {
    let Some(request) = tuning.audio.as_ref() else {
        return Ok(None);
    };
    refuse(tuning, from, to)?;
    let file = request.path.display();
    let read = probe.and_then(|p| p.soundtrack.as_ref()).ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("--audio needs ffprobe to read {file}; install ffprobe or check that it runs"),
        )
    })?;
    let probe = probe.expect("read from the probe above");
    if probe.video_streams == 0 && probe.video_codec.is_none() {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "--audio adds sound to a video, and {} has no video stream",
                input.display()
            ),
        ));
    }
    let name = request
        .path
        .file_name()
        .map_or_else(|| file.to_string(), |n| n.to_string_lossy().into_owned());
    let mut notes = Vec::new();
    // A mix needs a sound to go under; without one the new sound is the
    // only sound, as it is when replacing.
    let mix = request.mix.filter(|_| {
        let own = !probe.audio_codecs.is_empty();
        if !own {
            notes.push(format!(
                "{} has no sound of its own, so {name} is added as it is.",
                input.display()
            ));
        }
        own
    });
    let (_, audio_ok) = registry::compat_tables(to).ok_or_else(|| not_applicable(from, to))?;
    let copy = mix.is_none() && read.codec.as_deref().is_some_and(|c| audio_ok.contains(&c));
    let video_ms = probe.duration_ms;
    let limit_ms = match (read.duration_ms, video_ms) {
        (Some(sound), Some(video)) if sound > video + 50 => {
            // A mix ends with the video's own sound; only a replacement
            // needs telling where to stop.
            notes.push(format!(
                "{name} runs {}; it stops where the video ends, at {}.",
                clock(sound),
                clock(video)
            ));
            mix.is_none().then_some(video)
        }
        (Some(sound), Some(video)) if sound + 50 < video => {
            notes.push(match mix {
                Some(_) => format!(
                    "{name} ends at {}; the video's own sound goes on alone to {}.",
                    clock(sound),
                    clock(video)
                ),
                None => format!(
                    "{name} ends at {}, so the video is silent from there to {}.",
                    clock(sound),
                    clock(video)
                ),
            });
            None
        }
        _ => None,
    };
    if mix.is_some() && probe.audio_codecs.len() > 1 {
        notes.push(format!(
            "{name} is mixed with the first of {} audio tracks; the others are left out.",
            probe.audio_codecs.len()
        ));
    }
    if tuning.speed.is_some() {
        notes.push(format!(
            "--speed changes the video and its own sound; {name} plays at its own speed."
        ));
    }
    let gain_db = mix.map(|level| gain_db(level, read.loudness));
    if !copy && mix.is_none() {
        let codec = read.codec.as_deref().unwrap_or("its codec");
        let into = if to == Format::Webm { "opus" } else { "aac" };
        notes.push(format!(
            "{name} is re-encoded to {into}; {} cannot hold {codec}.",
            to.ext()
        ));
    }
    let soundtrack = Soundtrack {
        path: request.path.clone(),
        limit_ms,
        mix_gain: gain_db.map(|g| format!("{g:.1}dB")),
        copy,
    };
    let report = AudioReport {
        file: request.path.clone(),
        mode: if mix.is_some() { "mix" } else { "replace" },
        level: mix.map(|l| l.0),
        gain_db,
        copied: copy,
    };
    Ok(Some(Resolved {
        soundtrack,
        notes,
        report,
    }))
}

/// The gain that puts the new sound `level` below the video's own, from
/// their measured loudness: `level` of the new sound's own volume where
/// either could not be measured, or the video's is silence.
fn gain_db(level: Level, loudness: Option<(Option<i32>, Option<i32>)>) -> f64 {
    let under = 20.0 * (f64::from(level.0) / 100.0).log10();
    let gain = match loudness {
        Some((Some(own), Some(new))) if own > -600 && new > -600 => {
            f64::from(own - new) / 10.0 + under
        }
        _ => under,
    };
    (gain.clamp(-60.0, 30.0) * 10.0).round() / 10.0
}

/// The refusals that hold whatever the probe: a target that holds no
/// sound and picture, a source with no video to put it on, and the flags
/// that ask for something else of the sound.
pub(crate) fn refuse(tuning: &crate::Tuning, from: Format, to: Format) -> Result<()> {
    if tuning.audio.is_none() {
        return Ok(());
    }
    if !crate::sized::is_video_target(to) || from.kind() != Kind::Video {
        return Err(not_applicable(from, to));
    }
    let invalid = |m: &str| Err(ConvError::new(ErrorCode::InvalidInvocation, m.to_string()));
    if tuning.mute {
        return invalid("a silent clip has no sound to replace or mix; leave out --audio");
    }
    if tuning.max_size.is_some() {
        return invalid(
            "--max-size cannot size a video with new audio yet; add the audio first, then size \
             the result",
        );
    }
    Ok(())
}

/// Whether a video of `f` can take a new sound into its own format; `Err`
/// says what to do instead.
pub fn in_place(f: Format) -> std::result::Result<(), String> {
    if crate::sized::is_video_target(f) {
        return Ok(());
    }
    if f.kind() == Kind::Video {
        return Err(format!(
            "conv cannot keep {0} files as {0}; add --to mp4 to put the audio on them",
            f.ext()
        ));
    }
    Err(not_applicable(f, f).message)
}

/// The refusal of `--audio` on a pair it cannot go on.
pub(crate) fn not_applicable(from: Format, to: Format) -> ConvError {
    let why = if from == Format::Gif {
        "a gif has no sound to replace; convert it to mp4 first, then add the audio".to_string()
    } else {
        "it adds sound to a video, into mp4, mov, mkv or webm".to_string()
    };
    ConvError::new(
        ErrorCode::InvalidInvocation,
        format!(
            "--audio does not apply to {} -> {}: {why}",
            from.ext(),
            to.ext()
        ),
    )
}

/// Reads the new sound before planning: its codec and length, and for a
/// mix how loud it and the video's own sound are. The video's probe must
/// already be in `probed`; without one there is nothing to put it on, and
/// the plan says so. Shared by `exec::run` and the `--dry-run` preview.
pub fn attach(
    resolver: &crate::Resolver,
    from: Format,
    to: Format,
    input: &Path,
    tuning: &crate::Tuning,
    probed: &mut Option<MediaProbe>,
) -> Result<()> {
    let Some(request) = tuning.audio.as_ref() else {
        return Ok(());
    };
    refuse(tuning, from, to)?;
    let Some(video) = probed.as_mut() else {
        return Ok(());
    };
    if !request.path.is_file() {
        return Err(ConvError::new(
            ErrorCode::InputNotFound,
            format!("audio file not found: {}", request.path.display()),
        ));
    }
    let ffprobe = resolver.resolve(Backend::Ffprobe)?;
    let sound = crate::probe::run(&ffprobe.path, &request.path)?;
    let Some(codec) = sound.audio_codec().map(str::to_string) else {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!("{} has no sound to add", request.path.display()),
        ));
    };
    let loudness = match request.mix {
        Some(_) if !video.audio_codecs.is_empty() => {
            let ffmpeg = resolver.resolve(Backend::Ffmpeg)?;
            Some((
                loudness(&ffmpeg.path, input),
                loudness(&ffmpeg.path, &request.path),
            ))
        }
        _ => None,
    };
    video.soundtrack = Some(Read {
        codec: Some(codec),
        duration_ms: sound.duration_ms,
        loudness,
    });
    Ok(())
}

/// The integrated loudness of `path`'s first audio track, in tenths of a
/// LUFS: ffmpeg's `ebur128` summary. `None` when it cannot be read; a
/// silent track reads as -70.
fn loudness(ffmpeg: &Path, path: &Path) -> Option<i32> {
    let out = backend_command(ffmpeg)
        .args(["-hide_banner", "-nostats", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:a:0",
            "-af",
            "ebur128=framelog=quiet",
            "-f",
            "null",
            "-",
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| parse_loudness(&String::from_utf8_lossy(&out.stderr)))
        .flatten()
}

/// The `I:` line of an `ebur128` summary, in tenths of a LUFS.
fn parse_loudness(log: &str) -> Option<i32> {
    let summary = &log[log.rfind("Summary:")?..];
    let line = summary.lines().find(|l| l.trim_start().starts_with("I:"))?;
    let value: f64 = line
        .trim_start()
        .trim_start_matches("I:")
        .trim()
        .trim_end_matches("LUFS")
        .trim()
        .parse()
        .ok()?;
    Some((value * 10.0).round() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(secs: u64) -> MediaProbe {
        MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            audio_codecs: vec!["aac".into()],
            duration_ms: Some(secs * 1000),
            ..MediaProbe::default()
        }
    }

    fn with_sound(mut p: MediaProbe, codec: &str, secs: u64) -> MediaProbe {
        p.soundtrack = Some(Read {
            codec: Some(codec.into()),
            duration_ms: Some(secs * 1000),
            loudness: Some((Some(-250), Some(-100))),
        });
        p
    }

    fn tuning(mix: Option<Level>) -> crate::Tuning {
        crate::Tuning {
            audio: Some(Request {
                path: PathBuf::from("music/song.mp3"),
                mix,
            }),
            ..crate::Tuning::default()
        }
    }

    fn resolved(t: &crate::Tuning, to: Format, p: &MediaProbe) -> Resolved {
        resolve(t, Format::Mp4, to, Some(p), Path::new("clip.mp4"))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn a_level_parses_as_a_percentage() {
        assert_eq!(parse_level("25%"), Ok(Level(25)));
        assert_eq!(parse_level("100"), Ok(Level(100)));
        for bad in ["0", "401%", "", "%", "1.5", "-5", "abc"] {
            assert!(parse_level(bad).is_err(), "{bad}");
        }
    }

    /// A song longer than the video stops where it ends; one shorter
    /// leaves the rest silent, and says so.
    #[test]
    fn the_videos_length_decides() {
        let r = resolved(
            &tuning(None),
            Format::Mp4,
            &with_sound(video(60), "mp3", 200),
        );
        assert_eq!(r.soundtrack.limit_ms, Some(60_000));
        assert!(r.soundtrack.copy, "mp4 holds mp3");
        assert_eq!(
            r.soundtrack.input_args(),
            ["-t", "60", "-i", "music/song.mp3"]
        );
        assert_eq!(
            r.notes,
            ["song.mp3 runs 3:20; it stops where the video ends, at 1:00."]
        );
        let r = resolved(
            &tuning(None),
            Format::Mp4,
            &with_sound(video(60), "mp3", 20),
        );
        assert_eq!(r.soundtrack.limit_ms, None);
        assert_eq!(
            r.notes,
            ["song.mp3 ends at 0:20, so the video is silent from there to 1:00."]
        );
        // Of the same length, near enough, nothing is said.
        let r = resolved(
            &tuning(None),
            Format::Mp4,
            &with_sound(video(60), "aac", 60),
        );
        assert!(r.notes.is_empty(), "{:?}", r.notes);
    }

    #[test]
    fn a_sound_the_target_cannot_hold_is_reencoded() {
        let r = resolved(
            &tuning(None),
            Format::Webm,
            &with_sound(video(10), "mp3", 10),
        );
        assert!(!r.soundtrack.copy);
        assert_eq!(
            r.notes,
            ["song.mp3 is re-encoded to opus; webm cannot hold mp3."]
        );
        assert_eq!(r.report.mode, "replace");
    }

    /// A mix puts the new sound a quarter as loud as the video's own: 15
    /// LU louder to begin with, it goes 15 dB down and 12 more.
    #[test]
    fn a_mix_is_set_by_loudness() {
        let r = resolved(
            &tuning(Some(DEFAULT_LEVEL)),
            Format::Mp4,
            &with_sound(video(10), "mp3", 10),
        );
        assert_eq!(r.soundtrack.mix_gain.as_deref(), Some("-27.0dB"));
        assert!(!r.soundtrack.copy);
        assert_eq!(r.report.gain_db, Some(-27.0));
        let graph = r.soundtrack.mix_graph(Some("atempo=2"), None).unwrap();
        assert_eq!(
            graph,
            "[0:a:0]atempo=2[convkit_own];[1:a:0]volume=-27.0dB[convkit_new];\
             [convkit_own][convkit_new]amix=inputs=2:duration=first:dropout_transition=0:\
             normalize=0[convkit_mix]"
        );
        assert_eq!(r.soundtrack.map(), ["-map", "[convkit_mix]"]);
        // Unmeasured, the level is of the new sound's own volume.
        assert_eq!(gain_db(Level(25), None), -12.0);
        assert_eq!(gain_db(Level(100), Some((Some(-700), Some(-100)))), 0.0);
    }

    #[test]
    fn a_mix_into_a_video_with_no_sound_adds_it_as_it_is() {
        let silent = MediaProbe {
            audio_codecs: Vec::new(),
            ..with_sound(video(10), "aac", 10)
        };
        let r = resolved(&tuning(Some(DEFAULT_LEVEL)), Format::Mp4, &silent);
        assert_eq!(r.soundtrack.mix_gain, None);
        assert!(r.soundtrack.copy);
        assert!(
            r.notes[0].contains("has no sound of its own"),
            "{:?}",
            r.notes
        );
    }

    #[test]
    fn audio_is_refused_where_it_cannot_go() {
        let p = with_sound(video(10), "aac", 10);
        for (from, to) in [
            (Format::Mp4, Format::Gif),
            (Format::Mp4, Format::M4a),
            (Format::Gif, Format::Mp4),
            (Format::Png, Format::Jpg),
        ] {
            let e = resolve(&tuning(None), from, to, Some(&p), Path::new("in")).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidInvocation, "{from:?} -> {to:?}");
            assert!(e.message.starts_with("--audio does not apply"), "{e}");
        }
        let sized = crate::Tuning {
            max_size: Some(crate::size::parse("5mb").unwrap()),
            ..tuning(None)
        };
        let e = resolve(&sized, Format::Mp4, Format::Mp4, Some(&p), Path::new("in")).unwrap_err();
        assert!(e.message.starts_with("--max-size cannot size"), "{e}");
    }

    #[test]
    fn the_ebur128_summary_gives_the_integrated_loudness() {
        let log = "[Parsed_ebur128_0 @ 0x1] Summary:\n\n  Integrated loudness:\n    I:         \
                   -22.2 LUFS\n    Threshold: -32.2 LUFS\n";
        assert_eq!(parse_loudness(log), Some(-222));
        assert_eq!(parse_loudness("no summary"), None);
    }

    #[test]
    fn times_read_as_ffmpeg_and_people_write_them() {
        assert_eq!(seconds(62_000), "62");
        assert_eq!(seconds(62_500), "62.5");
        assert_eq!(seconds(7_050), "7.05");
        assert_eq!(clock(200_000), "3:20");
        assert_eq!(clock(3_725_000), "1:02:05");
    }
}
