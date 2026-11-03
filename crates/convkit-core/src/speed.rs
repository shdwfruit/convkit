//! `--speed` and `--shift-pitch`: a factor as typed, resolved against a
//! probed source into the filters ffmpeg runs, with the notes and file names
//! that go with it. Pure: no process, no filesystem.
//!
//! A factor is held in whole thousandths, parsed by digit string rather than
//! through a float, for the same reason `trim` holds milliseconds: `0.3`
//! must print back as `0.3`, in the argv and in a file name.

use std::path::Path;

use serde::{Serialize, Serializer};

use crate::probe::MediaProbe;
use crate::{ConvError, ErrorCode, Format};

/// A speed factor as typed: `2`, `0.5`, `1.25x`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Speed {
    thousandths: u32,
}

const FORMS: &str = "expected a number such as 2, 0.5 or 1.25x";

/// Parses `2`, `2x`, `0.5`, `1.25X`: a number with at most three decimal
/// places, optionally followed by an `x`, from 0.01 to 100. 100 is the most
/// ffmpeg's `atempo` takes in one step; 0.01 already makes a file a hundred
/// times longer. 1 is refused: it is the speed the file already has.
pub fn parse(text: &str) -> Result<Speed, String> {
    let bad = || format!("invalid speed '{text}'; {FORMS}");
    let body = text
        .strip_suffix('x')
        .or_else(|| text.strip_suffix('X'))
        .unwrap_or(text);
    let (whole, frac) = match body.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (body, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || frac.is_some_and(|f| !digits(f)) {
        return Err(bad());
    }
    // Trailing zeros say nothing: `1.5000` is 1.5.
    let frac = frac.map_or("", |f| f.trim_end_matches('0'));
    if frac.len() > 3 {
        return Err(format!(
            "invalid speed '{text}'; give at most three decimal places"
        ));
    }
    let out_of_range = || format!("invalid speed '{text}'; conv takes 0.01 to 100");
    let whole: u32 = whole.parse().map_err(|_| out_of_range())?;
    let frac: u32 = format!("{frac:0<3}").parse().expect("three ascii digits");
    let thousandths = whole
        .checked_mul(1000)
        .and_then(|w| w.checked_add(frac))
        .ok_or_else(out_of_range)?;
    if !(10..=100_000).contains(&thousandths) {
        return Err(out_of_range());
    }
    if thousandths == 1000 {
        return Err(format!(
            "invalid speed '{text}'; 1 is the speed it already plays at"
        ));
    }
    Ok(Speed { thousandths })
}

/// Thousandths as a decimal without trailing zeros: `2`, `0.5`, `0.64`.
fn decimal(thousandths: u32) -> String {
    match thousandths % 1000 {
        0 => format!("{}", thousandths / 1000),
        f => format!(
            "{}.{}",
            thousandths / 1000,
            format!("{f:03}").trim_end_matches('0')
        ),
    }
}

impl Speed {
    /// The factor as conv prints it, in the syntax `--speed` reads back:
    /// `2`, `0.5`, `1.25`.
    pub fn text(&self) -> String {
        decimal(self.thousandths)
    }

    /// What an output adds to its name: `2x`, `0.5x`.
    pub fn suffix(&self) -> String {
        format!("{}x", self.text())
    }

    /// Whether this slows the source down.
    pub fn slows(&self) -> bool {
        self.thousandths < 1000
    }

    /// The video filter: every frame's timestamp divided by the factor.
    pub fn setpts(&self) -> String {
        format!("setpts=PTS/{}", self.text())
    }

    /// The audio filter that keeps the pitch: `atempo`, chained in halves
    /// below 0.5, the least one `atempo` takes. Each half doubles the
    /// thousandths left, so the chain stays exact.
    pub fn atempo(&self) -> String {
        let mut steps = Vec::new();
        let mut left = self.thousandths;
        while left < 500 {
            steps.push("atempo=0.5".to_string());
            left *= 2;
        }
        steps.push(format!("atempo={}", decimal(left)));
        steps.join(",")
    }

    /// The audio filter that lets the pitch move with the speed, as a tape
    /// played fast does: the samples are relabelled at `rate` times the
    /// factor, then resampled back to `rate`. The first `aresample` brings a
    /// second track at another rate to `rate` before it is relabelled.
    pub fn asetrate(&self, rate: u32) -> String {
        let relabelled = (u64::from(rate) * u64::from(self.thousandths) + 500) / 1000;
        format!("aresample={rate},asetrate={relabelled},aresample={rate}")
    }

    /// Something of the source that runs with its time, its length in
    /// milliseconds or its size in bytes, played at this speed.
    pub fn scale(&self, len: u64) -> u64 {
        let th = u128::from(self.thousandths);
        u64::try_from((u128::from(len) * 1000 + th / 2) / th).unwrap_or(u64::MAX)
    }
}

impl Serialize for Speed {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(f64::from(self.thousandths) / 1000.0)
    }
}

/// The speed change resolved for one source: what an ffmpeg invocation adds
/// for it. Carried on `ResolvedVideo`, as the cut is, because every ffmpeg
/// invocation is built from that value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retime {
    pub speed: Speed,
    /// The audio filters, for `-af`; `None` when the output has no sound.
    pub audio: Option<String>,
    /// The source's frame rate, `n/d`, which a video target is held to
    /// when nothing else sets one (`VideoChainSpec::compose`); for a GIF,
    /// `GIF_MAX_FPS` when the new speed would pass it. Left to
    /// itself, ffmpeg repeats the frames of a slowed mp4 on 6.1 but shows
    /// each one longer on 9.0, and in mkv and webm on both, and it writes
    /// one frame too many when speeding up. `None` for a target with no
    /// picture, or a rate the probe could not read.
    pub rate: Option<String>,
}

/// What `--json` says about a speed change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpeedReport {
    pub factor: Speed,
    pub shift_pitch: bool,
}

/// A speed change resolved for one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retimed {
    pub retime: Retime,
    /// The probe as everything after the speed change should see it: the
    /// output's length, and the source's size scaled to it. The
    /// `--max-size` budget and the GIF memory note read these.
    pub probe: MediaProbe,
    pub notes: Vec<String>,
    pub report: SpeedReport,
}

/// The sample rate the sound is brought to when the source's own could not
/// be read: Opus's own, and the most common rate in video.
const FALLBACK_RATE: u32 = 48_000;

/// The fastest a GIF plays: a frame lasts a whole number of hundredths of a
/// second, and browsers show one that asks for less than two for ten.
const GIF_MAX_FPS: u32 = 50;

/// Refuses a speed change on a pair with no timeline, then resolves it
/// against `probe`, the source as it stands after any cut. The one entry
/// point both `plan::build_tuned` and `sized::prepare` use.
pub fn retime(
    from: Format,
    to: Format,
    speed: Speed,
    shift_pitch: bool,
    mute: bool,
    probe: Option<&MediaProbe>,
    input: &Path,
) -> crate::Result<Retimed> {
    crate::trim::refuse_without_timeline(
        "--speed",
        "changes the speed of video and audio",
        from,
        to,
    )?;
    let probe = probe.ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!(
                "--speed needs ffprobe to read {}; install ffprobe or check that it runs",
                input.display()
            ),
        )
    })?;
    let mut notes = Vec::new();
    // A probe that saw no stream at all failed to read the file, so its
    // sound is unknown rather than absent, and is filtered all the same.
    let saw_nothing = probe.video_streams == 0 && probe.audio_codecs.is_empty();
    let has_sound = to != Format::Gif && !mute && (saw_nothing || !probe.audio_codecs.is_empty());
    let audio = has_sound.then(|| match (shift_pitch, probe.audio_sample_rate) {
        (false, _) => speed.atempo(),
        (true, Some(rate)) => speed.asetrate(rate),
        (true, None) => {
            notes.push(
                "Source sample rate could not be read, so the sound was resampled to 48 kHz \
                 to shift its pitch."
                    .to_string(),
            );
            speed.asetrate(FALLBACK_RATE)
        }
    });
    if shift_pitch && !has_sound {
        notes.push("The output has no sound, so --shift-pitch changed nothing.".to_string());
    }
    // ffmpeg would carry them at their old times, so `output_args` drops
    // them. A GIF carries neither, and an audio target no subtitles.
    let video_target = crate::sized::is_video_target(to);
    let left_out = match (
        video_target && !probe.subtitle_codecs.is_empty(),
        to != Format::Gif && probe.chapters > 0,
    ) {
        (true, true) => Some("Subtitles and chapters are"),
        (true, false) => Some("Subtitles are"),
        (false, true) => Some("Chapters are"),
        (false, false) => None,
    };
    if let Some(what) = left_out {
        notes.push(format!(
            "{what} left out, since their times would not follow the new speed."
        ));
    }
    if speed.slows() && video_target && probe.video_streams > 0 {
        notes.push(format!(
            "Video slowed to {} repeats frames rather than making new ones, so its motion \
             is less smooth than the source's.",
            speed.suffix()
        ));
    }
    if speed.slows() && to == Format::Gif && probe.video_streams > 0 {
        notes.push(format!(
            "GIF slowed to {} shows each frame for longer rather than making new ones, so \
             its motion is less smooth than the source's.",
            speed.suffix()
        ));
    }
    let rate = if to == Format::Gif {
        // A GIF times each frame on its own, so a slowed one just shows
        // each frame for longer. Sped up, it is held to the fastest rate
        // browsers play: they show a frame meant for under 2/100 s for
        // 10/100 s, so a fast GIF would come out slower than its source.
        let fast = probe.frame_rate.is_some_and(|(n, d)| {
            u64::from(n) * u64::from(speed.thousandths)
                > u64::from(GIF_MAX_FPS) * 1000 * u64::from(d)
        });
        // Only a GIF source keeps its rate by default; any other is capped
        // at 15 fps by `TO_GIF_CHAIN` before this could bind.
        if fast && from == Format::Gif {
            notes.push(format!(
                "Sped up to {}, the GIF would play faster than {GIF_MAX_FPS} fps, the most \
                 browsers show, so some frames were dropped to keep its speed.",
                speed.suffix()
            ));
        }
        fast.then(|| GIF_MAX_FPS.to_string())
    } else {
        probe
            .frame_rate
            .filter(|_| video_target)
            .map(|(n, d)| format!("{n}/{d}"))
    };
    let mut scaled = probe.clone();
    scaled.duration_ms = probe.duration_ms.map(|d| speed.scale(d));
    scaled.size_bytes = probe.size_bytes.map(|b| speed.scale(b));
    Ok(Retimed {
        retime: Retime { speed, audio, rate },
        probe: scaled,
        notes,
        report: SpeedReport {
            factor: speed,
            shift_pitch,
        },
    })
}

/// The output options a cut or a speed change adds just before the output
/// path. A speed change drops the subtitles (`-sn` holds even where they are
/// mapped by name, 6.1 and 9.0 alike) and the chapters, whose times would
/// not follow it; with them gone, a cut's subtitle option has nothing left
/// to act on.
pub fn output_args(cut: Option<&crate::trim::Cut>, retime: Option<&Retime>) -> Vec<String> {
    match (retime, cut) {
        (Some(_), _) => ["-sn", "-map_chapters", "-1"].map(String::from).to_vec(),
        (None, Some(cut)) => cut.output_args(),
        (None, None) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sp(s: &str) -> Speed {
        parse(s).unwrap()
    }

    #[test]
    fn every_form_parses_to_whole_thousandths() {
        for (text, thousandths) in [
            ("2", 2000),
            ("2x", 2000),
            ("2X", 2000),
            ("0.5", 500),
            ("0.5x", 500),
            ("1.25", 1250),
            ("0.333", 333),
            ("0.01", 10),
            ("100", 100_000),
            ("2.000", 2000),
            ("1.5000", 1500),
        ] {
            assert_eq!(
                parse(text).map(|s| s.thousandths),
                Ok(thousandths),
                "{text}"
            );
        }
    }

    #[test]
    fn malformed_factors_are_refused_with_the_forms_that_work() {
        for text in [
            "", "x", "fast", "-2", "2xx", "x2", ".5", "2.", "1,5", "2 x", "1/2", "50%",
        ] {
            let e = parse(text).unwrap_err();
            assert_eq!(e, format!("invalid speed '{text}'; {FORMS}"), "{text}");
        }
    }

    #[test]
    fn a_factor_outside_what_conv_takes_is_refused_naming_the_bounds() {
        for text in ["0.009", "0", "0.001", "100.001", "200", "99999999999"] {
            let e = parse(text).unwrap_err();
            assert_eq!(
                e,
                format!("invalid speed '{text}'; conv takes 0.01 to 100"),
                "{text}"
            );
        }
    }

    #[test]
    fn more_than_three_decimal_places_is_refused() {
        assert_eq!(
            parse("0.3333").unwrap_err(),
            "invalid speed '0.3333'; give at most three decimal places"
        );
    }

    #[test]
    fn the_speed_a_file_already_has_is_refused() {
        for text in ["1", "1x", "1.0", "1.000"] {
            assert_eq!(
                parse(text).unwrap_err(),
                format!("invalid speed '{text}'; 1 is the speed it already plays at"),
                "{text}"
            );
        }
    }

    #[test]
    fn a_factor_prints_and_names_the_way_it_reads_back() {
        for (text, printed, suffix) in [
            ("2x", "2", "2x"),
            ("0.5", "0.5", "0.5x"),
            ("1.250", "1.25", "1.25x"),
            ("0.01", "0.01", "0.01x"),
            ("100", "100", "100x"),
        ] {
            assert_eq!(sp(text).text(), printed, "{text}");
            assert_eq!(sp(text).suffix(), suffix, "{text}");
            assert_eq!(parse(&sp(text).text()), Ok(sp(text)), "{text} round trips");
        }
    }

    #[test]
    fn only_a_factor_under_one_slows() {
        assert!(sp("0.5").slows());
        assert!(sp("0.999").slows());
        assert!(!sp("1.001").slows());
        assert!(!sp("2").slows());
    }

    #[test]
    fn the_video_filter_divides_every_timestamp() {
        assert_eq!(sp("2").setpts(), "setpts=PTS/2");
        assert_eq!(sp("0.5").setpts(), "setpts=PTS/0.5");
    }

    #[test]
    fn atempo_is_one_step_from_a_half_up_and_halves_below() {
        assert_eq!(sp("2").atempo(), "atempo=2");
        assert_eq!(sp("100").atempo(), "atempo=100");
        assert_eq!(sp("0.5").atempo(), "atempo=0.5");
        assert_eq!(sp("0.25").atempo(), "atempo=0.5,atempo=0.5");
        assert_eq!(sp("0.3").atempo(), "atempo=0.5,atempo=0.6");
        // 0.5^6 * 0.64 = 0.01.
        assert_eq!(
            sp("0.01").atempo(),
            "atempo=0.5,atempo=0.5,atempo=0.5,atempo=0.5,atempo=0.5,atempo=0.5,atempo=0.64"
        );
    }

    #[test]
    fn asetrate_relabels_at_the_factor_and_resamples_back() {
        assert_eq!(
            sp("2").asetrate(44100),
            "aresample=44100,asetrate=88200,aresample=44100"
        );
        assert_eq!(
            sp("0.5").asetrate(48000),
            "aresample=48000,asetrate=24000,aresample=48000"
        );
        // 44100 * 0.333 = 14685.3, to the nearest sample.
        assert_eq!(
            sp("0.333").asetrate(44100),
            "aresample=44100,asetrate=14685,aresample=44100"
        );
    }

    #[test]
    fn a_length_is_divided_by_the_factor_to_the_nearest_millisecond() {
        assert_eq!(sp("2").scale(10_000), 5_000);
        assert_eq!(sp("0.5").scale(10_000), 20_000);
        assert_eq!(sp("3").scale(10_000), 3_333);
        assert_eq!(sp("0.01").scale(u64::MAX / 10), u64::MAX);
    }

    #[test]
    fn json_carries_the_factor_as_a_number() {
        assert_eq!(serde_json::to_string(&sp("2")).unwrap(), "2.0");
        assert_eq!(serde_json::to_string(&sp("0.25")).unwrap(), "0.25");
    }

    // --- Resolving against a source ------------------------------------------

    /// Ten seconds of 30 fps video with one 44.1 kHz audio track, 10 MB.
    fn clip() -> MediaProbe {
        MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            audio_codecs: vec!["aac".into()],
            audio_bitrates: vec![None],
            audio_sample_rate: Some(44_100),
            frame_rate: Some((30, 1)),
            duration_ms: Some(10_000),
            size_bytes: Some(10_000_000),
            ..MediaProbe::default()
        }
    }

    fn at(to: Format, speed: &str, shift_pitch: bool, probe: &MediaProbe) -> Retimed {
        retime(
            Format::Mp4,
            to,
            sp(speed),
            shift_pitch,
            false,
            Some(probe),
            Path::new("a.mp4"),
        )
        .unwrap()
    }

    #[test]
    fn a_still_image_has_no_speed_to_change() {
        let e = retime(
            Format::Mp4,
            Format::Png,
            sp("2"),
            false,
            false,
            None,
            Path::new("a.mp4"),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "--speed does not apply to mp4 -> png: png is a still image; \
             --speed changes the speed of video and audio"
        );
    }

    #[test]
    fn a_speed_change_needs_the_probe() {
        let e = retime(
            Format::Mp4,
            Format::Mp4,
            sp("2"),
            false,
            false,
            None,
            Path::new("a.mp4"),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::ConversionFailed);
        assert_eq!(
            e.message,
            "--speed needs ffprobe to read a.mp4; install ffprobe or check that it runs"
        );
    }

    #[test]
    fn the_output_is_as_long_as_the_source_over_the_factor() {
        let r = at(Format::Mp4, "2", false, &clip());
        assert_eq!(r.retime.speed, sp("2"));
        assert_eq!(r.retime.audio.as_deref(), Some("atempo=2"));
        assert_eq!(r.probe.duration_ms, Some(5_000));
        assert_eq!(r.probe.size_bytes, Some(5_000_000));
        assert_eq!(r.probe.frame_rate, Some((30, 1)), "the frame rate stays");
        assert_eq!(
            r.report,
            SpeedReport {
                factor: sp("2"),
                shift_pitch: false
            }
        );
        assert!(r.notes.is_empty(), "{:?}", r.notes);
    }

    /// The output keeps the source's rate, so a video target is held to
    /// it: ffmpeg otherwise keeps every frame of a slowed mp4 on 9.0 but
    /// repeats them on 6.1, and writes a frame too many when speeding up.
    #[test]
    fn a_picture_target_is_held_to_the_sources_frame_rate() {
        assert_eq!(
            at(Format::Mp4, "2", false, &clip()).retime.rate.as_deref(),
            Some("30/1")
        );
        assert_eq!(at(Format::M4a, "2", false, &clip()).retime.rate, None);
        let unread = MediaProbe {
            frame_rate: None,
            ..clip()
        };
        assert_eq!(at(Format::Mp4, "2", false, &unread).retime.rate, None);
    }

    /// A GIF holds each frame for as long as it says, so it is only held
    /// to a rate where the new speed would pass the fastest browsers play.
    #[test]
    fn a_gif_is_held_to_fifty_fps_only_past_it() {
        let gif = MediaProbe {
            frame_rate: Some((25, 1)),
            ..clip()
        };
        let gif_at = |speed| {
            retime(
                Format::Gif,
                Format::Gif,
                sp(speed),
                false,
                false,
                Some(&gif),
                Path::new("a.gif"),
            )
            .unwrap()
        };
        let slowed = gif_at("0.5");
        assert_eq!(slowed.retime.rate, None);
        assert!(slowed.notes[0].starts_with("GIF slowed to 0.5x shows each frame"));
        assert_eq!(gif_at("2").retime.rate, None);
        let fast = gif_at("3");
        assert_eq!(fast.retime.rate.as_deref(), Some("50"));
        assert_eq!(
            fast.notes,
            [
                "Sped up to 3x, the GIF would play faster than 50 fps, the most browsers \
              show, so some frames were dropped to keep its speed."
            ]
        );
    }

    #[test]
    fn a_source_of_unknown_length_stays_unknown() {
        let probe = MediaProbe {
            duration_ms: None,
            size_bytes: None,
            ..clip()
        };
        let r = at(Format::Mp4, "2", false, &probe);
        assert_eq!((r.probe.duration_ms, r.probe.size_bytes), (None, None));
    }

    #[test]
    fn shift_pitch_resamples_at_the_sources_rate() {
        let r = at(Format::M4a, "2", true, &clip());
        assert_eq!(
            r.retime.audio.as_deref(),
            Some("aresample=44100,asetrate=88200,aresample=44100")
        );
        assert!(r.report.shift_pitch);
        assert!(r.notes.is_empty(), "{:?}", r.notes);
    }

    #[test]
    fn shift_pitch_on_an_unread_rate_resamples_to_48k_and_says_so() {
        let probe = MediaProbe {
            audio_sample_rate: None,
            ..clip()
        };
        let r = at(Format::M4a, "2", true, &probe);
        assert_eq!(
            r.retime.audio.as_deref(),
            Some("aresample=48000,asetrate=96000,aresample=48000")
        );
        assert_eq!(
            r.notes,
            [
                "Source sample rate could not be read, so the sound was resampled to 48 kHz \
              to shift its pitch."
            ]
        );
    }

    #[test]
    fn an_output_without_sound_gets_no_audio_filter() {
        let silent = MediaProbe {
            audio_codecs: Vec::new(),
            audio_bitrates: Vec::new(),
            audio_sample_rate: None,
            ..clip()
        };
        assert_eq!(at(Format::Gif, "2", false, &clip()).retime.audio, None);
        assert_eq!(at(Format::Mp4, "2", false, &silent).retime.audio, None);
        let muted = retime(
            Format::Mp4,
            Format::Mp4,
            sp("2"),
            false,
            true,
            Some(&clip()),
            Path::new("a.mp4"),
        )
        .unwrap();
        assert_eq!(muted.retime.audio, None);
    }

    #[test]
    fn shift_pitch_with_no_sound_to_shift_says_it_changed_nothing() {
        let r = at(Format::Gif, "2", true, &clip());
        assert_eq!(r.retime.audio, None);
        assert_eq!(
            r.notes,
            ["The output has no sound, so --shift-pitch changed nothing."]
        );
    }

    /// An empty probe is one that failed to read the file: its sound is
    /// unknown, not absent, so it is filtered all the same. ffmpeg ignores
    /// an audio filter when there is no audio (6.1 and 9.0 alike).
    #[test]
    fn a_probe_that_saw_nothing_still_filters_the_sound() {
        let r = at(Format::Mp4, "2", false, &MediaProbe::default());
        assert_eq!(r.retime.audio.as_deref(), Some("atempo=2"));
    }

    #[test]
    fn subtitles_and_chapters_are_left_out_and_that_is_said() {
        let both = MediaProbe {
            subtitle_codecs: vec!["mov_text".into()],
            chapters: 3,
            ..clip()
        };
        let note = |to, probe: &MediaProbe| at(to, "2", false, probe).notes;
        assert_eq!(
            note(Format::Mp4, &both),
            [
                "Subtitles and chapters are left out, since their times would not follow \
              the new speed."
            ]
        );
        let subs = MediaProbe {
            chapters: 0,
            ..both.clone()
        };
        assert_eq!(
            note(Format::Mkv, &subs),
            ["Subtitles are left out, since their times would not follow the new speed."]
        );
        // An audio target never carries subtitles, but can carry chapters.
        assert_eq!(
            note(Format::M4a, &both),
            ["Chapters are left out, since their times would not follow the new speed."]
        );
        assert!(note(Format::M4a, &subs).is_empty());
        assert!(note(Format::Gif, &both).is_empty(), "a GIF carries neither");
    }

    #[test]
    fn slowed_video_says_its_frames_repeat() {
        let note = "Video slowed to 0.5x repeats frames rather than making new ones, so its \
                    motion is less smooth than the source's.";
        assert_eq!(at(Format::Mp4, "0.5", false, &clip()).notes, [note]);
        assert_eq!(
            at(Format::Gif, "0.5", false, &clip()).notes,
            [
                "GIF slowed to 0.5x shows each frame for longer rather than making new ones, \
              so its motion is less smooth than the source's."
            ]
        );
        assert!(at(Format::M4a, "0.5", false, &clip()).notes.is_empty());
        assert!(at(Format::Mp4, "2", false, &clip()).notes.is_empty());
    }

    #[test]
    fn a_speed_change_drops_subtitles_and_chapters_in_place_of_a_cuts_options() {
        let retime = at(Format::Mp4, "2", false, &clip()).retime;
        let cut = crate::trim::Cut {
            start_ms: 1_000,
            end_ms: Some(5_000),
        };
        assert_eq!(
            output_args(Some(&cut), Some(&retime)),
            ["-sn", "-map_chapters", "-1"]
        );
        assert_eq!(
            output_args(None, Some(&retime)),
            ["-sn", "-map_chapters", "-1"]
        );
        assert_eq!(output_args(Some(&cut), None), cut.output_args());
        assert!(output_args(None, None).is_empty());
    }
}
