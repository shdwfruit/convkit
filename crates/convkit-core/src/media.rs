//! Probe-aware ffmpeg invocations for container changes and audio
//! extraction.
//!
//! The static registry table can only spell literal argv, which forces it
//! to lean on ffmpeg's default stream selection — the root cause behind a
//! whole class of silent losses: a remux that kept one audio track and no
//! subtitles while reporting "stream copy, no re-encode" with an empty
//! warnings array, an all-or-nothing remux decision that re-encoded video
//! because one audio codec didn't fit, and camera timecode streams that
//! made the matroska muxer fail outright. This module builds the argv
//! *from the probe*: every stream is mapped explicitly, every stream the
//! target can't carry is excluded deliberately and reported as a warning,
//! and a source whose video fits but whose audio doesn't gets a hybrid
//! copy-video/transcode-audio invocation instead of a full re-encode.
//!
//! Everything here is pure argv construction — no filesystem access, no
//! process spawning — so `plan::build` can use it for both `--dry-run`
//! previews and real runs, and the two can never disagree.

use std::path::Path;

use crate::probe::MediaProbe;
use crate::{registry, Format};

/// A fully rendered single-step ffmpeg invocation plus the honesty that
/// goes with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaInvocation {
    /// Complete ffmpeg argv (excluding the program itself), with real
    /// input/output paths already substituted.
    pub argv: Vec<String>,
    pub warnings: Vec<String>,
}

/// Subtitle codecs that are plain text and can therefore be re-encoded
/// between text formats (srt/ass/mov_text/webvtt) essentially losslessly.
/// Bitmap codecs (PGS, dvd_subtitle) have no text to extract, so a target
/// without bitmap support can only drop them — with a warning.
const TEXT_SUBTITLES: &[&str] = &["subrip", "srt", "ass", "ssa", "mov_text", "webvtt", "text"];

fn is_text_subtitle(codec: &str) -> bool {
    TEXT_SUBTITLES.contains(&codec)
}

/// The audio encoder + bitrate a hybrid (copy-video/transcode-audio)
/// invocation uses per target container, matching the static transcode
/// recipes' own choices.
fn audio_encoder_for(to: Format) -> Option<(&'static str, &'static str)> {
    match to {
        Format::Mp4 | Format::Mov | Format::Mkv => Some(("aac", "160k")),
        Format::Webm => Some(("libopus", "128k")),
        _ => None,
    }
}

/// The subtitle codec a target container wants text subtitles in, or
/// `None` when the container takes them as-is (matroska copies everything
/// except `mov_text`).
fn compat_tables(to: Format) -> Option<(&'static [&'static str], &'static [&'static str])> {
    match to {
        Format::Mp4 => Some((registry::MP4_COMPATIBLE_VIDEO, registry::MP4_COMPATIBLE_AUDIO)),
        Format::Mov => Some((registry::MOV_COMPATIBLE_VIDEO, registry::MOV_COMPATIBLE_AUDIO)),
        Format::Mkv => Some((registry::MKV_COMPATIBLE_VIDEO, registry::MKV_COMPATIBLE_AUDIO)),
        Format::Webm => Some((registry::WEBM_COMPATIBLE_VIDEO, registry::WEBM_COMPATIBLE_AUDIO)),
        _ => None,
    }
}

/// Builds the stream-mapped invocation for a container change, given a
/// probe: a full stream copy when every stream fits the target, a hybrid
/// copy-video/transcode-audio invocation when only the audio doesn't, and
/// `None` when the video itself has to be re-encoded (or was never seen) —
/// the caller then falls back to the registry's static transcode recipe.
pub(crate) fn stream_mapped_invocation(
    to: Format,
    probe: &MediaProbe,
    input: &Path,
    output: &Path,
) -> Option<MediaInvocation> {
    let (video_ok, audio_ok) = compat_tables(to)?;
    let video = probe.video_codec.as_deref()?;
    if !video_ok.contains(&video) {
        return None;
    }

    let audios = probe.all_audio();
    let all_audio_legal = audios.iter().all(|c| audio_ok.contains(c));

    let subtitles = probe.all_subtitles();
    let all_subs_text = subtitles.iter().all(|c| is_text_subtitle(c));

    let mut argv: Vec<String> = vec!["-i".into(), input.to_string_lossy().into_owned()];
    let mut warnings: Vec<String> = Vec::new();
    let arg = |argv: &mut Vec<String>, items: &[&str]| {
        argv.extend(items.iter().map(|s| (*s).to_string()));
    };

    // --- stream selection ------------------------------------------------
    if to == Format::Mkv {
        // Matroska holds almost anything, so keep everything — except data
        // (timecode/metadata) streams, which its muxer rejects outright.
        arg(&mut argv, &["-map", "0", "-map", "-0:d"]);
    } else {
        // mp4/mov/webm: name exactly what is carried. `?` keeps a map legal
        // when the source has no stream of that type at all.
        arg(&mut argv, &["-map", "0:v:0", "-map", "0:a?"]);
        if !subtitles.is_empty() && all_subs_text {
            arg(&mut argv, &["-map", "0:s?"]);
        }
    }

    // --- codecs ----------------------------------------------------------
    arg(&mut argv, &["-c:v", "copy"]);
    if all_audio_legal {
        arg(&mut argv, &["-c:a", "copy"]);
    } else {
        let (encoder, bitrate) = audio_encoder_for(to)?;
        arg(&mut argv, &["-c:a", encoder, "-b:a", bitrate]);
        if to == Format::Webm {
            // libopus rejects ffmpeg's default `5.1(side)` layout (AC-3/DTS
            // rips); coerce to the nearest layout it accepts.
            arg(&mut argv, &["-af", registry::OPUS_CHANNEL_LAYOUTS]);
        }
        let offenders: Vec<&str> = audios
            .iter()
            .copied()
            .filter(|c| !audio_ok.contains(c))
            .collect();
        warnings.push(format!(
            "Audio re-encoded to {encoder}: {} {} not supported by {}. Video is stream-copied untouched.",
            offenders.join("/"),
            if offenders.len() == 1 { "is" } else { "are" },
            to.ext(),
        ));
    }

    // --- subtitles -------------------------------------------------------
    match to {
        Format::Mkv => {
            if subtitles.iter().any(|c| *c == "mov_text") {
                // Matroska has no codec ID for mp4/mov's mov_text; re-encode
                // text-to-text into SRT, which it does take.
                arg(&mut argv, &["-c:s", "srt"]);
                warnings.push(
                    "Subtitle tracks stored as MP4/MOV's mov_text are re-encoded to SRT text; \
                     matroska has no codec for mov_text itself."
                        .to_string(),
                );
            }
        }
        Format::Mp4 | Format::Mov | Format::Webm => {
            if !subtitles.is_empty() {
                if all_subs_text {
                    let target_codec = if to == Format::Webm { "webvtt" } else { "mov_text" };
                    arg(&mut argv, &["-c:s", target_codec]);
                } else {
                    warnings.push(format!(
                        "Bitmap subtitle track(s) ({}) cannot be carried by {}; they are dropped. \
                         Convert to mkv to keep them.",
                        subtitles
                            .iter()
                            .copied()
                            .filter(|c| !is_text_subtitle(c))
                            .collect::<Vec<_>>()
                            .join("/"),
                        to.ext(),
                    ));
                }
            }
        }
        _ => {}
    }

    if probe.data_streams > 0 {
        warnings.push(format!(
            "{} timecode/metadata data stream(s) in the source are not carried by {}.",
            probe.data_streams,
            to.ext(),
        ));
    }

    if matches!(to, Format::Mp4 | Format::Mov) {
        arg(&mut argv, &["-movflags", "+faststart"]);
    }
    arg(&mut argv, &["-y"]);
    argv.push(output.to_string_lossy().into_owned());

    Some(MediaInvocation { argv, warnings })
}

/// Audio codecs each audio target container can hold as-is — the gate for
/// lossless `-c:a copy` extraction instead of a generation-loss re-encode
/// of a codec the target already speaks (mp4 → m4a used to re-encode AAC
/// to AAC).
fn copyable_audio_for(to: Format) -> Option<&'static [&'static str]> {
    match to {
        Format::M4a => Some(&["aac", "alac"]),
        Format::Mp3 => Some(&["mp3"]),
        Format::Flac => Some(&["flac"]),
        Format::Wav => Some(&["pcm_s16le"]),
        _ => None,
    }
}

/// Builds a stream-copy audio extraction when the source's first audio
/// stream is already in a codec the target container holds natively.
/// `None` falls back to the registry's static transcode recipe. An audio
/// source keeps its attached cover art where the target can carry it,
/// matching the static keep-art recipes; a video source drops the video
/// stream.
pub(crate) fn audio_copy_invocation(
    from: Format,
    to: Format,
    probe: &MediaProbe,
    input: &Path,
    output: &Path,
) -> Option<MediaInvocation> {
    let copyable = copyable_audio_for(to)?;
    let audios = probe.all_audio();
    let first = audios.first()?;
    if !copyable.contains(first) {
        return None;
    }

    let audio_source = matches!(
        from,
        Format::Mp3 | Format::M4a | Format::Wav | Format::Flac
    );

    let mut argv: Vec<String> = vec!["-i".into(), input.to_string_lossy().into_owned()];
    let mut push = |items: &[&str]| {
        // Closure over argv only; warnings are appended after.
        items.iter().for_each(|s| argv.push((*s).to_string()));
    };
    // The probe describes stream order, so map the first audio stream
    // explicitly rather than trusting default selection (which picks by
    // channel count and could grab a stream the probe never approved).
    push(&["-map", "0:a:0"]);
    if audio_source && to != Format::Wav {
        // WAV can't carry an attached picture; everything else keeps it.
        push(&["-map", "0:v?", "-c:v", "copy"]);
    }
    push(&["-c:a", "copy", "-y"]);
    argv.push(output.to_string_lossy().into_owned());

    let mut warnings = Vec::new();
    if audios.len() > 1 {
        warnings.push(format!(
            "Source has {} audio tracks; only the first is extracted.",
            audios.len()
        ));
    }

    Some(MediaInvocation { argv, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn probe(
        video: Option<&str>,
        audio: &[&str],
        subs: &[&str],
        data_streams: usize,
    ) -> MediaProbe {
        MediaProbe {
            video_codec: video.map(str::to_owned),
            audio_codec: audio.first().map(|s| (*s).to_string()),
            subtitle_codec: subs.first().map(|s| (*s).to_string()),
            audio_codecs: audio.iter().map(|s| (*s).to_string()).collect(),
            subtitle_codecs: subs.iter().map(|s| (*s).to_string()).collect(),
            data_streams,
            ..MediaProbe::default()
        }
    }

    fn invoke(to: Format, p: &MediaProbe) -> Option<MediaInvocation> {
        stream_mapped_invocation(to, p, &PathBuf::from("in"), &PathBuf::from("out"))
    }

    fn has(argv: &[String], pair: [&str; 2]) -> bool {
        argv.windows(2).any(|w| w == pair)
    }

    /// The flagship bug: a two-audio-track source must map *both* tracks
    /// through an mp4 remux, not silently keep one.
    #[test]
    fn mp4_remux_maps_every_audio_track() {
        let m = invoke(Format::Mp4, &probe(Some("h264"), &["aac", "aac"], &[], 0)).unwrap();
        assert!(has(&m.argv, ["-map", "0:a?"]), "{:?}", m.argv);
        assert!(has(&m.argv, ["-c:v", "copy"]), "{:?}", m.argv);
        assert!(has(&m.argv, ["-c:a", "copy"]), "{:?}", m.argv);
        assert!(m.warnings.is_empty(), "{:?}", m.warnings);
    }

    /// A DTS track on stream 2 must veto the full copy — the probe once
    /// approved a remux off stream 1 alone and shipped a DTS track most
    /// players can't decode. Video stays copied; audio is re-encoded, with
    /// a warning that says so.
    #[test]
    fn an_illegal_second_audio_track_forces_the_hybrid_not_a_full_copy() {
        let m = invoke(Format::Mp4, &probe(Some("h264"), &["aac", "dts"], &[], 0)).unwrap();
        assert!(has(&m.argv, ["-c:v", "copy"]), "{:?}", m.argv);
        assert!(has(&m.argv, ["-c:a", "aac"]), "{:?}", m.argv);
        assert!(!has(&m.argv, ["-c:a", "copy"]), "{:?}", m.argv);
        assert!(
            m.warnings.iter().any(|w| w.contains("dts")),
            "{:?}",
            m.warnings
        );
    }

    /// The all-or-nothing bug (F2): incompatible audio used to trigger a
    /// full libx264 re-encode. Video must stay `-c:v copy`.
    #[test]
    fn incompatible_audio_alone_never_reencodes_the_video() {
        let m = invoke(Format::Mp4, &probe(Some("h264"), &["opus"], &[], 0)).unwrap();
        assert!(has(&m.argv, ["-c:v", "copy"]), "{:?}", m.argv);
        assert!(!m.argv.contains(&"libx264".to_string()), "{:?}", m.argv);
    }

    /// An incompatible *video* codec is not this module's job: the caller
    /// falls back to the registry's static transcode recipe.
    #[test]
    fn incompatible_video_falls_back_to_the_static_transcode() {
        assert!(invoke(Format::Mp4, &probe(Some("prores"), &["aac"], &[], 0)).is_none());
        assert!(invoke(Format::Mp4, &probe(None, &["aac"], &[], 0)).is_none());
    }

    /// Text subtitles the target can carry are carried (F9): mp4 takes
    /// mov_text, webm takes webvtt.
    #[test]
    fn text_subtitles_are_carried_not_dropped() {
        let m = invoke(Format::Mp4, &probe(Some("h264"), &["aac"], &["subrip"], 0)).unwrap();
        assert!(has(&m.argv, ["-map", "0:s?"]), "{:?}", m.argv);
        assert!(has(&m.argv, ["-c:s", "mov_text"]), "{:?}", m.argv);
        assert!(!m.argv.contains(&"-sn".to_string()), "{:?}", m.argv);

        let m = invoke(
            Format::Webm,
            &probe(Some("vp9"), &["opus"], &["subrip"], 0),
        )
        .unwrap();
        assert!(has(&m.argv, ["-c:s", "webvtt"]), "{:?}", m.argv);
    }

    /// Bitmap subtitles genuinely can't fit mp4 — they are excluded from
    /// the mapping, and the loss is named in a warning instead of silently
    /// swallowed by `-sn`.
    #[test]
    fn bitmap_subtitles_are_dropped_with_a_warning_never_silently() {
        let m = invoke(
            Format::Mp4,
            &probe(Some("h264"), &["aac"], &["hdmv_pgs_subtitle"], 0),
        )
        .unwrap();
        assert!(!m.argv.iter().any(|a| a.starts_with("0:s")), "{:?}", m.argv);
        assert!(
            m.warnings
                .iter()
                .any(|w| w.contains("hdmv_pgs_subtitle") && w.contains("mkv")),
            "{:?}",
            m.warnings
        );
    }

    /// F5: camera/GoPro timecode data streams break the matroska muxer, so
    /// the mkv remux keeps everything except them — and says so.
    #[test]
    fn mkv_remux_excludes_data_streams_and_warns_when_they_exist() {
        let m = invoke(Format::Mkv, &probe(Some("h264"), &["aac"], &[], 2)).unwrap();
        assert!(has(&m.argv, ["-map", "0"]), "{:?}", m.argv);
        assert!(has(&m.argv, ["-map", "-0:d"]), "{:?}", m.argv);
        assert!(
            m.warnings.iter().any(|w| w.contains("timecode")),
            "{:?}",
            m.warnings
        );

        let quiet = invoke(Format::Mkv, &probe(Some("h264"), &["aac"], &[], 0)).unwrap();
        assert!(quiet.warnings.is_empty(), "{:?}", quiet.warnings);
    }

    /// mkv's one subtitle exception: mov_text re-encodes to SRT.
    #[test]
    fn mkv_reencodes_mov_text_subtitles_to_srt() {
        let m = invoke(
            Format::Mkv,
            &probe(Some("h264"), &["aac"], &["mov_text"], 0),
        )
        .unwrap();
        assert!(has(&m.argv, ["-c:s", "srt"]), "{:?}", m.argv);
        assert!(
            m.warnings.iter().any(|w| w.contains("mov_text")),
            "{:?}",
            m.warnings
        );
    }

    /// The webm hybrid must coerce channel layouts for libopus (F4's
    /// surround-layout rejection applies to the hybrid path too).
    #[test]
    fn webm_hybrid_coerces_channel_layouts_for_libopus() {
        let m = invoke(Format::Webm, &probe(Some("vp9"), &["ac3"], &[], 0)).unwrap();
        assert!(has(&m.argv, ["-c:a", "libopus"]), "{:?}", m.argv);
        assert!(
            m.argv
                .iter()
                .any(|a| a.contains("aformat=channel_layouts")),
            "{:?}",
            m.argv
        );
    }

    /// `-movflags +faststart` belongs to the mov/mp4 muxer family only.
    #[test]
    fn only_the_mp4_family_gets_movflags() {
        for (to, expect) in [
            (Format::Mp4, true),
            (Format::Mov, true),
            (Format::Mkv, false),
            (Format::Webm, false),
        ] {
            let video = if to == Format::Webm { "vp9" } else { "h264" };
            let audio = if to == Format::Webm { "opus" } else { "aac" };
            let m = invoke(to, &probe(Some(video), &[audio], &[], 0)).unwrap();
            assert_eq!(
                m.argv.contains(&"-movflags".to_string()),
                expect,
                "{to:?}: {:?}",
                m.argv
            );
        }
    }

    // --- audio extraction (F6) ------------------------------------------

    fn audio_invoke(from: Format, to: Format, p: &MediaProbe) -> Option<MediaInvocation> {
        audio_copy_invocation(from, to, p, &PathBuf::from("in"), &PathBuf::from("out"))
    }

    /// mp4 → m4a used to re-encode AAC to AAC; a matching codec must be a
    /// stream copy, dropping the video explicitly.
    #[test]
    fn matching_audio_codec_extracts_by_stream_copy() {
        let m = audio_invoke(Format::Mp4, Format::M4a, &probe(Some("h264"), &["aac"], &[], 0))
            .unwrap();
        assert!(has(&m.argv, ["-map", "0:a:0"]), "{:?}", m.argv);
        assert!(has(&m.argv, ["-c:a", "copy"]), "{:?}", m.argv);
        assert!(!m.argv.contains(&"aac".to_string()), "{:?}", m.argv);
    }

    /// A codec the target can't hold as-is falls back to the static
    /// transcode recipe.
    #[test]
    fn non_matching_audio_codec_falls_back_to_transcode() {
        assert!(
            audio_invoke(Format::Mp4, Format::Mp3, &probe(Some("h264"), &["aac"], &[], 0))
                .is_none()
        );
    }

    /// An audio source keeps its cover art through a copy extraction,
    /// matching the static keep-art recipes; WAV still can't carry one.
    #[test]
    fn audio_sources_keep_cover_art_except_into_wav() {
        let m = audio_invoke(Format::Flac, Format::M4a, &probe(None, &["alac"], &[], 0));
        // flac holding alac is unusual but legal for the copy gate; the
        // point is the art mapping.
        let m = m.unwrap();
        assert!(has(&m.argv, ["-map", "0:v?"]), "{:?}", m.argv);
        assert!(has(&m.argv, ["-c:v", "copy"]), "{:?}", m.argv);

        let m = audio_invoke(Format::Mp4, Format::M4a, &probe(Some("h264"), &["aac"], &[], 0))
            .unwrap();
        assert!(!has(&m.argv, ["-map", "0:v?"]), "video sources drop video: {:?}", m.argv);
    }

    /// More than one audio track can't all fit a single-track extraction;
    /// the loss is named.
    #[test]
    fn multi_track_sources_warn_that_only_the_first_is_extracted() {
        let m = audio_invoke(
            Format::Mkv,
            Format::M4a,
            &probe(Some("h264"), &["aac", "ac3"], &[], 0),
        )
        .unwrap();
        assert!(
            m.warnings.iter().any(|w| w.contains("audio tracks")),
            "{:?}",
            m.warnings
        );
    }
}
