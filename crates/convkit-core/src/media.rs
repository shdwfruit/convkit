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
    /// One pass (`pass` is 1 or 2) of a bitrate-targeted re-encode, for
    /// `--max-size`. Pass 2 is the real conversion. Pass 1 uses this only
    /// for mkv, where it has to lay out the same streams as pass 2 (see
    /// `two_pass_invocations`); the other targets build their pass 1
    /// directly.
    TwoPass {
        pass: u8,
        chain: &'a str,
        encoder: &'static str,
        bitrate: String,
        passlog: String,
        companions: &'static [&'static str],
    },
}

/// What happens to the audio tracks.
#[derive(Debug, Clone, Copy)]
pub(crate) enum AudioDisposition {
    /// Copy what the target holds, re-encode only what it does not.
    Fit,
    /// Re-encode every track at this rate, so its size is known in advance.
    Reencode { kbps: u32 },
    /// Stream-copy every track, whether or not the target would keep it.
    /// For a pass whose output is discarded.
    Copy,
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
        VideoDisposition::TwoPass {
            pass,
            chain,
            encoder,
            bitrate,
            passlog,
            companions,
        } => {
            let pass = pass.to_string();
            // Scoped to stream 0 on mkv for the same reason the codec is
            // (see this function's docs): -map 0 also selects cover art,
            // which is copied, not encoded.
            if to == Format::Mkv {
                push(argv, &["-filter:v:0", chain]);
                push(argv, &["-c:v", "copy"]);
                push(argv, &["-c:v:0", encoder]);
                push(argv, &["-b:v:0", bitrate.as_str()]);
                push(argv, &["-pass:v:0", pass.as_str()]);
                push(argv, &["-passlogfile:v:0", passlog.as_str()]);
                push(argv, &["-fps_mode:v:0", two_pass_fps_mode(to)]);
            } else {
                push(argv, &["-vf", chain]);
                push(argv, &["-c:v", encoder]);
                push(argv, &["-b:v", bitrate.as_str()]);
                push(argv, &["-pass", pass.as_str()]);
                push(argv, &["-passlogfile", passlog.as_str()]);
                push(argv, &["-fps_mode", two_pass_fps_mode(to)]);
            }
            push(argv, companions);
        }
    }
}

/// Emits the audio arguments for the chosen disposition.
fn push_audio_args(
    argv: &mut Vec<String>,
    warnings: &mut Vec<String>,
    to: Format,
    audio_ok: &[&str],
    audios: &[&str],
    audio: AudioDisposition,
) {
    match audio {
        AudioDisposition::Fit => audio_codec_args(argv, warnings, to, audio_ok, audios),
        AudioDisposition::Reencode { kbps } => reencode_audio_args(argv, to, kbps, audios.len()),
        AudioDisposition::Copy => push(argv, &["-c:a", "copy"]),
    }
}

/// Every audio track at one known rate, so the audio's size is arithmetic. A
/// source with no audio gets no audio arguments at all.
fn reencode_audio_args(argv: &mut Vec<String>, to: Format, kbps: u32, tracks: usize) {
    if tracks == 0 {
        return;
    }
    let rate = format!("{kbps}k");
    if to == Format::Webm {
        push(argv, &["-c:a", "libopus", "-b:a", &rate]);
        push(argv, &["-af", registry::OPUS_CHANNEL_LAYOUTS]);
    } else {
        push(argv, &["-c:a", "aac", "-b:a", &rate]);
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
    audio: AudioDisposition,
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
        push_audio_args(&mut argv, &mut warnings, to, audio_ok, &audios, audio);

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
        push_audio_args(&mut argv, &mut warnings, to, audio_ok, &audios, audio);

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
    mapped_invocation(
        to,
        probe,
        VideoDisposition::Copy,
        AudioDisposition::Fit,
        input,
        output,
    )
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
        AudioDisposition::Fit,
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

/// Both passes of a bitrate-targeted encode, for `--max-size`.
///
/// Pass 1 runs the same filter chain, encoder settings and frame-rate mode
/// as pass 2 (the statistics are only valid if it does) and writes nothing
/// but the pass
/// log: `-f null -` works the same on every platform, so there is no
/// `/dev/null` versus `NUL` branch. Pass 2 is the ordinary stream mapping
/// with the rate set by bitrate rather than CRF, and every audio track
/// re-encoded at `audio_kbps` so its size is known. `audio_kbps` is `None`
/// only for a source with no audio; a rate given for one is ignored.
///
/// ffmpeg names the pass log by the encoded stream's index in the output. For
/// mp4, mov and webm both passes map `0:v:0` first, so that index is 0 in
/// each. mkv maps everything (`-map 0`), which can put audio ahead of the
/// video, so its pass 1 is built from the same mapping as pass 2, with the
/// audio copied and the output discarded; the video then lands on the same
/// index in both passes and pass 2 finds the log pass 1 wrote.
#[allow(clippy::too_many_arguments)] // each is a distinct input to one argv
pub(crate) fn two_pass_invocations(
    to: Format,
    probe: &MediaProbe,
    resolved: &ResolvedVideo,
    video_bps: u64,
    audio_kbps: Option<u32>,
    passlog: &Path,
    input: &Path,
    output: &Path,
) -> Option<TwoPass> {
    registry::compat_tables(to)?;
    probe.video_codec.as_deref()?;
    let (encoder, companions): (&'static str, &'static [&'static str]) = match to {
        Format::Mp4 | Format::Mov | Format::Mkv => ("libx264", &["-pix_fmt", "yuv420p"]),
        // No `-b:v 0`: that is libvpx's constant-quality switch, the
        // opposite of a bitrate target.
        Format::Webm => ("libvpx-vp9", &["-row-mt", "1", "-threads", "0"]),
        _ => return None,
    };
    let chain = registry::TRANSCODE_CHAIN.compose(resolved);
    let bitrate = video_bps.to_string();
    let log = passlog.to_string_lossy().into_owned();
    let video = |pass| VideoDisposition::TwoPass {
        pass,
        chain: &chain,
        encoder,
        bitrate: bitrate.clone(),
        passlog: log.clone(),
        companions,
    };

    let pass1 = if to == Format::Mkv {
        let mut argv =
            mapped_invocation(to, probe, video(1), AudioDisposition::Copy, input, output)?.argv;
        // The mapping ends `-y <output>`; pass 1 writes nowhere.
        argv.pop();
        push(&mut argv, &["-f", "null", "-"]);
        argv
    } else {
        let mut argv: Vec<String> = vec!["-i".into(), input.to_string_lossy().into_owned()];
        push(
            &mut argv,
            &["-map", "0:v:0", "-vf", &chain, "-c:v", encoder],
        );
        push(
            &mut argv,
            &["-b:v", &bitrate, "-pass", "1", "-passlogfile", &log],
        );
        push(&mut argv, &["-fps_mode", two_pass_fps_mode(to)]);
        push(&mut argv, companions);
        push(&mut argv, &["-an", "-sn", "-dn", "-f", "null", "-"]);
        argv
    };

    let audio = match audio_kbps {
        Some(kbps) => AudioDisposition::Reencode { kbps },
        None => AudioDisposition::Fit,
    };
    let pass2 = mapped_invocation(to, probe, video(2), audio, input, output)?;
    Some(TwoPass { pass1, pass2 })
}

/// The frame-rate mode both passes of a two-pass encode run at: the one
/// ffmpeg picks for `to` itself, a constant rate for mp4 and mov (their
/// muxer has no variable-rate flag) and the timestamps as they are for
/// Matroska. Left to choose, pass 1's null muxer takes neither, and ffmpeg
/// 6.1 then encodes a different number of frames in each pass: x264's
/// statistics come up short and pass 2 hangs or crashes. `-fps_mode` is
/// ffmpeg 5.1's name for the option; 9.0 no longer takes `-vsync`.
fn two_pass_fps_mode(to: Format) -> &'static str {
    match to {
        Format::Mp4 | Format::Mov => "cfr",
        _ => "vfr",
    }
}

/// Pass 1's argv and pass 2's full invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TwoPass {
    pub pass1: Vec<String>,
    pub pass2: MediaInvocation,
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
             The video itself is untouched by this: stream-copied, or already \
             transcoded if a video knob requested one.",
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
         is stream-copied untouched. The video is unaffected by this: it is \
         stream-copied, or already transcoded if a video knob requested one.",
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
        // Not per-index maps (-map 0:a:0 / -map 0:a:1): this branch never
        // emits those, even on the copy path.
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

    fn h264_aac() -> MediaProbe {
        MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            audio_codecs: vec!["aac".into()],
            audio_bitrates: vec![Some(160_000)],
            ..MediaProbe::default()
        }
    }

    fn resolved_720p15() -> ResolvedVideo {
        ResolvedVideo {
            fps: Some("15/1".into()),
            scale: Some("scale=w=1280:h=720".into()),
            notes: vec![],
            keep_source_rate: false,
            keep_source_size: false,
            enlarged: None,
            ..ResolvedVideo::default()
        }
    }

    #[test]
    fn mp4_two_pass_argv_is_exactly_the_documented_shape() {
        let t = two_pass_invocations(
            Format::Mp4,
            &h264_aac(),
            &resolved_720p15(),
            1_190_000,
            Some(96),
            Path::new("/s/out.convkit-pass"),
            Path::new("in.mov"),
            Path::new("/s/out.mp4"),
        )
        .unwrap();
        let chain = "fps=15/1,scale=w=1280:h=720,scale=trunc(iw/2)*2:trunc(ih/2)*2";
        assert_eq!(
            t.pass1.join(" "),
            format!(
                "-i in.mov -map 0:v:0 -vf {chain} -c:v libx264 -b:v 1190000 -pass 1 \
                 -passlogfile /s/out.convkit-pass -fps_mode cfr -pix_fmt yuv420p \
                 -an -sn -dn -f null -"
            )
        );
        assert_eq!(
            t.pass2.argv.join(" "),
            format!(
                "-i in.mov -map 0:v:0 -map 0:a? -vf {chain} -c:v libx264 -b:v 1190000 \
                 -pass 2 -passlogfile /s/out.convkit-pass -fps_mode cfr -pix_fmt yuv420p \
                 -c:a aac -b:a 96k -movflags +faststart -y /s/out.mp4"
            )
        );
    }

    /// Pass 2's statistics are read frame by frame, so both passes must
    /// encode the same frames. Left to itself, ffmpeg picks a frame-rate
    /// mode from the output format: timestamps as they are for pass 1's
    /// null muxer, a constant rate for mp4. ffmpeg 6.1 then duplicates a
    /// frame in pass 2 alone, x264 finds its statistics a frame short, and
    /// ffmpeg hangs or crashes. Both passes name the mode pass 2's format
    /// would pick, so neither is left to choose.
    #[test]
    fn both_passes_encode_at_the_same_frame_rate_mode() {
        for (to, flag, mode) in [
            (Format::Mp4, "-fps_mode", "cfr"),
            (Format::Mov, "-fps_mode", "cfr"),
            (Format::Mkv, "-fps_mode:v:0", "vfr"),
            (Format::Webm, "-fps_mode", "vfr"),
        ] {
            let t = two_pass_invocations(
                to,
                &h264_aac(),
                &ResolvedVideo::default(),
                500_000,
                Some(96),
                Path::new("/s/o.convkit-pass"),
                Path::new("in.mkv"),
                Path::new("/s/o"),
            )
            .unwrap();
            for argv in [&t.pass1, &t.pass2.argv] {
                assert!(has(argv, [flag, mode]), "{to:?}: {argv:?}");
                let modes = argv.iter().filter(|x| x.starts_with("-fps_mode")).count();
                assert_eq!(modes, 1, "{to:?}: {argv:?}");
            }
        }
    }

    #[test]
    fn mkv_scopes_the_rate_options_to_the_encoded_stream() {
        let t = two_pass_invocations(
            Format::Mkv,
            &h264_aac(),
            &ResolvedVideo::default(),
            800_000,
            Some(128),
            Path::new("/s/o.convkit-pass"),
            Path::new("in.mp4"),
            Path::new("/s/o.mkv"),
        )
        .unwrap();
        let a = &t.pass2.argv;
        assert!(has(a, ["-map", "0"]), "{a:?}");
        assert!(has(a, ["-c:v:0", "libx264"]), "{a:?}");
        assert!(has(a, ["-b:v:0", "800000"]), "{a:?}");
        assert!(has(a, ["-pass:v:0", "2"]), "{a:?}");
        assert!(has(a, ["-passlogfile:v:0", "/s/o.convkit-pass"]), "{a:?}");
        assert!(!a.iter().any(|x| x == "-crf"), "{a:?}");
        // ffmpeg takes the last matching per-stream option, so the blanket
        // copy has to come first and the scoped encoder after it.
        let at = |pair| a.windows(2).position(|w| w == pair).unwrap();
        assert!(at(["-c:v", "copy"]) < at(["-c:v:0", "libx264"]), "{a:?}");
    }

    fn maps(argv: &[String]) -> Vec<&str> {
        argv.windows(2)
            .filter(|w| w[0] == "-map")
            .map(|w| w[1].as_str())
            .collect()
    }

    /// ffmpeg names the pass log by the encoded stream's output index, and
    /// mkv's `-map 0` can put audio ahead of the video. Pass 1 therefore
    /// has to lay out the same streams as pass 2, or pass 2 looks for a log
    /// pass 1 never wrote.
    #[test]
    fn mkv_pass_one_lays_out_the_same_streams_as_pass_two() {
        let probe = MediaProbe {
            audio_codecs: vec!["aac".into(), "aac".into()],
            audio_bitrates: vec![Some(160_000), Some(96_000)],
            subtitle_codecs: vec!["subrip".into(), "xsub".into()],
            data_streams: 1,
            ..h264_aac()
        };
        let t = two_pass_invocations(
            Format::Mkv,
            &probe,
            &ResolvedVideo::default(),
            800_000,
            Some(128),
            Path::new("/s/o.convkit-pass"),
            Path::new("in.mkv"),
            Path::new("/s/o.mkv"),
        )
        .unwrap();
        let (p1, p2) = (&t.pass1, &t.pass2.argv);
        assert_eq!(maps(p1), ["0", "-0:d", "-0:s:1"], "{p1:?}");
        assert_eq!(maps(p1), maps(p2));
        assert!(has(p1, ["-pass:v:0", "1"]), "{p1:?}");
        assert!(has(p2, ["-pass:v:0", "2"]), "{p2:?}");
        assert!(!has(p1, ["-pass:v:0", "2"]) && !has(p2, ["-pass:v:0", "1"]));
        for a in [p1, p2] {
            assert!(has(a, ["-passlogfile:v:0", "/s/o.convkit-pass"]), "{a:?}");
            assert!(has(a, ["-b:v:0", "800000"]), "{a:?}");
        }
        // Pass 1 has no use for the audio, so it copies it rather than
        // spending time on an encode whose output is thrown away.
        assert!(has(p1, ["-c:a", "copy"]), "{p1:?}");
        assert!(!p1.iter().any(|x| x == "-b:a"), "{p1:?}");
        assert!(has(p1, ["-f", "null"]), "{p1:?}");
        assert_eq!(p1.last().unwrap(), "-");
        assert!(!p1.iter().any(|x| x.ends_with("o.mkv")), "{p1:?}");
    }

    #[test]
    fn webm_two_pass_drops_constant_quality_and_sizes_opus() {
        let t = two_pass_invocations(
            Format::Webm,
            &h264_aac(),
            &ResolvedVideo::default(),
            500_000,
            Some(64),
            Path::new("/s/o.convkit-pass"),
            Path::new("in.mp4"),
            Path::new("/s/o.webm"),
        )
        .unwrap();
        for argv in [&t.pass1, &t.pass2.argv] {
            assert!(has(argv, ["-c:v", "libvpx-vp9"]), "{argv:?}");
            assert!(has(argv, ["-b:v", "500000"]), "{argv:?}");
            assert!(
                !has(argv, ["-b:v", "0"]),
                "constant-quality mode must be off: {argv:?}"
            );
            assert!(!argv.iter().any(|x| x == "-crf"), "{argv:?}");
        }
        assert!(has(&t.pass2.argv, ["-c:a", "libopus"]));
        assert!(has(&t.pass2.argv, ["-b:a", "64k"]));
        assert!(has(&t.pass2.argv, ["-af", registry::OPUS_CHANNEL_LAYOUTS]));
    }

    #[test]
    fn a_silent_source_gets_no_audio_rate() {
        let silent = MediaProbe {
            audio_codecs: vec![],
            audio_bitrates: vec![],
            ..h264_aac()
        };
        let t = two_pass_invocations(
            Format::Mp4,
            &silent,
            &ResolvedVideo::default(),
            500_000,
            None,
            Path::new("p"),
            Path::new("in.mp4"),
            Path::new("o.mp4"),
        )
        .unwrap();
        assert!(
            !t.pass2.argv.iter().any(|x| x == "-b:a"),
            "{:?}",
            t.pass2.argv
        );
    }

    /// A rate handed to a source with nothing to encode at it is ignored,
    /// not turned into an audio encode that has no stream to act on.
    #[test]
    fn a_rate_on_a_silent_source_adds_no_audio_arguments() {
        let silent = MediaProbe {
            audio_codecs: vec![],
            audio_bitrates: vec![],
            ..h264_aac()
        };
        for to in [Format::Mp4, Format::Mkv, Format::Webm] {
            let t = two_pass_invocations(
                to,
                &silent,
                &ResolvedVideo::default(),
                500_000,
                Some(96),
                Path::new("p"),
                Path::new("in.mp4"),
                Path::new("o"),
            )
            .unwrap();
            for flag in ["-c:a", "-b:a", "-af"] {
                assert!(
                    !t.pass2.argv.iter().any(|x| x == flag),
                    "{to:?} {flag}: {:?}",
                    t.pass2.argv
                );
            }
        }
    }

    #[test]
    fn every_audio_track_shares_one_reencode_rate() {
        let two_tracks = MediaProbe {
            audio_codecs: vec!["aac".into(), "ac3".into()],
            audio_bitrates: vec![Some(160_000), Some(384_000)],
            ..h264_aac()
        };
        let t = two_pass_invocations(
            Format::Mp4,
            &two_tracks,
            &ResolvedVideo::default(),
            500_000,
            Some(128),
            Path::new("p"),
            Path::new("in.mp4"),
            Path::new("o.mp4"),
        )
        .unwrap();
        let a = &t.pass2.argv;
        let count = |flag: &str| a.iter().filter(|x| *x == flag).count();
        // Unindexed options apply to every mapped audio stream.
        assert_eq!((count("-c:a"), count("-b:a")), (1, 1), "{a:?}");
        assert!(has(a, ["-c:a", "aac"]) && has(a, ["-b:a", "128k"]), "{a:?}");
    }

    #[test]
    fn non_video_targets_and_videoless_sources_get_none() {
        let args = |to, probe: &MediaProbe| {
            two_pass_invocations(
                to,
                probe,
                &ResolvedVideo::default(),
                1,
                None,
                Path::new("p"),
                Path::new("i"),
                Path::new("o"),
            )
        };
        assert!(args(Format::Gif, &h264_aac()).is_none());
        let no_video = MediaProbe {
            video_codec: None,
            ..h264_aac()
        };
        assert!(args(Format::Mp4, &no_video).is_none());
    }
}
