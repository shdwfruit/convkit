//! `--denoise`: background noise and mains hum taken out of the sound, with
//! ffmpeg's own filters and no model to download. Pure: no process, no
//! filesystem.
//!
//! The chain, measured in docs/defaults-calibration.md, is a high-pass at
//! 80 Hz, which takes the hum's fundamental and any rumble, then `afftdn`,
//! ffmpeg's FFT denoiser, set for a noise floor around -35 dB with a
//! spectral shape that falls with frequency (`nt=v`), as room and fan noise
//! and the hum's harmonics do. `strong` assumes a louder floor and takes up
//! to 30 dB off it rather than 20: the pauses go quieter still, and the
//! voice loses a little more.
//!
//! `afftdn` delays its output by half its 50 ms window and leaves the
//! timestamps as they were, so on its own it moves the sound 25 ms behind
//! the picture and drops the last 25 ms. The chain pads 25 ms of silence
//! onto the end, then moves every timestamp back by 25 ms and trims what
//! now falls before 0, which is the window's warm-up. The sound then lines
//! up to the sample, on ffmpeg 6.1 and 9.0 alike.

use std::path::Path;

use serde::{Serialize, Serializer};

use crate::probe::MediaProbe;
use crate::{registry, ConvError, ErrorCode, Format, Kind};

/// How hard `--denoise` works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strength {
    /// For the usual noisy voice recording: hum, a fan, a room.
    #[default]
    Normal,
    /// For a loud floor: deeper pauses, at some cost to the voice.
    Strong,
}

/// The default chain. See the module docs for the padding and the shift.
pub const NORMAL_CHAIN: &str = "apad=pad_dur=0.025,highpass=f=80,\
     afftdn=nt=v:nf=-35:nr=20,asetpts=PTS-0.025/TB,atrim=start=0";

/// `--denoise strong`: the same chain, assuming a louder floor and taking
/// more off it.
pub const STRONG_CHAIN: &str = "apad=pad_dur=0.025,highpass=f=80,\
     afftdn=nt=v:nf=-30:nr=30,asetpts=PTS-0.025/TB,atrim=start=0";

/// What every denoised conversion is told: the high-pass is a choice made
/// for voice, and music pays for it.
pub const VOICE_NOTE: &str = "--denoise is tuned for voice: it also cuts everything below 80 Hz, \
     so music loses its deepest bass.";

/// What a video whose picture is copied is told: its sound could not be,
/// so it was encoded again, at the rate the target's recipes use.
pub(crate) fn reencoded_note(to: Format) -> String {
    let (codec, rate) = if to == Format::Webm {
        ("Opus", registry::WEBM_AUDIO_BITRATE)
    } else {
        ("AAC", registry::AUDIO_BITRATE)
    };
    format!(
        "The video is copied as it is; only the sound is re-encoded, to {codec} at {} kbps, \
         to clean it up.",
        rate.trim_end_matches('k')
    )
}

/// Parses the strength `--denoise=` takes: `strong`, or `normal`, which
/// is what a bare `--denoise` asks for.
pub fn parse(text: &str) -> Result<Strength, String> {
    match text.to_ascii_lowercase().as_str() {
        "normal" => Ok(Strength::Normal),
        "strong" => Ok(Strength::Strong),
        _ => Err(format!(
            "invalid strength '{text}'; expected strong, or no value for the default"
        )),
    }
}

impl Strength {
    /// The audio filters, for `-af`.
    pub fn chain(&self) -> &'static str {
        match self {
            Strength::Normal => NORMAL_CHAIN,
            Strength::Strong => STRONG_CHAIN,
        }
    }

    /// What an output adds to its name.
    pub fn suffix(&self) -> &'static str {
        match self {
            Strength::Normal => "denoised",
            Strength::Strong => "denoised-strong",
        }
    }

    fn text(&self) -> &'static str {
        match self {
            Strength::Normal => "normal",
            Strength::Strong => "strong",
        }
    }
}

impl Serialize for Strength {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.text())
    }
}

/// Whether the pair's output carries sound conv can filter: its recipe has
/// an `Arg::AudioFilter` slot, which every recipe with sound does and none
/// without it does (`registry`'s `every_timed_recipe_can_take_a_speed_change`).
pub fn takes_denoise(from: Format, to: Format) -> bool {
    let recipe = if from == to {
        registry::same_format_recipe(to)
    } else {
        registry::lookup(from, to)
    };
    recipe.is_some_and(|r| {
        r.steps.iter().any(|s| {
            s.args
                .iter()
                .any(|a| matches!(a, crate::Arg::AudioFilter(_)))
        })
    })
}

const DOES: &str = "--denoise cleans up the sound of video and audio";

/// Refuses `--denoise` where there is no sound to clean, then gives the
/// chain to run. The one entry point both `plan::build_tuned` and
/// `sized::prepare` use. `probe` is the source as it stands after any cut.
pub fn resolve(
    from: Format,
    to: Format,
    strength: Strength,
    mute: bool,
    probe: Option<&MediaProbe>,
    input: &Path,
) -> crate::Result<&'static str> {
    let invalid = |why: String| ConvError::new(ErrorCode::InvalidInvocation, why);
    if !takes_denoise(from, to) {
        let still = |f: Format| f.kind() == Kind::Image && f != Format::Gif;
        let why = if still(to) || still(from) {
            let f = if still(to) { to } else { from };
            format!("{} is a still image", f.ext())
        } else if to == Format::Gif || from == Format::Gif {
            "a GIF has no sound".to_string()
        } else {
            format!("{} -> {} carries no sound", from.ext(), to.ext())
        };
        return Err(invalid(format!(
            "--denoise does not apply to {} -> {}: {why}; {DOES}",
            from.ext(),
            to.ext()
        )));
    }
    if mute {
        return Err(invalid(
            "--denoise cleans up the sound, and a silent clip has none".to_string(),
        ));
    }
    let probe = probe.ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!(
                "--denoise needs ffprobe to read {}; install ffprobe or check that it runs",
                input.display()
            ),
        )
    })?;
    // A probe that saw no stream at all failed to read the file, so its
    // sound is unknown rather than absent, and is filtered all the same.
    let saw_nothing = probe.video_streams == 0 && probe.audio_codecs.is_empty();
    if !saw_nothing && probe.audio_codecs.is_empty() {
        return Err(invalid(format!(
            "--denoise cleans up the sound, and {} has none",
            input.display()
        )));
    }
    Ok(strength.chain())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip() -> MediaProbe {
        MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            audio_codecs: vec!["aac".into()],
            audio_bitrates: vec![None],
            audio_sample_rate: Some(48_000),
            duration_ms: Some(10_000),
            ..MediaProbe::default()
        }
    }

    fn at(from: Format, to: Format, probe: Option<&MediaProbe>) -> crate::Result<&'static str> {
        resolve(from, to, Strength::Normal, false, probe, Path::new("a.mp4"))
    }

    #[test]
    fn a_bare_flag_and_normal_are_the_default_and_strong_is_the_other() {
        assert_eq!(parse("normal"), Ok(Strength::Normal));
        assert_eq!(parse("strong"), Ok(Strength::Strong));
        assert_eq!(parse("STRONG"), Ok(Strength::Strong));
        assert_eq!(Strength::default(), Strength::Normal);
        assert_eq!(
            parse("max").unwrap_err(),
            "invalid strength 'max'; expected strong, or no value for the default"
        );
    }

    #[test]
    fn each_strength_names_its_output_apart() {
        assert_eq!(Strength::Normal.suffix(), "denoised");
        assert_eq!(Strength::Strong.suffix(), "denoised-strong");
        assert_eq!(
            serde_json::to_string(&Strength::Strong).unwrap(),
            "\"strong\""
        );
    }

    /// The measured chain, and the 25 ms `afftdn` holds the sound back,
    /// padded on at the end and taken off the front.
    #[test]
    fn the_chain_is_a_high_pass_then_afftdn_with_its_delay_undone() {
        assert_eq!(
            Strength::Normal.chain(),
            "apad=pad_dur=0.025,highpass=f=80,afftdn=nt=v:nf=-35:nr=20,\
             asetpts=PTS-0.025/TB,atrim=start=0"
        );
        assert_eq!(
            Strength::Strong.chain(),
            "apad=pad_dur=0.025,highpass=f=80,afftdn=nt=v:nf=-30:nr=30,\
             asetpts=PTS-0.025/TB,atrim=start=0"
        );
    }

    #[test]
    fn every_pair_with_sound_takes_it_and_none_without() {
        assert!(takes_denoise(Format::Mp4, Format::Mp4));
        assert!(takes_denoise(Format::Mp4, Format::M4a));
        assert!(takes_denoise(Format::Mkv, Format::Webm));
        assert!(takes_denoise(Format::Mp3, Format::Mp3));
        assert!(takes_denoise(Format::Wav, Format::Flac));
        assert!(!takes_denoise(Format::Mp4, Format::Gif));
        assert!(!takes_denoise(Format::Gif, Format::Mp4));
        assert!(!takes_denoise(Format::Png, Format::Jpg));
        assert!(!takes_denoise(Format::Avi, Format::Avi));
    }

    #[test]
    fn a_copied_video_is_told_its_sound_was_re_encoded_and_how() {
        assert_eq!(
            reencoded_note(Format::Mp4),
            "The video is copied as it is; only the sound is re-encoded, to AAC at 160 kbps, \
             to clean it up."
        );
        assert!(reencoded_note(Format::Webm).contains("to Opus at 128 kbps"));
    }

    #[test]
    fn a_pair_with_no_sound_is_refused_saying_why() {
        let e = at(Format::Mp4, Format::Gif, Some(&clip())).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "--denoise does not apply to mp4 -> gif: a GIF has no sound; \
             --denoise cleans up the sound of video and audio"
        );
        assert_eq!(
            at(Format::Mp4, Format::Png, None).unwrap_err().message,
            "--denoise does not apply to mp4 -> png: png is a still image; \
             --denoise cleans up the sound of video and audio"
        );
    }

    #[test]
    fn a_source_with_no_sound_is_refused() {
        let silent = MediaProbe {
            audio_codecs: Vec::new(),
            audio_bitrates: Vec::new(),
            audio_sample_rate: None,
            ..clip()
        };
        assert_eq!(
            at(Format::Mp4, Format::Mp4, Some(&silent))
                .unwrap_err()
                .message,
            "--denoise cleans up the sound, and a.mp4 has none"
        );
        let muted = resolve(
            Format::Mp4,
            Format::Mp4,
            Strength::Normal,
            true,
            Some(&clip()),
            Path::new("a.mp4"),
        );
        assert_eq!(
            muted.unwrap_err().message,
            "--denoise cleans up the sound, and a silent clip has none"
        );
    }

    #[test]
    fn it_needs_the_probe_and_filters_a_probe_that_saw_nothing() {
        let e = at(Format::Mp4, Format::Mp4, None).unwrap_err();
        assert_eq!(e.code, ErrorCode::ConversionFailed);
        assert_eq!(
            e.message,
            "--denoise needs ffprobe to read a.mp4; install ffprobe or check that it runs"
        );
        assert_eq!(
            at(Format::Mp4, Format::Mp4, Some(&MediaProbe::default())),
            Ok(NORMAL_CHAIN)
        );
        assert_eq!(
            at(Format::Mp4, Format::M4a, Some(&clip())),
            Ok(NORMAL_CHAIN)
        );
    }
}
