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
    /// Stored width of the first real video stream. *Stored*, not
    /// displayed: ffmpeg autorotates before the user filter chain, so a
    /// cap must be decided against `display_dimensions()` instead.
    pub width: Option<u32>,
    /// Stored height, with the same caveat as `width`.
    pub height: Option<u32>,
    /// Display-matrix rotation in degrees, when the source carries one.
    pub rotation: Option<i32>,
    /// Frame rate as a rational, never a float: `MediaProbe` derives `Eq`,
    /// which a float member would stop deriving, and ffprobe reports
    /// `30000/1001` as a rational already — the float would be the lossy
    /// form. `None` when neither reported rate is usable.
    pub frame_rate: Option<(u32, u32)>,
    /// Container duration in whole milliseconds, from `-show_format`. Held
    /// as an integer for the same reason `frame_rate` is a rational: the
    /// struct derives `Eq`. `None` for a live stream or a truncated file.
    pub duration_ms: Option<u64>,
    /// The file's size in bytes as ffprobe reports it (`format.size`).
    pub size_bytes: Option<u64>,
    /// Which input or page `width` and `height` are of, when an image
    /// conversion takes several: the one a `--resize --upscale` enlarges
    /// most. `None` for a single picture.
    pub label: Option<String>,
    /// Each audio stream's bitrate in bits per second, in stream order and
    /// always the same length as `audio_codecs`, `None` where the container
    /// does not say (mkv usually does not) or reports zero.
    pub audio_bitrates: Vec<Option<u32>>,
    /// Bytes carried by attachment streams (fonts in mkv), which a remux
    /// or re-encode passes through untouched.
    pub attachment_bytes: u64,
    /// What an image source holds that a single-image target drops, read
    /// by `image_traits`. `None` when the source was not read.
    pub image: Option<ImageTraits>,
    /// The container's tags that `--strip-metadata` keeps
    /// (`metadata::KEPT_TAGS`), keys lower-cased, in that list's order, to
    /// be written back after every tag is cleared.
    pub kept_tags: Vec<(String, String)>,
    /// Which location tags the container carries.
    pub location: LocationTags,
}

/// The two ways a video or audio file records where it was made. Outputs
/// keep them differently: ffmpeg's mov muxer writes `location` (as `©xyz`
/// in mov, `loci` in mp4 and m4a) but drops Apple's mdta keys unless
/// `-movflags use_metadata_tags` asks it not to, which conv never passes;
/// Matroska, WebM, ID3 and Vorbis comments keep both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocationTags {
    /// `location`, `location-eng`, Matroska's `LOCATION`.
    pub classic: bool,
    /// `com.apple.quicktime.location.ISO6709`, as iPhones write it, and its
    /// siblings (`...location.accuracy.horizontal`).
    pub apple: bool,
}

/// What a jpg/png/bmp target cannot keep from its source, for the notes
/// that say so (`registry::notes_for`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageTraits {
    /// Whether the first frame has an alpha channel. `None` where a header
    /// read cannot tell (see `parse_traits`).
    pub alpha: Option<bool>,
    /// Whether there is more than one frame or page.
    pub multi_frame: bool,
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

    /// The dimensions the filter chain will actually see. ffmpeg applies a
    /// display matrix before user filters, so a portrait clip stored as
    /// 1280x720 arrives at `scale` as 720x1280 — and a cap decided against
    /// the stored pair caps the wrong axis.
    pub fn display_dimensions(&self) -> Option<(u32, u32)> {
        let (w, h) = (self.width?, self.height?);
        match self.rotation.map(|r| r.rem_euclid(360)) {
            Some(90) | Some(270) => Some((h, w)),
            _ => Some((w, h)),
        }
    }
}

/// Parses one ffprobe `N/D` rate. `0/0`, `N/A` and an absent value all
/// yield `None` rather than a zero tuple, which would divide by zero at
/// every comparison site.
fn parse_rate(s: Option<&str>) -> Option<(u32, u32)> {
    let (n, d) = s?.split_once('/')?;
    let (n, d) = (n.parse::<u32>().ok()?, d.parse::<u32>().ok()?);
    (n != 0 && d != 0).then_some((n, d))
}

/// Parses ffprobe's decimal seconds (`60.123456`) into whole milliseconds,
/// by digit string rather than through a float. Anything that is not plain
/// digits with an optional fraction (`N/A`, exponent forms) is `None`.
fn parse_duration_ms(s: &str) -> Option<u64> {
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let millis: String = frac.chars().chain("000".chars()).take(3).collect();
    whole
        .parse::<u64>()
        .ok()?
        .checked_mul(1000)?
        .checked_add(millis.parse().ok()?)
}

/// Parses `ffprobe -show_streams -show_format` JSON: the streams, and the
/// container's duration and size. Any malformed input yields an empty probe,
/// which callers treat as "unknown" and therefore transcode.
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
                        p.width = s.get("width").and_then(|w| w.as_u64()).map(|w| w as u32);
                        p.height = s.get("height").and_then(|h| h.as_u64()).map(|h| h as u32);
                        p.rotation = s
                            .get("side_data_list")
                            .and_then(|l| l.as_array())
                            .and_then(|l| l.iter().find_map(|d| d.get("rotation")))
                            .and_then(|r| r.as_i64())
                            .map(|r| r as i32);
                        // r_frame_rate is the LCM of frame durations, not
                        // the source rate: on VFR phone footage it reads
                        // 600/1 against a true ~27.6 fps. Taking the lower
                        // of the two keeps a cap from ever raising a rate.
                        let r = parse_rate(s.get("r_frame_rate").and_then(|f| f.as_str()));
                        let a = parse_rate(s.get("avg_frame_rate").and_then(|f| f.as_str()));
                        p.frame_rate = match (r, a) {
                            (Some(r), Some(a)) => {
                                let lower = |x: (u32, u32), y: (u32, u32)| {
                                    if u64::from(x.0) * u64::from(y.1)
                                        <= u64::from(y.0) * u64::from(x.1)
                                    {
                                        x
                                    } else {
                                        y
                                    }
                                };
                                Some(lower(r, a))
                            }
                            (Some(r), None) => Some(r),
                            (None, a) => a,
                        };
                    }
                }
            }
            "audio" => {
                p.audio_codecs.push(name);
                p.audio_bitrates.push(
                    s.get("bit_rate")
                        .and_then(|b| b.as_str())
                        .and_then(|b| b.parse::<u32>().ok())
                        // ffprobe's own "could not tell" is a zero.
                        .filter(|&b| b > 0),
                );
            }
            "subtitle" => p.subtitle_codecs.push(name),
            "data" => p.data_streams += 1,
            "attachment" => {
                p.attachment_streams += 1;
                p.attachment_bytes += s
                    .get("extradata_size")
                    .and_then(|e| e.as_u64())
                    .unwrap_or(0);
            }
            _ => {}
        }
    }
    if let Some(format) = v.get("format") {
        p.duration_ms = format
            .get("duration")
            .and_then(|d| d.as_str())
            .and_then(parse_duration_ms);
        p.size_bytes = format
            .get("size")
            .and_then(|s| s.as_str())
            .and_then(|s| s.parse().ok());
        if let Some(tags) = format.get("tags").and_then(|t| t.as_object()) {
            read_tags(&mut p, tags);
        }
    }
    p
}

/// The container's tags, as `--strip-metadata` and the location note need
/// them. Keys are matched without case, as ffmpeg itself matches them:
/// Matroska spells them `TITLE` and `LOCATION`.
fn read_tags(p: &mut MediaProbe, tags: &serde_json::Map<String, serde_json::Value>) {
    for key in tags.keys() {
        let key = key.to_ascii_lowercase();
        p.location.classic |= key == "location" || key.starts_with("location-");
        p.location.apple |= key.starts_with("com.apple.quicktime.location");
    }
    for &kept in crate::metadata::KEPT_TAGS {
        let found = tags
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(kept))
            .and_then(|(_, v)| v.as_str());
        if let Some(value) = found {
            p.kept_tags.push((kept.to_string(), value.to_string()));
        }
    }
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
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_streams",
            "-show_format",
        ])
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

/// How a recipe reads an image, so its pages can be sized the same way:
/// whether it takes every input, every page of its input or only the first,
/// and the density it renders a vector source at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageRead {
    pub density: Option<&'static str>,
    pub every_input: bool,
    pub every_page: bool,
}

/// Reads the size of every page a `--resize --upscale` will scale, with
/// ImageMagick's `-ping`, which reads headers and stops before the pixels:
/// one cheap spawn per input, run only for `--upscale`, whose warning and
/// question need the sizes. `-ping` and `info:` read the same way in
/// ImageMagick 6's `convert` as in 7's `magick`, so this needs no
/// `identify` binary of its own.
///
/// Returns the page that enlarges most -- the one the warning is about and
/// the question is decided on -- labelled with its input, and its page when
/// an input has several, whenever there is more than one. A page that
/// cannot be read fails the whole read, so a run is never decided on part
/// of its pages.
pub fn image(
    magick: &Path,
    inputs: &[std::path::PathBuf],
    read: ImageRead,
    geometry: &str,
) -> Result<MediaProbe> {
    let inputs = if read.every_input {
        inputs
    } else {
        &inputs[..inputs.len().min(1)]
    };
    let mut pages: Vec<(String, (u32, u32))> = Vec::new();
    for input in inputs {
        let sizes = image_pages(magick, input, read)?;
        let name = input.file_name().map_or_else(
            || input.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let several = sizes.len() > 1;
        for (i, size) in sizes.into_iter().enumerate() {
            let label = if several {
                format!("{name} page {}", i + 1)
            } else {
                name.clone()
            };
            pages.push((label, size));
        }
    }
    let sizes: Vec<(u32, u32)> = pages.iter().map(|&(_, s)| s).collect();
    let i = crate::video::most_enlarged(geometry, &sizes).ok_or_else(|| {
        ConvError::new(ErrorCode::ConversionFailed, "no image to read a size from")
    })?;
    let several = pages.len() > 1;
    let (label, (width, height)) = pages.swap_remove(i);
    Ok(MediaProbe {
        width: Some(width),
        height: Some(height),
        label: several.then_some(label),
        ..MediaProbe::default()
    })
}

/// One input's pages, each at the size the recipe will render it: every
/// page, or only the first frame, as the recipe takes them.
fn image_pages(magick: &Path, input: &Path, read: ImageRead) -> Result<Vec<(u32, u32)>> {
    if !input.is_file() {
        return Err(ConvError::new(
            ErrorCode::InputNotFound,
            format!(
                "not an existing regular file, refusing to probe: {}",
                input.display()
            ),
        ));
    }
    let mut target = input.as_os_str().to_owned();
    if !read.every_page {
        target.push("[0]");
    }
    let out = backend_command(magick)
        .arg("-ping")
        .arg(target)
        .args(["-format", "%w %h %[orientation] %x\n", "info:"])
        .output()
        .map_err(|e| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("failed to run ImageMagick: {e}"),
            )
        })?;
    let density = read.density.and_then(|d| d.parse::<f64>().ok());
    let text = String::from_utf8_lossy(&out.stdout);
    let pages: Option<Vec<(u32, u32)>> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| parse_page(l, density))
        .collect();
    pages.filter(|p| !p.is_empty()).ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("ImageMagick could not read the size of {}", input.display()),
        )
    })
}

/// Parses one `-ping` line, `W H ORIENTATION XRES`, into the size the
/// recipe produces. The recipes auto-orient before they resize, so EXIF
/// orientations 5-8, which turn the picture on its side, swap it. A vector
/// source is pinged at the density ImageMagick assumed, which `XRES`
/// reports, and rendered at the recipe's own `density`; a raster's
/// resolution is only a tag, so with no `density` it never scales.
/// ImageMagick 6 may follow `XRES` with its unit, which is ignored.
fn parse_page(line: &str, density: Option<f64>) -> Option<(u32, u32)> {
    let mut it = line.split_whitespace();
    let positive = |t: Option<&str>| t?.parse::<u32>().ok().filter(|&n| n > 0);
    let (w, h) = (positive(it.next())?, positive(it.next())?);
    let on_its_side = matches!(
        it.next(),
        Some("LeftTop" | "RightTop" | "RightBottom" | "LeftBottom")
    );
    let (w, h) = match density {
        None => (w, h),
        Some(rendered) => {
            let assumed = it.next()?.parse::<f64>().ok().filter(|&x| x > 0.0)?;
            let scale = |v: u32| (f64::from(v) * rendered / assumed).round() as u32;
            (scale(w), scale(h))
        }
    };
    Some(if on_its_side { (h, w) } else { (w, h) })
}

/// Reads an image's `ImageTraits` with `-ping`, which stops before the
/// pixels, over its first two frames only: enough to tell one frame from
/// several, so a long animation costs what a still does. Run only for the
/// pairs whose notes depend on it (`registry::notes_need_image`). Any
/// failure is an error, which callers take as "not read", keeping those
/// notes whole.
pub fn image_traits(magick: &Path, input: &Path) -> Result<ImageTraits> {
    if !input.is_file() {
        return Err(ConvError::new(
            ErrorCode::InputNotFound,
            format!(
                "not an existing regular file, refusing to probe: {}",
                input.display()
            ),
        ));
    }
    let mut target = input.as_os_str().to_owned();
    target.push("[0-1]");
    let out = backend_command(magick)
        .arg("-ping")
        .arg(target)
        .args(["-format", "%m %A %n\n", "info:"])
        .output()
        .map_err(|e| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("failed to run ImageMagick: {e}"),
            )
        })?;
    out.status
        .success()
        .then(|| parse_traits(&String::from_utf8_lossy(&out.stdout)))
        .flatten()
        .ok_or_else(|| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("ImageMagick could not read {}", input.display()),
            )
        })
}

/// Parses the first `-ping` line, `CODER ALPHA FRAMES`. ImageMagick 7 names
/// the alpha trait (`Undefined` for none); 6 says `True` or `False`. "No
/// alpha" is believed only from coders that set alpha before a ping stops:
/// TIFF's does not, and says `Undefined` for a transparent file too.
fn parse_traits(text: &str) -> Option<ImageTraits> {
    let mut it = text.lines().next()?.split_whitespace();
    let coder = it.next()?;
    let alpha = match it.next()? {
        "Undefined" | "False" => false,
        "Blend" | "Copy" | "Update" | "True" => true,
        _ => return None,
    };
    let frames = it.next()?.parse::<u32>().ok().filter(|&n| n > 0)?;
    let trusted = matches!(
        coder,
        "JPEG" | "PNG" | "WEBP" | "AVIF" | "HEIC" | "HEIF" | "BMP" | "BMP2" | "BMP3"
    );
    Some(ImageTraits {
        alpha: (alpha || trusted).then_some(alpha),
        multi_frame: frames > 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kept tags in `KEPT_TAGS` order, lower-cased, whatever case the
    /// container spells them in; and both kinds of location tag.
    #[test]
    fn format_tags_yield_the_kept_tags_and_the_location_kinds() {
        let p = parse(
            r#"{"streams":[],"format":{"tags":{
                "artist":"Band","TITLE":"Song","date":"2024","encoder":"Lavf",
                "com.apple.quicktime.location.ISO6709":"+51.5007-000.1246+010.000/",
                "location-eng":"+51.5007-000.1246/"}}}"#,
        );
        assert_eq!(
            p.kept_tags,
            [
                ("title".to_string(), "Song".to_string()),
                ("artist".to_string(), "Band".to_string())
            ]
        );
        assert_eq!(
            p.location,
            LocationTags {
                classic: true,
                apple: true
            }
        );
        let plain = parse(r#"{"streams":[],"format":{"tags":{"title":"x"}}}"#);
        assert_eq!(plain.location, LocationTags::default());
    }

    #[test]
    fn traits_read_alpha_and_whether_a_second_frame_follows() {
        let t = |text| parse_traits(text).unwrap();
        assert_eq!(
            t("HEIC Undefined 1\n"),
            ImageTraits {
                alpha: Some(false),
                multi_frame: false
            }
        );
        assert_eq!(t("PNG Blend 1\n").alpha, Some(true));
        assert!(t("WEBP Blend 2\nWEBP Blend 2\n").multi_frame);
        // ImageMagick 6, and a Windows line ending.
        assert_eq!(t("PNG False 1\r\n").alpha, Some(false));
        assert_eq!(t("BMP3 True 1\r\n").alpha, Some(true));
    }

    /// A TIFF ping leaves alpha unset whatever the file holds, so its "no
    /// alpha" is unknown; its frame count still reads.
    #[test]
    fn a_tiff_ping_cannot_rule_alpha_out() {
        let tiff = parse_traits("TIFF Undefined 2\nTIFF Undefined 2\n").unwrap();
        assert_eq!(tiff.alpha, None);
        assert!(tiff.multi_frame);
        assert_eq!(parse_traits("TIFF Blend 1\n").unwrap().alpha, Some(true));
    }

    #[test]
    fn a_traits_answer_that_is_not_one_is_none() {
        assert_eq!(parse_traits(""), None);
        assert_eq!(parse_traits("640 360 TopLeft"), None);
        assert_eq!(parse_traits("PNG Blend"), None);
        assert_eq!(parse_traits("PNG Blend 0"), None);
        assert_eq!(parse_traits("magick: no decode delegate"), None);
    }

    #[test]
    fn a_page_reads_its_displayed_size() {
        assert_eq!(parse_page("320 240 TopLeft 72", None), Some((320, 240)));
        assert_eq!(
            parse_page("4032 3024 Undefined 72", None),
            Some((4032, 3024))
        );
    }

    /// The recipes auto-orient before resizing, so EXIF orientations 5-8,
    /// which turn the picture on its side, swap what the user sees.
    #[test]
    fn a_page_on_its_side_swaps_width_and_height() {
        for o in ["LeftTop", "RightTop", "RightBottom", "LeftBottom"] {
            assert_eq!(
                parse_page(&format!("320 240 {o} 72"), None),
                Some((240, 320)),
                "{o}"
            );
        }
        for o in ["TopLeft", "TopRight", "BottomRight", "BottomLeft"] {
            assert_eq!(
                parse_page(&format!("320 240 {o} 72"), None),
                Some((320, 240)),
                "{o}"
            );
        }
    }

    /// `-ping` sizes a vector source at the density it assumed, which it
    /// reports; the recipe renders at its own. A raster's resolution is
    /// only a tag, and never scales it.
    #[test]
    fn a_vector_page_is_sized_at_the_recipes_density() {
        assert_eq!(
            parse_page("100 100 Undefined 96", Some(384.0)),
            Some((400, 400))
        );
        // ImageMagick 6 may follow the resolution with its unit.
        assert_eq!(
            parse_page("100 100 Undefined 72 PixelsPerInch", Some(384.0)),
            Some((533, 533))
        );
        assert_eq!(parse_page("100 100 Undefined 0", Some(384.0)), None);
    }

    #[test]
    fn a_page_answer_that_is_not_a_size_is_none() {
        assert!(parse_page("", None).is_none());
        assert!(parse_page("magick: no decode delegate", None).is_none());
        assert!(parse_page("0 240 TopLeft 72", None).is_none());
    }

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

    #[test]
    fn a_video_stream_yields_dimensions_and_a_rational_frame_rate() {
        let p = parse(
            r#"{"streams":[{"codec_type":"video","codec_name":"h264",
                "width":1920,"height":1080,
                "r_frame_rate":"30000/1001","avg_frame_rate":"30000/1001"}]}"#,
        );
        assert_eq!(p.width, Some(1920));
        assert_eq!(p.height, Some(1080));
        assert_eq!(p.frame_rate, Some((30000, 1001)));
        assert_eq!(p.rotation, None);
    }

    #[test]
    fn the_lower_of_the_two_reported_rates_wins() {
        // r_frame_rate is the LCM of frame durations on a VFR source: 600/1
        // against a true ~27.6 fps. Capping against it would let --fps 60
        // duplicate frames, which is the one thing a cap must never do.
        let p = parse(
            r#"{"streams":[{"codec_type":"video","codec_name":"h264",
                "width":1280,"height":720,
                "r_frame_rate":"600/1","avg_frame_rate":"2500/90"}]}"#,
        );
        assert_eq!(p.frame_rate, Some((2500, 90)));
    }

    #[test]
    fn an_unusable_frame_rate_is_none_not_a_zero_tuple() {
        // A (0, 0) tuple divides by zero at every comparison site.
        for rate in [r#""0/0""#, r#""N/A""#, r#""""#] {
            let json = format!(
                r#"{{"streams":[{{"codec_type":"video","codec_name":"h264",
                    "width":640,"height":480,
                    "r_frame_rate":{rate},"avg_frame_rate":{rate}}}]}}"#
            );
            assert_eq!(parse(&json).frame_rate, None, "rate {rate}");
        }
    }

    #[test]
    fn a_rotated_source_reports_stored_dimensions_and_displayed_ones_separately() {
        let p = parse(
            r#"{"streams":[{"codec_type":"video","codec_name":"h264",
                "width":1280,"height":720,
                "r_frame_rate":"30/1","avg_frame_rate":"30/1",
                "side_data_list":[{"side_data_type":"Display Matrix","rotation":-90}]}]}"#,
        );
        assert_eq!((p.width, p.height), (Some(1280), Some(720)));
        assert_eq!(p.rotation, Some(-90));
        // ffmpeg autorotates before the user filter chain, so iw/ih are these.
        assert_eq!(p.display_dimensions(), Some((720, 1280)));
    }

    #[test]
    fn an_unrotated_source_displays_as_it_is_stored() {
        let p = parse(
            r#"{"streams":[{"codec_type":"video","codec_name":"h264",
                "width":1280,"height":720,"r_frame_rate":"30/1"}]}"#,
        );
        assert_eq!(p.display_dimensions(), Some((1280, 720)));
    }

    #[test]
    fn cover_art_contributes_no_dimensions() {
        // Cover art is not the file's video track; it must not set width.
        let p = parse(
            r#"{"streams":[
                {"codec_type":"video","codec_name":"mjpeg","width":600,"height":600,
                 "disposition":{"attached_pic":1}},
                {"codec_type":"audio","codec_name":"aac"}]}"#,
        );
        assert_eq!(p.width, None);
        assert_eq!(p.frame_rate, None);
    }

    const WITH_FORMAT: &str = r#"{
        "streams":[
            {"codec_type":"video","codec_name":"h264","width":1920,"height":1080,
             "r_frame_rate":"30/1","avg_frame_rate":"30/1"},
            {"codec_type":"audio","codec_name":"aac","bit_rate":"160000"},
            {"codec_type":"audio","codec_name":"ac3"},
            {"codec_type":"attachment","codec_name":"ttf","extradata_size":51234},
            {"codec_type":"attachment","codec_name":"otf","extradata_size":1000}
        ],
        "format":{"duration":"60.123456","size":"12345678"}
    }"#;

    #[test]
    fn reads_duration_and_size_from_the_format_block() {
        let p = parse(WITH_FORMAT);
        assert_eq!(p.duration_ms, Some(60_123));
        assert_eq!(p.size_bytes, Some(12_345_678));
    }

    /// Parallel to `audio_codecs`, so index N is always stream `0:a:N`,
    /// including a stream that reported no bitrate.
    #[test]
    fn audio_bitrates_stay_parallel_to_audio_codecs() {
        let p = parse(WITH_FORMAT);
        assert_eq!(p.audio_codecs, vec!["aac".to_string(), "ac3".to_string()]);
        assert_eq!(p.audio_bitrates, vec![Some(160_000), None]);
    }

    #[test]
    fn attachment_bytes_sum_every_attachment() {
        assert_eq!(parse(WITH_FORMAT).attachment_bytes, 52_234);
    }

    #[test]
    fn a_missing_or_unusable_format_block_leaves_duration_unknown() {
        assert_eq!(parse(SAMPLE).duration_ms, None);
        let na = r#"{"streams":[{"codec_type":"video","codec_name":"h264"}],
                     "format":{"duration":"N/A"}}"#;
        assert_eq!(parse(na).duration_ms, None);
    }

    #[test]
    fn durations_parse_without_floating_point() {
        assert_eq!(parse_duration_ms("12"), Some(12_000));
        assert_eq!(parse_duration_ms("12.5"), Some(12_500));
        assert_eq!(parse_duration_ms("0.0009"), Some(0));
        assert_eq!(parse_duration_ms("2700.000000"), Some(2_700_000));
        assert_eq!(parse_duration_ms("N/A"), None);
        assert_eq!(parse_duration_ms("1.2e3"), None);
    }

    /// ffprobe writes `"bit_rate":"0"` for a stream whose rate it could not
    /// work out. A zero is "unknown", not a track that costs nothing: the
    /// budget would otherwise skip that track's audio allowance.
    #[test]
    fn a_zero_audio_bitrate_is_unknown() {
        let p = parse(
            r#"{"streams":[
                {"codec_type":"audio","codec_name":"aac","bit_rate":"0"},
                {"codec_type":"audio","codec_name":"aac","bit_rate":"128000"}]}"#,
        );
        assert_eq!(p.audio_bitrates, vec![None, Some(128_000)]);
    }
}
