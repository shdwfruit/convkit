use std::path::Path;

use crate::error::{ConvError, ErrorCode, Result};
use crate::procutil::backend_command;

/// The codecs already present in a media file. `video_codec`/`audio_codec`
/// decide whether a container change can be a stream copy at all (see
/// `registry::can_remux`); `subtitle_codec` exists specifically for the
/// mkv remux decision (see `registry::mkv_remux_for`) -- matroska has no
/// codec ID for `mov_text` (mp4/mov's own subtitle codec), even though it
/// happily copies almost everything else, so `-map 0 -c copy` needs to know
/// up front whether it's about to hit that one incompatibility.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaProbe {
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub subtitle_codec: Option<String>,
}

/// Parses `ffprobe -show_streams` JSON. Any malformed input yields an empty
/// probe, which callers treat as "unknown" and therefore transcode.
pub fn parse(json: &str) -> MediaProbe {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return MediaProbe::default();
    };
    let streams = v.get("streams").and_then(|s| s.as_array());
    let find = |kind: &str| -> Option<String> {
        streams?
            .iter()
            .find(|s| s.get("codec_type").and_then(|t| t.as_str()) == Some(kind))
            .and_then(|s| s.get("codec_name"))
            .and_then(|n| n.as_str())
            .map(str::to_owned)
    };
    MediaProbe {
        video_codec: find("video"),
        audio_codec: find("audio"),
        subtitle_codec: find("subtitle"),
    }
}

/// Runs ffprobe. This is the one place in core that spawns a process outside
/// `exec`, because plan construction needs the answer before it can choose a
/// recipe.
pub fn run(ffprobe: &Path, input: &Path) -> Result<MediaProbe> {
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
        assert_eq!(p.audio_codec.as_deref(), Some("aac"));
    }

    #[test]
    fn tolerates_a_file_with_no_audio() {
        let p = parse(r#"{"streams":[{"codec_type":"video","codec_name":"vp9"}]}"#);
        assert_eq!(p.video_codec.as_deref(), Some("vp9"));
        assert_eq!(p.audio_codec, None);
    }

    #[test]
    fn malformed_json_yields_an_empty_probe_rather_than_failing() {
        let p = parse("not json");
        assert_eq!(p.video_codec, None);
        assert_eq!(p.audio_codec, None);
        assert_eq!(p.subtitle_codec, None);
    }

    #[test]
    fn extracts_the_first_subtitle_codec_too() {
        let p = parse(
            r#"{"streams":[
            {"codec_type":"video","codec_name":"h264"},
            {"codec_type":"audio","codec_name":"aac"},
            {"codec_type":"subtitle","codec_name":"mov_text"}]}"#,
        );
        assert_eq!(p.subtitle_codec.as_deref(), Some("mov_text"));
    }

    #[test]
    fn tolerates_a_file_with_no_subtitle_track() {
        let p = parse(SAMPLE);
        assert_eq!(p.subtitle_codec, None);
    }
}
