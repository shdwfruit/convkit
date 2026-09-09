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
//! target can't carry is excluded deliberately and reported as a warning
//! naming exactly what was lost or re-encoded, and a source whose video
//! fits but whose audio doesn't keeps the video as a stream copy.
//!
//! Everything here is pure argv construction — no filesystem access, no
//! process spawning — so `plan::build` can use it for both `--dry-run`
//! previews and real runs, and the two can never disagree.

use std::path::Path;

use crate::probe::MediaProbe;
use crate::video::ResolvedVideo;
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
/// between text formats (srt/ass/mov_text/webvtt) essentially losslessly —
/// modulo ASS styling, which gets its own warning. Bitmap codecs (PGS,
/// dvd_subtitle, xsub) have no text to extract, so a target without
/// bitmap support can only drop them — with a warning. A stream the probe
/// saw but could not name (`unknown`) is treated as neither.
const TEXT_SUBTITLES: &[&str] = &["subrip", "srt", "ass", "ssa", "mov_text", "webvtt", "text"];

/// Subtitle codecs matroska takes with a plain `-c:s copy` — verified the
/// hard way: its muxer rejects `mov_text` ("Subtitle codec mov_text
/// (94213) is not supported") and `xsub` outright, while text formats and
/// the two common bitmap formats copy cleanly. `mov_text` is handled by
/// re-encoding to SRT; anything not listed here and not `mov_text` is
/// excluded from the mapping with a warning, because ffmpeg's fallback —
/// its default matroska subtitle encoder — dies on any bitmap source
/// ("Subtitle encoding currently only possible from text to text or
/// bitmap to bitmap").
const MKV_COPY_SUBTITLES: &[&str] = &[
    "subrip",
    "srt",
    "ass",
    "ssa",
    "webvtt",
    "text",
    "hdmv_pgs_subtitle",
    "dvd_subtitle",
];

fn is_text_subtitle(codec: &str) -> bool {
    TEXT_SUBTITLES.contains(&codec)
}

fn push(argv: &mut Vec<String>, items: &[&str]) {
    argv.extend(items.iter().map(|s| (*s).to_string()));
}

/// What happens to the video stream. Everything else about the mapping --
/// which audio tracks survive, which subtitles the muxer can hold, which
/// attachments ride along -- is identical either way, which is why there is
/// one function and not two.
#[derive(Debug, Clone)]
pub(crate) enum VideoDisposition<'a> {
    /// `-c:v copy`. The fast path, and the reason this file exists.
    Copy,
    /// Re-encode: the composed filter chain, the target's encoder, and the
    /// CRF to use.
    Transcode {
        chain: &'a str,
        encoder: &'static str,
        crf: String,
        /// libx264 needs `-pix_fmt yuv420p`; libvpx-vp9 needs its own
        /// companions instead.
        companions: &'static [&'static str],
    },
}

/// Emits the video codec and, for a transcode, its filter chain.
///
/// The mkv branch maps `-map 0` -- keep everything, carve out what the
/// muxer rejects -- so `-map 0` selects every video stream, including
/// attached-picture cover art that `probe.video_streams` deliberately does
/// not count. A global `-c:v libx264` there would re-encode album art as a
/// video track, so mkv scopes both the filter and the codec to stream 0 and
/// leaves the rest copied.
fn push_video_args(argv: &mut Vec<String>, to: Format, video: &VideoDisposition<'_>) {
    match video {
        VideoDisposition::Copy => push(argv, &["-c:v", "copy"]),
        VideoDisposition::Transcode {
            chain,
            encoder,
            crf,
            companions,
        } => {
            if to == Format::Mkv {
                push(argv, &["-filter:v:0", chain]);
                push(argv, &["-c:v", "copy"]);
                push(argv, &["-c:v:0", encoder]);
            } else {
                push(argv, &["-vf", chain]);
                push(argv, &["-c:v", encoder]);
            }
            push(argv, &["-crf", crf.as_str()]);
            push(argv, companions);
        }
    }
}

/// Builds the stream-mapped invocation for a container change, given a
/// probe and a video disposition (stream-copy or transcode). Every stream
/// is mapped explicitly, every stream the target can't carry is excluded
/// deliberately and reported as a warning naming exactly what was lost or
/// re-encoded. Shared by `stream_mapped_invocation` and
/// `transcoded_invocation` so the mapping -- which audio tracks survive,
/// which subtitles the muxer can hold, which attachments ride along -- is
/// decided exactly once.
fn mapped_invocation(
    to: Format,
    probe: &MediaProbe,
    video: VideoDisposition<'_>,
    input: &Path,
    output: &Path,
) -> Option<MediaInvocation> {
    let (_, audio_ok) = registry::compat_tables(to)?;

    let audios = probe.all_audio();
    let subtitles = probe.all_subtitles();

    let mut argv: Vec<String> = vec!["-i".into(), input.to_string_lossy().into_owned()];
    let mut warnings: Vec<String> = Vec::new();

    if to == Format::Mkv {
        // Matroska holds almost anything: keep everything, then carve out
        // the exceptions its muxer genuinely rejects — data
        // (timecode/metadata) streams, and the few subtitle codecs it has
        // no ID for. Attachments (fonts) ride along.
        push(&mut argv, &["-map", "0", "-map", "-0:d"]);
        let mut any_kept_sub = false;
        let mut any_mov_text = false;
        for (i, sub) in subtitles.iter().enumerate() {
            if *sub == "mov_text" || MKV_COPY_SUBTITLES.contains(sub) {
                any_kept_sub = true;
                any_mov_text |= *sub == "mov_text";
            } else {
                push(&mut argv, &["-map", &format!("-0:s:{i}")]);
                warnings.push(format!(
                    "Subtitle track {i} ({sub}) cannot be carried by mkv; it is dropped."
                ));
            }
        }

        push_video_args(&mut argv, to, &video);
        audio_codec_args(&mut argv, &mut warnings, to, audio_ok, &audios);

        if any_mov_text {
            // mov_text only ever comes from mp4/mov, which cannot hold the
            // codecs that would need a per-stream split here — so a global
            // SRT re-encode is safe, and text-to-text is lossless.
            push(&mut argv, &["-c:s", "srt"]);
            warnings.push(
                "Subtitle tracks stored as MP4/MOV's mov_text are re-encoded to SRT text; \
                 matroska has no codec for mov_text itself."
                    .to_string(),
            );
        } else if any_kept_sub {
            // Explicit: without this, ffmpeg re-encodes text subtitles to
            // its matroska default (ASS) — silently, and wrongly labelled
            // a stream copy.
            push(&mut argv, &["-c:s", "copy"]);
        }

        if probe.data_streams > 0 {
            warnings.push(format!(
                "{} timecode/metadata data stream(s) in the source are not carried by mkv.",
                probe.data_streams
            ));
        }
    } else {
        // mp4/mov/webm: name exactly what is carried, per stream.
        push(&mut argv, &["-map", "0:v:0", "-map", "0:a?"]);
        if probe.video_streams > 1 {
            warnings.push(format!(
                "{} additional video stream(s) in the source are not carried by {}; \
                 convert to mkv to keep them.",
                probe.video_streams - 1,
                to.ext(),
            ));
        }

        // Text subtitles ride along, each mapped by its own index so a
        // bitmap sibling never costs the text tracks their seat; bitmap
        // and unidentifiable tracks are excluded by never being mapped.
        let kept_subs: Vec<(usize, &str)> = subtitles
            .iter()
            .enumerate()
            .filter(|(_, s)| is_text_subtitle(s))
            .map(|(i, s)| (i, *s))
            .collect();
        let dropped_subs: Vec<&str> = subtitles
            .iter()
            .copied()
            .filter(|s| !is_text_subtitle(s))
            .collect();
        for (i, _) in &kept_subs {
            push(&mut argv, &["-map", &format!("0:s:{i}")]);
        }
        if !dropped_subs.is_empty() {
            warnings.push(format!(
                "Subtitle track(s) ({}) cannot be carried by {}; they are dropped. \
                 Convert to mkv to keep them.",
                dropped_subs.join("/"),
                to.ext(),
            ));
        }

        push_video_args(&mut argv, to, &video);
        audio_codec_args(&mut argv, &mut warnings, to, audio_ok, &audios);

        if !kept_subs.is_empty() {
            let target_codec = if to == Format::Webm {
                "webvtt"
            } else {
                "mov_text"
            };
            push(&mut argv, &["-c:s", target_codec]);
            if kept_subs.iter().any(|(_, s)| matches!(*s, "ass" | "ssa")) {
                warnings.push(format!(
                    "ASS subtitle styling is not preserved by {target_codec}; the text is kept."
                ));
            }
        }

        if probe.attachment_streams > 0 {
            warnings.push(format!(
                "{} attachment stream(s) (fonts) in the source are not carried by {}; \
                 convert to mkv to keep them.",
                probe.attachment_streams,
                to.ext(),
            ));
        }
        // Data streams: no warning for mp4/mov — their muxer regenerates a
        // tmcd track from the copied video's timecode side data, so
        // claiming a loss would be untrue (verified against a real tmcd
        // source). webm genuinely cannot carry them.
        if to == Format::Webm && probe.data_streams > 0 {
            warnings.push(format!(
                "{} timecode/metadata data stream(s) in the source are not carried by webm.",
                probe.data_streams
            ));
        }
    }

    if matches!(to, Format::Mp4 | Format::Mov) {
        push(&mut argv, &["-movflags", "+faststart"]);
    }
    push(&mut argv, &["-y"]);
    argv.push(output.to_string_lossy().into_owned());

    Some(MediaInvocation { argv, warnings })
}

/// Builds the stream-mapped invocation for a container change, given a
/// probe: a full stream copy when every stream fits the target, a hybrid
/// that keeps the video copied and re-encodes only the audio tracks that
/// don't fit, and `None` when the video itself has to be re-encoded (or
/// was never seen) — the caller then falls back to the registry's static
/// transcode recipe.
pub(crate) fn stream_mapped_invocation(
    to: Format,
    probe: &MediaProbe,
    input: &Path,
    output: &Path,
) -> Option<MediaInvocation> {
    let (video_ok, _) = registry::compat_tables(to)?;
    let video = probe.video_codec.as_deref()?;
    if !video_ok.contains(&video) {
        return None;
    }
    mapped_invocation(to, probe, VideoDisposition::Copy, input, output)
}

/// The same stream mapping, with the video re-encoded.
///
/// Reached when a video knob is given on a pair that would otherwise stream
/// copy. ffmpeg refuses a filter alongside `-c:v copy` outright
/// ("Filtering and streamcopy cannot be used together"), so honouring the
/// knob means giving up the copy -- and giving it up here, rather than
/// falling through to the static table, is what keeps the second audio
/// track and the subtitles the static recipe would drop.
pub(crate) fn transcoded_invocation(
    to: Format,
    probe: &MediaProbe,
    resolved: &ResolvedVideo,
    crf: Option<u8>,
    input: &Path,
    output: &Path,
) -> Option<MediaInvocation> {
    registry::compat_tables(to)?;
    probe.video_codec.as_deref()?;
    let (encoder, anchor, companions): (&str, &str, &[&str]) = match to {
        Format::Mp4 | Format::Mov | Format::Mkv => {
            ("libx264", registry::CRF, &["-pix_fmt", "yuv420p"])
        }
        Format::Webm => (
            "libvpx-vp9",
            registry::WEBM_CRF,
            &["-b:v", "0", "-row-mt", "1", "-threads", "0"],
        ),
        _ => return None,
    };
    let chain = registry::TRANSCODE_CHAIN.compose(resolved);
    let mut m = mapped_invocation(
        to,
        probe,
        VideoDisposition::Transcode {
            chain: &chain,
            encoder,
            crf: crf
                .map(|c| c.to_string())
                .unwrap_or_else(|| anchor.to_string()),
            companions,
        },
        input,
        output,
    )?;
    m.warnings.push(
        "Re-encoded rather than stream-copied, because a video knob changes \
         the picture; the copy path cannot filter."
            .to_string(),
    );
    Some(m)
}

/// Emits the audio codec arguments: a plain `-c:a copy` when every track
/// fits, or — when some don't — a per-track split that copies the legal
/// tracks and re-encodes only the offenders, so an AAC track never pays a
/// generation loss for its DTS sibling. WebM is the exception: filtering
/// (the channel-layout coercion libopus requires) cannot coexist with
/// stream copy on the same invocation, so any offender there re-encodes
/// every track.
fn audio_codec_args(
    argv: &mut Vec<String>,
    warnings: &mut Vec<String>,
    to: Format,
    audio_ok: &[&str],
    audios: &[&str],
) {
    let all_legal = audios.iter().all(|c| audio_ok.contains(c));
    if all_legal {
        push(argv, &["-c:a", "copy"]);
        return;
    }

    let offenders: Vec<String> = audios
        .iter()
        .copied()
        .filter(|c| !audio_ok.contains(c))
        .map(str::to_owned)
        .collect();

    if to == Format::Webm {
        push(
            argv,
            &["-c:a", "libopus", "-b:a", registry::WEBM_AUDIO_BITRATE],
        );
        push(argv, &["-af", registry::OPUS_CHANNEL_LAYOUTS]);
        warnings.push(format!(
            "All audio tracks re-encoded to opus: {} not supported by webm \
             (stream copy and the required channel-layout filter cannot mix). \
             Video is stream-copied untouched.",
            offenders.join("/"),
        ));
        return;
    }

    let mut reencoded: Vec<String> = Vec::new();
    for (i, codec) in audios.iter().enumerate() {
        if audio_ok.contains(codec) {
            push(argv, &[&format!("-c:a:{i}"), "copy"]);
        } else {
            push(
                argv,
                &[
                    &format!("-c:a:{i}"),
                    "aac",
                    &format!("-b:a:{i}"),
                    registry::AUDIO_BITRATE,
                ],
            );
            reencoded.push(format!("track {i} ({codec})"));
        }
    }
    warnings.push(format!(
        "Audio {} re-encoded to aac ({} not supported by {}); every other track \
         and the video are stream-copied untouched.",
        reencoded.join(", "),
        offenders.join("/"),
        to.ext(),
    ));
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
/// `None` falls back to the registry's static transcode recipe — including
/// for a stream the probe saw but could not name, which is `"unknown"`
/// here and in no allowlist. An audio source keeps its attached cover art
/// where the target can carry it, matching the static keep-art recipes; a
/// video source drops the video stream.
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

    let audio_source = matches!(from, Format::Mp3 | Format::M4a | Format::Wav | Format::Flac);

    let mut argv: Vec<String> = vec!["-i".into(), input.to_string_lossy().into_owned()];
    // The probe describes stream order, so map the first audio stream
    // explicitly rather than trusting default selection (which picks by
    // channel count and could grab a stream the probe never approved).
    push(&mut argv, &["-map", "0:a:0"]);
    if audio_source && to != Format::Wav {
        // WAV can't carry an attached picture; everything else keeps it.
        push(&mut argv, &["-map", "0:v?", "-c:v", "copy"]);
    }
    push(&mut argv, &["-c:a", "copy", "-y"]);
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
            audio_codecs: audio.iter().map(|s| (*s).to_string()).collect(),
            subtitle_codecs: subs.iter().map(|s| (*s).to_string()).collect(),
            data_streams,
            video_streams: usize::from(video.is_some()),
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

    /// A DTS track on stream 2 must not veto its AAC sibling's stream
    /// copy: only the offending track is re-encoded, per-stream, and the
    /// warning names exactly which.
    #[test]
    fn an_illegal_audio_track_reencodes_only_itself() {
        let m = invoke(Format::Mp4, &probe(Some("h264"), &["aac", "dts"], &[], 0)).unwrap();
        assert!(has(&m.argv, ["-c:v", "copy"]), "{:?}", m.argv);
        assert!(has(&m.argv, ["-c:a:0", "copy"]), "{:?}", m.argv);
        assert!(has(&m.argv, ["-c:a:1", "aac"]), "{:?}", m.argv);
        assert!(!has(&m.argv, ["-c:a", "copy"]), "{:?}", m.argv);
        let w = m.warnings.join(" ");
        assert!(w.contains("track 1 (dts)"), "{:?}", m.warnings);
        assert!(w.contains("stream-copied"), "{:?}", m.warnings);
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
    /// falls back to the registry's static transcode recipe. Same for a
    /// video stream the probe could not identify (`unknown` placeholder).
    #[test]
    fn incompatible_or_unknown_video_falls_back_to_the_static_transcode() {
        assert!(invoke(Format::Mp4, &probe(Some("prores"), &["aac"], &[], 0)).is_none());
        assert!(invoke(Format::Mp4, &probe(None, &["aac"], &[], 0)).is_none());
        assert!(invoke(Format::Mp4, &probe(Some("unknown"), &["aac"], &[], 0)).is_none());
    }

    /// Text subtitles the target can carry are carried (F9): mp4 takes
    /// mov_text, webm takes webvtt — each mapped by its own index.
    #[test]
    fn text_subtitles_are_carried_not_dropped() {
        let m = invoke(Format::Mp4, &probe(Some("h264"), &["aac"], &["subrip"], 0)).unwrap();
        assert!(has(&m.argv, ["-map", "0:s:0"]), "{:?}", m.argv);
        assert!(has(&m.argv, ["-c:s", "mov_text"]), "{:?}", m.argv);
        assert!(!m.argv.contains(&"-sn".to_string()), "{:?}", m.argv);

        let m = invoke(Format::Webm, &probe(Some("vp9"), &["opus"], &["subrip"], 0)).unwrap();
        assert!(has(&m.argv, ["-c:s", "webvtt"]), "{:?}", m.argv);
    }

    /// Mixed text + bitmap subtitles: the text track keeps its seat (its
    /// own `-map 0:s:N`), only the bitmap track is dropped, and the
    /// warning names the dropped one — a bitmap sibling used to silently
    /// cost the text track its mapping.
    #[test]
    fn a_bitmap_sibling_never_costs_a_text_subtitle_its_seat() {
        let m = invoke(
            Format::Mp4,
            &probe(Some("h264"), &["aac"], &["subrip", "hdmv_pgs_subtitle"], 0),
        )
        .unwrap();
        assert!(has(&m.argv, ["-map", "0:s:0"]), "{:?}", m.argv);
        assert!(!has(&m.argv, ["-map", "0:s:1"]), "{:?}", m.argv);
        assert!(has(&m.argv, ["-c:s", "mov_text"]), "{:?}", m.argv);
        assert!(
            m.warnings
                .iter()
                .any(|w| w.contains("hdmv_pgs_subtitle") && w.contains("mkv")),
            "{:?}",
            m.warnings
        );
    }

    /// ASS styling does not survive mov_text/webvtt; carrying the text is
    /// right, but silence about the styling would not be.
    #[test]
    fn ass_subtitles_warn_about_styling_loss() {
        let m = invoke(Format::Mp4, &probe(Some("h264"), &["aac"], &["ass"], 0)).unwrap();
        assert!(has(&m.argv, ["-c:s", "mov_text"]), "{:?}", m.argv);
        assert!(
            m.warnings.iter().any(|w| w.contains("styling")),
            "{:?}",
            m.warnings
        );
    }

    /// F5: camera/GoPro timecode data streams break the matroska muxer, so
    /// the mkv remux keeps everything except them — and says so. mp4/mov
    /// get no such warning: their muxer regenerates the tmcd track from
    /// the copied video's side data, so nothing is actually lost there.
    #[test]
    fn data_stream_warnings_track_what_each_muxer_actually_does() {
        let m = invoke(Format::Mkv, &probe(Some("h264"), &["aac"], &[], 2)).unwrap();
        assert!(has(&m.argv, ["-map", "-0:d"]), "{:?}", m.argv);
        assert!(
            m.warnings.iter().any(|w| w.contains("timecode")),
            "{:?}",
            m.warnings
        );

        let mp4 = invoke(Format::Mp4, &probe(Some("h264"), &["aac"], &[], 1)).unwrap();
        assert!(
            !mp4.warnings.iter().any(|w| w.contains("timecode")),
            "mp4 regenerates tmcd; warning would be untrue: {:?}",
            mp4.warnings
        );

        let quiet = invoke(Format::Mkv, &probe(Some("h264"), &["aac"], &[], 0)).unwrap();
        assert!(quiet.warnings.is_empty(), "{:?}", quiet.warnings);
    }

    /// Subtitle codecs matroska has no ID for (xsub, unknown) are excluded
    /// per-index with a warning — ffmpeg's fallback (its default ASS
    /// encoder) dies on bitmap sources, which made avi-with-XSUB → mkv
    /// fail outright.
    #[test]
    fn mkv_excludes_subtitle_codecs_matroska_rejects() {
        let m = invoke(Format::Mkv, &probe(Some("mpeg4"), &["mp3"], &["xsub"], 0)).unwrap();
        assert!(has(&m.argv, ["-map", "-0:s:0"]), "{:?}", m.argv);
        assert!(
            m.warnings.iter().any(|w| w.contains("xsub")),
            "{:?}",
            m.warnings
        );
    }

    /// Text subtitles into mkv must be `-c:s copy` explicitly — without
    /// it, ffmpeg silently re-encodes them to its matroska default (ASS).
    #[test]
    fn mkv_copies_text_subtitles_explicitly() {
        let m = invoke(Format::Mkv, &probe(Some("vp9"), &["opus"], &["webvtt"], 0)).unwrap();
        assert!(has(&m.argv, ["-c:s", "copy"]), "{:?}", m.argv);
    }

    /// mkv's one subtitle re-encode: mov_text to SRT.
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

    /// A second video stream cannot ride into mp4/mov/webm's single
    /// `-map 0:v:0`; the loss must be named.
    #[test]
    fn additional_video_streams_are_warned_about() {
        let mut p = probe(Some("h264"), &["aac"], &[], 0);
        p.video_streams = 2;
        let m = invoke(Format::Mp4, &p).unwrap();
        assert!(
            m.warnings
                .iter()
                .any(|w| w.contains("additional video stream")),
            "{:?}",
            m.warnings
        );
    }

    /// Font attachments (mkv) cannot ride into mp4/mov/webm; the loss must
    /// be named. Into mkv they ride along silently — nothing is lost.
    #[test]
    fn attachment_streams_are_warned_about_for_non_mkv_targets() {
        let mut p = probe(Some("h264"), &["aac"], &[], 0);
        p.attachment_streams = 1;
        let m = invoke(Format::Mp4, &p).unwrap();
        assert!(
            m.warnings.iter().any(|w| w.contains("attachment")),
            "{:?}",
            m.warnings
        );
        let mkv = invoke(Format::Mkv, &p).unwrap();
        assert!(mkv.warnings.is_empty(), "{:?}", mkv.warnings);
    }

    /// The webm hybrid must coerce channel layouts for libopus (F4's
    /// surround-layout rejection applies to the hybrid path too) — and
    /// because filtering can't mix with stream copy, every track
    /// re-encodes there, with the warning saying so.
    #[test]
    fn webm_hybrid_coerces_channel_layouts_for_libopus() {
        let m = invoke(Format::Webm, &probe(Some("vp9"), &["ac3"], &[], 0)).unwrap();
        assert!(has(&m.argv, ["-c:a", "libopus"]), "{:?}", m.argv);
        assert!(
            m.argv.iter().any(|a| a.contains("aformat=channel_layouts")),
            "{:?}",
            m.argv
        );
        assert!(
            m.warnings.iter().any(|w| w.contains("All audio")),
            "{:?}",
            m.warnings
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
        let m = audio_invoke(
            Format::Mp4,
            Format::M4a,
            &probe(Some("h264"), &["aac"], &[], 0),
        )
        .unwrap();
        assert!(has(&m.argv, ["-map", "0:a:0"]), "{:?}", m.argv);
        assert!(has(&m.argv, ["-c:a", "copy"]), "{:?}", m.argv);
        assert!(!m.argv.contains(&"aac".to_string()), "{:?}", m.argv);
    }

    /// A codec the target can't hold as-is falls back to the static
    /// transcode recipe — as does a stream the probe could not identify
    /// (an `unknown` first track once slipped through as a "lossless"
    /// copy of bytes nothing could decode).
    #[test]
    fn non_matching_or_unknown_audio_falls_back_to_transcode() {
        assert!(audio_invoke(
            Format::Mp4,
            Format::Mp3,
            &probe(Some("h264"), &["aac"], &[], 0)
        )
        .is_none());
        assert!(audio_invoke(
            Format::Avi,
            Format::Wav,
            &probe(None, &["unknown", "pcm_s16le"], &[], 0)
        )
        .is_none());
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

        let m = audio_invoke(
            Format::Mp4,
            Format::M4a,
            &probe(Some("h264"), &["aac"], &[], 0),
        )
        .unwrap();
        assert!(
            !has(&m.argv, ["-map", "0:v?"]),
            "video sources drop video: {:?}",
            m.argv
        );
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

    // --- transcoded_invocation (video knobs) -----------------------------

    fn probe_h264_with(audios: &[&str], subs: &[&str]) -> MediaProbe {
        MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            audio_codecs: audios.iter().map(|s| (*s).to_string()).collect(),
            subtitle_codecs: subs.iter().map(|s| (*s).to_string()).collect(),
            width: Some(1920),
            height: Some(1080),
            frame_rate: Some((30, 1)),
            ..MediaProbe::default()
        }
    }

    fn chain() -> ResolvedVideo {
        ResolvedVideo {
            fps: Some("24".into()),
            ..Default::default()
        }
    }

    #[test]
    fn a_transcode_to_mp4_carries_an_encoder_and_a_pixel_format() {
        let m = transcoded_invocation(
            Format::Mp4,
            &probe_h264_with(&["aac"], &[]),
            &chain(),
            None,
            Path::new("in.mkv"),
            Path::new("out.mp4"),
        )
        .expect("mp4 is a transcodable target");
        assert!(
            m.argv.windows(2).any(|w| w == ["-c:v", "libx264"]),
            "{:?}",
            m.argv
        );
        // Without this, libx264 preserves a 10-bit source and emits High 10,
        // which a large share of hardware decoders refuse.
        assert!(
            m.argv.windows(2).any(|w| w == ["-pix_fmt", "yuv420p"]),
            "{:?}",
            m.argv
        );
        assert!(
            m.argv.windows(2).any(|w| w == ["-crf", "20"]),
            "{:?}",
            m.argv
        );
        // Not "no `copy` anywhere": an aac source into mp4 legitimately
        // keeps `-c:a copy` (see the next test) -- copying a compatible
        // audio stream is the whole point of the path this extends. Only
        // the video codec must not be `copy`.
        let c_v = m
            .argv
            .iter()
            .position(|a| a == "-c:v")
            .expect("-c:v present");
        assert_ne!(
            m.argv[c_v + 1],
            "copy",
            "video must not be copied: {:?}",
            m.argv
        );
    }

    #[test]
    fn a_transcode_keeps_every_audio_track_the_copy_path_would_have_kept() {
        // The cheap alternative -- falling through to VIDEO_TO_MP4 -- emits
        // -sn and relies on default stream selection, dropping subtitles
        // and every audio track past the first. That is the bug this file
        // exists to prevent.
        let m = transcoded_invocation(
            Format::Mp4,
            &probe_h264_with(&["aac", "ac3"], &["mov_text"]),
            &chain(),
            None,
            Path::new("in.mkv"),
            Path::new("out.mp4"),
        )
        .unwrap();
        // The mp4/mov/webm branch maps every audio track as one group,
        // `0:a?` -- the same wildcard `mp4_remux_maps_every_audio_track`
        // (the copy path's own flagship-bug test) checks for, and both aac
        // and ac3 are MP4_COMPATIBLE_AUDIO, so both ride under -c:a copy.
        // The brief's original assertion here checked for per-index maps
        // (-map 0:a:0 / -map 0:a:1), which this branch never emits even on
        // the copy path; corrected to match the mapping this file actually
        // produces.
        assert!(
            m.argv.windows(2).any(|w| w == ["-map", "0:a?"]),
            "{:?}",
            m.argv
        );
        assert!(
            m.argv.windows(2).any(|w| w == ["-map", "0:s:0"]),
            "{:?}",
            m.argv
        );
        assert!(!m.argv.iter().any(|a| a == "-sn"), "{:?}", m.argv);
    }

    #[test]
    fn a_transcode_to_mkv_tunes_only_the_first_video_stream() {
        // The mkv branch maps `-map 0`, which selects attached-picture
        // cover art that probe.video_streams deliberately does not count.
        // A global -c:v libx264 would re-encode album art as a video track.
        let m = transcoded_invocation(
            Format::Mkv,
            &probe_h264_with(&["aac"], &[]),
            &chain(),
            None,
            Path::new("in.mp4"),
            Path::new("out.mkv"),
        )
        .unwrap();
        assert!(
            m.argv.windows(2).any(|w| w == ["-c:v", "copy"]),
            "{:?}",
            m.argv
        );
        assert!(
            m.argv.windows(2).any(|w| w == ["-c:v:0", "libx264"]),
            "{:?}",
            m.argv
        );
        assert!(m.argv.iter().any(|a| a == "-filter:v:0"), "{:?}", m.argv);
        assert!(
            !m.argv.iter().any(|a| a == "-vf"),
            "mkv must scope the filter: {:?}",
            m.argv
        );
    }

    #[test]
    fn a_transcode_to_webm_uses_vp9_and_its_own_anchor() {
        let m = transcoded_invocation(
            Format::Webm,
            &probe_h264_with(&["aac"], &[]),
            &chain(),
            None,
            Path::new("in.mp4"),
            Path::new("out.webm"),
        )
        .unwrap();
        assert!(
            m.argv.windows(2).any(|w| w == ["-c:v", "libvpx-vp9"]),
            "{:?}",
            m.argv
        );
        assert!(
            m.argv.windows(2).any(|w| w == ["-crf", "32"]),
            "{:?}",
            m.argv
        );
        assert!(
            m.argv.windows(2).any(|w| w == ["-b:v", "0"]),
            "{:?}",
            m.argv
        );
    }

    #[test]
    fn a_user_crf_overrides_the_anchor() {
        let m = transcoded_invocation(
            Format::Mp4,
            &probe_h264_with(&["aac"], &[]),
            &chain(),
            Some(28),
            Path::new("in.mkv"),
            Path::new("out.mp4"),
        )
        .unwrap();
        assert!(
            m.argv.windows(2).any(|w| w == ["-crf", "28"]),
            "{:?}",
            m.argv
        );
    }

    #[test]
    fn a_target_with_no_encoder_declines() {
        assert!(transcoded_invocation(
            Format::Mp3,
            &probe_h264_with(&["aac"], &[]),
            &chain(),
            None,
            Path::new("in.mp4"),
            Path::new("out.mp3"),
        )
        .is_none());
    }

    #[test]
    fn the_argv_still_opens_with_the_input_and_closes_with_the_output() {
        // plan.rs:127 hardcodes path_args = vec![1, argv.len() - 1] on this
        // assumption; break it and the Windows verbatim-path rewriter
        // targets the wrong token.
        let m = transcoded_invocation(
            Format::Mp4,
            &probe_h264_with(&["aac"], &[]),
            &chain(),
            None,
            Path::new("in.mkv"),
            Path::new("out.mp4"),
        )
        .unwrap();
        assert_eq!(m.argv[0], "-i");
        assert_eq!(m.argv[1], "in.mkv");
        assert_eq!(m.argv.last().unwrap(), "out.mp4");
    }
}
