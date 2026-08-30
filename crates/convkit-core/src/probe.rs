use std::path::Path;

use crate::error::{ConvError, ErrorCode, Result};
use crate::procutil::backend_command;

/// The full stream inventory of a media file, not just the first stream of
/// each type: the remux decision has to hold for *every* stream a container
/// change would carry, or a `-c copy` approved off stream 1 ships a DTS
/// track on stream 2 that most players can't decode.
/// `all_audio`/`all_subtitles` are what stream-mapping decisions consult.
/// `data_streams` counts tmcd/mebx/gpmd-style timecode and metadata tracks
/// (camera/GoPro/iPhone footage), which some muxers reject outright and
/// explicit mapping must deliberately exclude. `color_transfer` describes
/// the first real video stream, so a plan can recognise an HDR (PQ/HLG)
/// source that needs tonemapping before an SDR target.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaProbe {
    /// First real video stream's codec (attached-picture cover art
    /// excluded) — genuinely a scalar, unlike audio/subtitles, which the
    /// stream-mapping decisions consume in full.
    pub video_codec: Option<String>,
    /// Every audio stream's codec, in stream order. A stream ffprobe saw
    /// but could not name is recorded as `"unknown"` — never skipped:
    /// skipping would desynchronise these indices from the real `0:a:N`
    /// positions the stream-mapping argv is built against, and `"unknown"`
    /// appears in no compatibility allowlist, so every gate rejects it.
    pub audio_codecs: Vec<String>,
    /// Every subtitle stream's codec, in stream order — same `"unknown"`
    /// placeholder rule as `audio_codecs`.
    pub subtitle_codecs: Vec<String>,
    /// How many data (timecode/metadata) streams the source carries.
    pub data_streams: usize,
    /// How many real video streams (attached-picture cover art excluded).
    pub video_streams: usize,
    /// How many attachment streams (fonts in mkv).
    pub attachment_streams: usize,
    pub color_transfer: Option<String>,
}

impl MediaProbe {
    /// The first audio stream's codec — derived, never stored, so it can
    /// never drift out of sync with `audio_codecs`.
    pub fn audio_codec(&self) -> Option<&str> {
        self.audio_codecs.first().map(String::as_str)
    }

    /// Every audio codec seen, in stream order.
    pub fn all_audio(&self) -> Vec<&str> {
        self.audio_codecs.iter().map(String::as_str).collect()
    }

    /// Every subtitle codec seen, in stream order.
    pub fn all_subtitles(&self) -> Vec<&str> {
        self.subtitle_codecs.iter().map(String::as_str).collect()
    }

    /// Whether the video stream is HDR: PQ (smpte2084) or HLG
    /// (arib-std-b67) transfer characteristics — the two transfers default
    /// iPhone and HDR-YouTube footage actually carries. SDR targets need a
    /// tonemap for these or the output comes out grey and hue-shifted.
    pub fn is_hdr(&self) -> bool {
        matches!(
            self.color_transfer.as_deref(),
            Some("smpte2084") | Some("arib-std-b67")
        )
    }
}

/// Parses `ffprobe -show_streams` JSON. Any malformed input yields an empty
/// probe, which callers treat as "unknown" and therefore transcode.
pub fn parse(json: &str) -> MediaProbe {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return MediaProbe::default();
    };
    let Some(streams) = v.get("streams").and_then(|s| s.as_array()) else {
        return MediaProbe::default();
    };

    let mut p = MediaProbe::default();
    for s in streams {
        let kind = s.get("codec_type").and_then(|t| t.as_str()).unwrap_or("");
        // ffprobe omits codec_name entirely for codecs it cannot identify;
        // see `audio_codecs`' docs for why that becomes a placeholder
        // rather than a skipped entry.
        let name = s
            .get("codec_name")
            .and_then(|n| n.as_str())
            .unwrap_or("unknown")
            .to_owned();
        match kind {
            "video" => {
                // Cover art embedded in an audio file (mp3/m4a/flac) shows
                // up as a video stream with the attached_pic disposition;
                // it must not masquerade as the file's video track.
                let attached_pic = s
                    .get("disposition")
                    .and_then(|d| d.get("attached_pic"))
                    .and_then(|a| a.as_i64())
                    == Some(1);
                if !attached_pic {
                    p.video_streams += 1;
                    if p.video_codec.is_none() {
                        p.video_codec = Some(name);
                        p.color_transfer = s
                            .get("color_transfer")
                            .and_then(|f| f.as_str())
                            .map(str::to_owned);
                    }
                }
            }
            "audio" => p.audio_codecs.push(name),
            "subtitle" => p.subtitle_codecs.push(name),
            "data" => p.data_streams += 1,
            "attachment" => p.attachment_streams += 1,
            _ => {}
        }
    }
    p
}

/// Runs ffprobe. This is the one place in core that spawns a process outside
/// `exec`, because plan construction needs the answer before it can choose a
/// recipe.
///
/// Refuses anything that is not an existing regular file *here, in core*:
/// ffprobe honours URLs and device paths, so probing a raw user-supplied
/// path is an outbound-fetch primitive. Callers may keep their own gates
/// as fast paths, but the invariant lives where every future caller —
/// including a `conv mcp` frontend — inherits it, the same reasoning that
/// put refuse-by-default overwrite into `exec::run` (I5).
pub fn run(ffprobe: &Path, input: &Path) -> Result<MediaProbe> {
    if !input.is_file() {
        return Err(ConvError::new(
            ErrorCode::InputNotFound,
            format!(
                "not an existing regular file, refusing to probe: {}",
                input.display()
            ),
        ));
    }
    // Windows console-window suppression (`CREATE_NO_WINDOW`) is applied
    // inside `backend_command`, not repeated here -- see its docs.
    let out = backend_command(ffprobe)
        .args(["-v", "quiet", "-print_format", "json", "-show_streams"])
        .arg(input)
        .output()
        .map_err(|e| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("failed to run ffprobe: {e}"),
            )
        })?;
    Ok(parse(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"streams":[
        {"codec_type":"video","codec_name":"h264"},
        {"codec_type":"audio","codec_name":"aac"}]}"#;

    #[test]
    fn extracts_first_video_and_audio_codec() {
        let p = parse(SAMPLE);
        assert_eq!(p.video_codec.as_deref(), Some("h264"));
        assert_eq!(p.audio_codec(), Some("aac"));
    }

    #[test]
    fn tolerates_a_file_with_no_audio() {
        let p = parse(r#"{"streams":[{"codec_type":"video","codec_name":"vp9"}]}"#);
        assert_eq!(p.video_codec.as_deref(), Some("vp9"));
        assert_eq!(p.audio_codec(), None);
    }

    #[test]
    fn malformed_json_yields_an_empty_probe_rather_than_failing() {
        let p = parse("not json");
        assert_eq!(p.video_codec, None);
        assert_eq!(p.audio_codec(), None);
        assert!(p.subtitle_codecs.is_empty());
    }

    #[test]
    fn extracts_the_first_subtitle_codec_too() {
        let p = parse(
            r#"{"streams":[
            {"codec_type":"video","codec_name":"h264"},
            {"codec_type":"audio","codec_name":"aac"},
            {"codec_type":"subtitle","codec_name":"mov_text"}]}"#,
        );
        assert_eq!(
            p.subtitle_codecs.first().map(String::as_str),
            Some("mov_text")
        );
    }

    #[test]
    fn tolerates_a_file_with_no_subtitle_track() {
        let p = parse(SAMPLE);
        assert!(p.subtitle_codecs.is_empty());
    }

    /// The full inventory: every audio/subtitle codec in stream order,
    /// data and attachment counts, and cover art excluded from the video
    /// count.
    #[test]
    fn records_the_full_stream_inventory() {
        let p = parse(
            r#"{"streams":[
            {"codec_type":"video","codec_name":"h264","pix_fmt":"yuv420p10le","color_transfer":"smpte2084"},
            {"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}},
            {"codec_type":"audio","codec_name":"aac"},
            {"codec_type":"audio","codec_name":"dts"},
            {"codec_type":"subtitle","codec_name":"subrip"},
            {"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle"},
            {"codec_type":"data","codec_name":"tmcd"},
            {"codec_type":"attachment","codec_name":"ttf"}]}"#,
        );
        assert_eq!(p.audio_codecs, vec!["aac", "dts"]);
        assert_eq!(p.subtitle_codecs, vec!["subrip", "hdmv_pgs_subtitle"]);
        assert_eq!(p.data_streams, 1);
        assert_eq!(p.attachment_streams, 1);
        assert_eq!(p.video_streams, 1, "cover art is not a video stream");
        assert!(p.is_hdr());
    }

    /// ffprobe omits codec_name entirely for codecs it cannot identify.
    /// Skipping such a stream would desynchronise `audio_codecs` indices
    /// from the real `0:a:N` positions the stream-mapping argv is built
    /// against — demonstrated as a silent-corruption path where the copy
    /// gate approved stream 0 off stream 1's codec. It must become an
    /// `"unknown"` placeholder instead.
    #[test]
    fn nameless_streams_become_unknown_placeholders_not_gaps() {
        let p = parse(
            r#"{"streams":[
            {"codec_type":"audio"},
            {"codec_type":"audio","codec_name":"pcm_s16le"}]}"#,
        );
        assert_eq!(p.audio_codecs, vec!["unknown", "pcm_s16le"]);
        assert_eq!(p.audio_codec(), Some("unknown"));
    }
}
