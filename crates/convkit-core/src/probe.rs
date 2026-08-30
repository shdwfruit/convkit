use std::path::Path;

use crate::error::{ConvError, ErrorCode, Result};
use crate::procutil::backend_command;

/// The full stream inventory of a media file, not just the first stream of
/// each type: the remux decision has to hold for *every* stream a container
/// change would carry, or a `-c copy` approved off stream 1 ships a DTS
/// track on stream 2 that most players can't decode. `video_codec`/
/// `audio_codec`/`subtitle_codec` remain the firsts, both for callers that
/// only need one answer and for tests that build probes by hand;
/// `all_audio`/`all_subtitles` are what stream-mapping decisions consult.
/// `data_streams` counts tmcd/mebx/gpmd-style timecode and metadata tracks
/// (camera/GoPro/iPhone footage), which some muxers reject outright and
/// explicit mapping must deliberately exclude. `pix_fmt`/`color_transfer`
/// describe the first real video stream, so a plan can recognise an HDR
/// (PQ/HLG) source that needs tonemapping before an SDR target.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaProbe {
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub subtitle_codec: Option<String>,
    /// Every audio stream's codec, in stream order.
    pub audio_codecs: Vec<String>,
    /// Every subtitle stream's codec, in stream order.
    pub subtitle_codecs: Vec<String>,
    /// How many data (timecode/metadata) streams the source carries.
    pub data_streams: usize,
    pub pix_fmt: Option<String>,
    pub color_transfer: Option<String>,
}

impl MediaProbe {
    /// Every audio codec seen, falling back to the legacy single field for
    /// probes constructed by hand with only `audio_codec` set.
    pub fn all_audio(&self) -> Vec<&str> {
        if self.audio_codecs.is_empty() {
            self.audio_codec.as_deref().into_iter().collect()
        } else {
            self.audio_codecs.iter().map(String::as_str).collect()
        }
    }

    /// Every subtitle codec seen, with the same fallback as `all_audio`.
    pub fn all_subtitles(&self) -> Vec<&str> {
        if self.subtitle_codecs.is_empty() {
            self.subtitle_codec.as_deref().into_iter().collect()
        } else {
            self.subtitle_codecs.iter().map(String::as_str).collect()
        }
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
        let name = s
            .get("codec_name")
            .and_then(|n| n.as_str())
            .map(str::to_owned);
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
                if !attached_pic && p.video_codec.is_none() {
                    p.video_codec = name;
                    p.pix_fmt = s
                        .get("pix_fmt")
                        .and_then(|f| f.as_str())
                        .map(str::to_owned);
                    p.color_transfer = s
                        .get("color_transfer")
                        .and_then(|f| f.as_str())
                        .map(str::to_owned);
                }
            }
            "audio" => {
                if let Some(name) = name {
                    if p.audio_codec.is_none() {
                        p.audio_codec = Some(name.clone());
                    }
                    p.audio_codecs.push(name);
                }
            }
            "subtitle" => {
                if let Some(name) = name {
                    if p.subtitle_codec.is_none() {
                        p.subtitle_codec = Some(name.clone());
                    }
                    p.subtitle_codecs.push(name);
                }
            }
            "data" => p.data_streams += 1,
            _ => {}
        }
    }
    p
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
