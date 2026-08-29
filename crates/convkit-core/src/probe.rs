use std::path::Path;
use std::process::Command;

use crate::error::{ConvError, ErrorCode, Result};

/// The codecs already present in a media file. Used only to decide whether a
/// container change can be a stream copy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaProbe {
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
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
    }
}

/// Runs ffprobe. This is the one place in core that spawns a process outside
/// `exec`, because plan construction needs the answer before it can choose a
/// recipe.
pub fn run(ffprobe: &Path, input: &Path) -> Result<MediaProbe> {
    let out = Command::new(ffprobe)
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
    }
}
