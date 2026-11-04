//! `conv info`: what a file is, read without changing it.
//!
//! Video and audio are read with ffprobe, pictures with ImageMagick's
//! `-ping` (headers only), PDFs and Office files by conv itself. A missing
//! backend is not a failure of the whole answer: what conv can tell without
//! it (the format and the size) is still returned, with the backend's
//! `backend_missing` error alongside, so the caller can say what is missing
//! and how to get it. Nothing is ever installed from here.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use crate::pdf::inspect::PdfInfo;
use crate::procutil::backend_command;
use crate::rotate::Orientation;
use crate::{registry, Backend, ConvError, ErrorCode, Format, Kind, Resolver};

/// Everything `describe` found out about one file.
#[derive(Debug, Clone, Serialize)]
pub struct FileInfo {
    pub path: String,
    /// The format conv takes the file to be, from its extension, as every
    /// other conv command does. `None` for an extension conv does not know.
    pub format: Option<Format>,
    pub kind: Option<Kind>,
    pub bytes: u64,
    pub media: Option<MediaInfo>,
    pub image: Option<ImageInfo>,
    pub pdf: Option<PdfInfo>,
    pub office: Option<OfficeInfo>,
    /// What conv can convert it into.
    pub targets: Vec<Format>,
    /// Things worth knowing, such as contents that do not match the name.
    pub notes: Vec<String>,
    /// Backends that would have shown more, each with how to install it.
    pub missing: Vec<ConvError>,
    /// The backend ran and could not read the file.
    pub error: Option<ConvError>,
}

/// A video or audio file, from ffprobe.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MediaInfo {
    /// ffprobe's name for the container, `QuickTime / MOV`.
    pub container: Option<String>,
    pub duration_ms: Option<u64>,
    /// The whole file's bitrate, in bits per second.
    pub bit_rate: Option<u64>,
    pub streams: Vec<StreamInfo>,
    pub chapters: usize,
    /// Whether the container records where it was made (a `location` tag,
    /// or Apple's `com.apple.quicktime.location.*`).
    pub location: bool,
    /// The camera or phone, from Apple's or a generic make and model tag.
    pub camera: Option<String>,
    /// When it was made, as the file says.
    pub created: Option<String>,
    /// Title, artist, album, album_artist, genre, date and track, as the
    /// file holds them.
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    Video,
    Audio,
    Subtitle,
    Data,
    Attachment,
    Other,
}

/// One stream, with only the fields its kind has.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamInfo {
    pub index: u32,
    #[serde(rename = "type")]
    pub kind: StreamKind,
    pub codec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// How far the picture is turned clockwise for display, from the
    /// display matrix: 90, 180 or 270. `None` when it is shown as stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotation: Option<u16>,
    /// The average frame rate as ffprobe gives it, `30000/1001`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    /// The frames are not evenly spaced (phone footage often is not).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub variable_frame_rate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pixel_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u32>,
    /// `HDR10`, `HLG` or `Dolby Vision`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_layout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_rate: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// An audio file's embedded cover picture, which ffprobe lists as a
    /// video stream.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cover_art: bool,
}

/// A picture, from `magick -ping`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImageInfo {
    /// ImageMagick's name for what the bytes are: `JPEG`, `HEIC`.
    pub coder: String,
    /// The first frame's stored size.
    pub width: u32,
    pub height: u32,
    pub bit_depth: Option<u32>,
    pub colorspace: Option<String>,
    /// The embedded colour profile's name, `Display P3`.
    pub profile: Option<String>,
    /// `None` where a header read cannot tell (TIFF, ICO).
    pub alpha: Option<bool>,
    pub frames: u32,
    /// More than one frame in a GIF or WebP: an animation, not pages.
    pub animated: bool,
    /// Every frame's size, for an icon, which holds one per size.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub frame_sizes: Vec<(u32, u32)>,
    pub orientation: Option<OrientationInfo>,
    pub camera: Option<String>,
    /// EXIF's DateTimeOriginal, `2025-12-27 13:59:08`, with its offset
    /// from UTC when the file records one.
    pub taken: Option<String>,
    /// Whether the EXIF records a GPS position.
    pub location: bool,
}

/// An EXIF orientation tag, and what it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OrientationInfo {
    /// 1 to 8.
    pub tag: u16,
    /// Turned this far clockwise for display.
    pub clockwise: u16,
    pub mirrored: bool,
}

impl FileInfo {
    /// Whether the file records where it was made, as a picture's EXIF or
    /// a video's container tags say.
    pub fn location(&self) -> bool {
        self.image.as_ref().is_some_and(|i| i.location)
            || self.media.as_ref().is_some_and(|m| m.location)
    }
}

impl OrientationInfo {
    /// What EXIF orientation tag `tag` (1 to 8) does.
    pub fn from_tag(tag: u16) -> Option<OrientationInfo> {
        let o = Orientation::from_exif(tag)?;
        Some(OrientationInfo {
            tag,
            clockwise: o.clockwise(),
            mirrored: o.mirrored(),
        })
    }

    /// Whether the picture is shown on its side, so its shown width and
    /// height are its stored height and width.
    pub fn on_its_side(&self) -> bool {
        self.clockwise % 180 == 90
    }
}

/// What conv reads of an Office file itself (`office::traits`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OfficeInfo {
    pub encrypted: bool,
    pub macros: bool,
}

/// Describes `path`. Never fails as a whole: what could not be read is in
/// `missing` (a backend that is not installed) or `error` (one that could
/// not read the file).
pub fn describe(path: &Path, resolver: &Resolver) -> FileInfo {
    let format = Format::from_path(path);
    let mut info = FileInfo {
        path: path.to_string_lossy().into_owned(),
        format,
        kind: format.map(|f| f.kind()),
        bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        media: None,
        image: None,
        pdf: None,
        office: None,
        targets: format.map(targets_of).unwrap_or_default(),
        notes: Vec::new(),
        missing: Vec::new(),
        error: None,
    };
    if !path.is_file() {
        info.error = Some(ConvError::new(
            ErrorCode::InputNotFound,
            format!("not an existing regular file: {}", path.display()),
        ));
        return info;
    }
    let Some(format) = format else {
        return info;
    };
    match format.kind() {
        Kind::Video | Kind::Audio => match resolver.resolve(Backend::Ffprobe) {
            Ok(ffprobe) => match crate::probe::json(&ffprobe.path, path) {
                Ok(json) => match parse_media(&json) {
                    Some(media) => info.media = Some(media),
                    None => info.error = Some(unreadable("ffprobe", path)),
                },
                Err(e) => info.error = Some(e),
            },
            Err(e) => info.missing.push(e),
        },
        Kind::Image => match resolver.resolve(Backend::Magick) {
            Ok(magick) => match read_image(&magick.path, path, format) {
                Some(image) => {
                    if let Some(note) = mismatch_note(format, &image.coder) {
                        info.notes.push(note);
                    }
                    if matches!(format, Format::Heic | Format::Heif) && no_exif(&image) {
                        info.notes.push(HEIC_EXIF_NOTE.to_string());
                    }
                    info.image = Some(image);
                }
                None => info.error = Some(unreadable("ImageMagick", path)),
            },
            Err(e) => info.missing.push(e),
        },
        Kind::Document => match format {
            Format::Pdf => match crate::pdf::inspect::read(path) {
                Some(pdf) => info.pdf = Some(pdf),
                None => {
                    info.error = Some(ConvError::new(
                        ErrorCode::ConversionFailed,
                        format!("{} does not read as a PDF", file_name(path)),
                    ))
                }
            },
            Format::Doc
            | Format::Xls
            | Format::Ppt
            | Format::Docx
            | Format::Xlsx
            | Format::Pptx => {
                info.office = crate::office::traits(path).map(|t| OfficeInfo {
                    encrypted: t.encrypted,
                    macros: t.macros,
                });
            }
            _ => {}
        },
    }
    info
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn unreadable(backend: &str, path: &Path) -> ConvError {
    ConvError::new(
        ErrorCode::ConversionFailed,
        format!(
            "{backend} could not read {}; it may be damaged, or not what its name says",
            file_name(path)
        ),
    )
}

/// Every format `from` converts into, in the registry's order.
fn targets_of(from: Format) -> Vec<Format> {
    registry::all_pairs()
        .into_iter()
        .filter(|&(f, _)| f == from)
        .map(|(_, to)| to)
        .collect()
}

// ---------------------------------------------------------------------------
// Video and audio

fn string(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "unknown" && *s != "N/A")
        .map(str::to_owned)
}

fn number(v: &serde_json::Value, key: &str) -> Option<u64> {
    let n = v.get(key)?;
    n.as_u64()
        .or_else(|| n.as_str().and_then(|s| s.parse().ok()))
        .filter(|&n| n > 0)
}

/// A tag, matched without case as ffmpeg matches them.
fn tag(tags: Option<&serde_json::Map<String, serde_json::Value>>, key: &str) -> Option<String> {
    tags?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, v)| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Parses `ffprobe -show_streams -show_format -show_chapters` JSON. `None`
/// when it lists no streams, which is how ffprobe reports a file it could
/// not read.
pub fn parse_media(json: &str) -> Option<MediaInfo> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let streams = v.get("streams")?.as_array()?;
    if streams.is_empty() {
        return None;
    }
    let format = v.get("format");
    let ftags = format
        .and_then(|f| f.get("tags"))
        .and_then(|t| t.as_object());
    // An Ogg file keeps its tags on its audio stream (see `probe::parse`).
    let ogg = format
        .and_then(|f| f.get("format_name"))
        .and_then(|n| n.as_str())
        == Some("ogg");
    let stream_tags = streams
        .iter()
        .find(|s| s.get("codec_type").and_then(|t| t.as_str()) == Some("audio"))
        .and_then(|s| s.get("tags"))
        .and_then(|t| t.as_object());
    let file_tags = if ogg && stream_tags.is_some() {
        stream_tags
    } else {
        ftags
    };
    let mut tags = BTreeMap::new();
    for key in [
        "title",
        "artist",
        "album",
        "album_artist",
        "genre",
        "date",
        "track",
    ] {
        if let Some(value) = tag(file_tags, key) {
            tags.insert(key.to_string(), value);
        }
    }
    let make = tag(ftags, "com.apple.quicktime.make").or_else(|| tag(ftags, "make"));
    let model = tag(ftags, "com.apple.quicktime.model").or_else(|| tag(ftags, "model"));
    let camera = match (make, model) {
        (Some(make), Some(model)) if model.starts_with(&make) => Some(model),
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (a, b) => a.or(b),
    };
    Some(MediaInfo {
        container: format.and_then(|f| string(f, "format_long_name")),
        duration_ms: format
            .and_then(|f| f.get("duration"))
            .and_then(|d| d.as_str())
            .and_then(crate::probe::parse_duration_ms),
        bit_rate: format.and_then(|f| number(f, "bit_rate")),
        streams: streams
            .iter()
            .enumerate()
            .map(|(i, s)| parse_stream(i, s))
            .collect(),
        chapters: v
            .get("chapters")
            .and_then(|c| c.as_array())
            .map_or(0, Vec::len),
        location: {
            let p = crate::probe::parse(json);
            p.location.classic || p.location.apple
        },
        camera,
        created: tag(ftags, "com.apple.quicktime.creationdate")
            .or_else(|| tag(ftags, "creation_time")),
        tags,
    })
}

fn parse_stream(i: usize, s: &serde_json::Value) -> StreamInfo {
    let kind = match s.get("codec_type").and_then(|t| t.as_str()) {
        Some("video") => StreamKind::Video,
        Some("audio") => StreamKind::Audio,
        Some("subtitle") => StreamKind::Subtitle,
        Some("data") => StreamKind::Data,
        Some("attachment") => StreamKind::Attachment,
        _ => StreamKind::Other,
    };
    let cover_art = s
        .pointer("/disposition/attached_pic")
        .and_then(|a| a.as_i64())
        == Some(1);
    let tags = s.get("tags").and_then(|t| t.as_object());
    let video = kind == StreamKind::Video;
    let audio = kind == StreamKind::Audio;
    let side_data = s.get("side_data_list").and_then(|l| l.as_array());
    // ffprobe gives the display matrix's turn counter-clockwise: an iPhone
    // portrait clip, shown turned a quarter right, says -90.
    let rotation = side_data
        .and_then(|l| l.iter().find_map(|d| d.get("rotation")))
        .and_then(|r| r.as_f64())
        .map(|r| (-(r.round() as i64)).rem_euclid(360) as u16)
        .filter(|&r| r != 0);
    let avg = crate::probe::parse_rate(s.get("avg_frame_rate").and_then(|f| f.as_str()));
    let r = crate::probe::parse_rate(s.get("r_frame_rate").and_then(|f| f.as_str()));
    let rate = if video && !cover_art { avg.or(r) } else { None };
    let as_f64 = |(n, d): (u32, u32)| f64::from(n) / f64::from(d);
    let variable = match (avg, r) {
        (Some(a), Some(r)) if rate.is_some() => {
            let (a, r) = (as_f64(a), as_f64(r));
            (a - r).abs() / r > 0.02
        }
        _ => false,
    };
    let pixel_format = if video { string(s, "pix_fmt") } else { None };
    let bit_depth = if video {
        pixel_format
            .as_deref()
            .and_then(bit_depth_of)
            .or_else(|| number(s, "bits_per_raw_sample").map(|b| b as u32))
    } else {
        None
    };
    let dolby_vision = side_data.is_some_and(|l| {
        l.iter().any(|d| {
            d.get("side_data_type")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.starts_with("DOVI"))
        })
    });
    let hdr = if !video {
        None
    } else if dolby_vision {
        Some("Dolby Vision".to_string())
    } else {
        match s.get("color_transfer").and_then(|t| t.as_str()) {
            Some("smpte2084") => Some("HDR10".to_string()),
            Some("arib-std-b67") => Some("HLG".to_string()),
            _ => None,
        }
    };
    StreamInfo {
        index: s
            .get("index")
            .and_then(|n| n.as_u64())
            .map_or(i as u32, |n| n as u32),
        kind,
        codec: string(s, "codec_name").unwrap_or_else(|| "unknown".to_string()),
        profile: string(s, "profile"),
        width: if video {
            number(s, "width").map(|w| w as u32)
        } else {
            None
        },
        height: if video {
            number(s, "height").map(|h| h as u32)
        } else {
            None
        },
        rotation: if video { rotation } else { None },
        frame_rate: rate.map(|(n, d)| format!("{n}/{d}")),
        fps: rate.map(|r| (as_f64(r) * 1000.0).round() / 1000.0),
        variable_frame_rate: variable,
        pixel_format,
        bit_depth,
        hdr,
        sample_rate: if audio {
            number(s, "sample_rate").map(|r| r as u32)
        } else {
            None
        },
        channels: if audio {
            number(s, "channels").map(|c| c as u32)
        } else {
            None
        },
        channel_layout: if audio {
            string(s, "channel_layout")
        } else {
            None
        },
        bit_rate: number(s, "bit_rate"),
        language: tag(tags, "language").filter(|l| l != "und"),
        title: tag(tags, "title"),
        cover_art,
    }
}

/// The bits per sample a pixel format names: `yuv420p10le` is 10.
fn bit_depth_of(pix_fmt: &str) -> Option<u32> {
    let digits: String = pix_fmt
        .trim_end_matches("le")
        .trim_end_matches("be")
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    match digits.parse::<u32>().ok() {
        Some(d @ (9 | 10 | 12 | 14 | 16)) => Some(d),
        // `yuv420p` and friends name no depth: 8 bits.
        _ if pix_fmt.starts_with("yuv") || pix_fmt.starts_with("nv12") => Some(8),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Pictures

/// Said of a HEIC ImageMagick read no EXIF from. ImageMagick 6, which
/// most Linux distributions ship, reads none from a HEIC, so a photo that
/// records where it was taken would otherwise look as if it didn't.
pub const HEIC_EXIF_NOTE: &str = "ImageMagick read no camera, date or location from this HEIC; \
     ImageMagick 6, as most Linux distributions ship it, can't, so the photo may still \
     record where it was taken.";

/// Whether ImageMagick gave none of a picture's EXIF.
fn no_exif(image: &ImageInfo) -> bool {
    image.camera.is_none() && image.taken.is_none() && !image.location
}

/// What `-ping` prints for each frame, tab-separated. A property the file
/// lacks prints as nothing (with a warning on stderr, which is dropped).
const IMAGE_FORMAT: &str = "%m\t%w\t%h\t%z\t%[colorspace]\t%A\t%[orientation]\t\
    %[EXIF:Make]\t%[EXIF:Model]\t%[EXIF:DateTimeOriginal]\t%[EXIF:OffsetTimeOriginal]\t\
    %[EXIF:GPSLatitude]\t%[icc:description]\n";

fn read_image(magick: &Path, input: &Path, format: Format) -> Option<ImageInfo> {
    let run = |ping: bool, first_only: bool| -> Option<String> {
        let mut target = input.as_os_str().to_owned();
        if first_only {
            target.push("[0]");
        }
        let mut cmd = backend_command(magick);
        if ping {
            cmd.arg("-ping");
        }
        let out = cmd
            .arg(target)
            .args(["-format", IMAGE_FORMAT, "info:"])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let mut image = parse_image(&run(true, false)?)?;
    // A WebP's `-ping` stops before its EXIF and colour profile, so its
    // first frame is read whole for them.
    if format == Format::Webp {
        if let Some(full) = run(false, true).as_deref().and_then(parse_image) {
            image.orientation = full.orientation;
            image.camera = full.camera;
            image.taken = full.taken;
            image.location = full.location;
            image.profile = full.profile;
        }
    }
    if format != Format::Ico {
        image.frame_sizes.clear();
    }
    Some(image)
}

/// Parses `IMAGE_FORMAT`'s lines: the first frame's fields, and how many
/// frames there are.
pub fn parse_image(text: &str) -> Option<ImageInfo> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let first: Vec<&str> = lines.first()?.split('\t').map(str::trim).collect();
    let field = |i: usize| first.get(i).copied().filter(|s| !s.is_empty());
    let coder = field(0)?.to_string();
    let width = field(1)?.parse().ok()?;
    let height = field(2)?.parse().ok()?;
    // ImageMagick 7 names the alpha trait (`Undefined` for none); 6 says
    // `True` or `False`. "No alpha" is believed only from coders that set
    // it before a ping stops, as in `probe::parse_traits`.
    let alpha = match field(5) {
        Some("Undefined" | "False") => Some(false),
        Some("Blend" | "Copy" | "Update" | "True") => Some(true),
        _ => None,
    };
    let trusted = matches!(
        coder.as_str(),
        "JPEG" | "PNG" | "WEBP" | "AVIF" | "HEIC" | "HEIF" | "BMP" | "BMP2" | "BMP3" | "GIF"
    );
    let alpha = alpha.filter(|&a| a || trusted);
    let orientation = field(6)
        .and_then(orientation_tag)
        .and_then(OrientationInfo::from_tag);
    let clean = |s: Option<&str>| {
        s.map(|s| s.trim_matches('\0').trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let make = clean(field(7));
    let model = clean(field(8));
    let camera = match (make, model) {
        (Some(make), Some(model)) if model.starts_with(&make) => Some(model),
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (a, b) => a.or(b),
    };
    let taken = clean(field(9)).map(|t| {
        // EXIF writes dates `2025:12:27 13:59:08`.
        let t = match t.split_once(' ') {
            Some((date, time)) => format!("{} {time}", date.replace(':', "-")),
            None => t,
        };
        match clean(field(10)) {
            Some(offset) => format!("{t} {offset}"),
            None => t,
        }
    });
    let frames = lines.len() as u32;
    let frame_sizes = lines
        .iter()
        .filter_map(|l| {
            let mut it = l.split('\t').skip(1);
            Some((
                it.next()?.trim().parse().ok()?,
                it.next()?.trim().parse().ok()?,
            ))
        })
        .collect();
    Some(ImageInfo {
        animated: frames > 1 && matches!(coder.as_str(), "GIF" | "WEBP" | "PNG" | "APNG"),
        coder,
        width,
        height,
        bit_depth: field(3).and_then(|d| d.parse().ok()),
        colorspace: field(4).map(str::to_owned),
        profile: clean(field(12)),
        alpha,
        frames,
        frame_sizes,
        orientation,
        camera,
        taken,
        location: field(11).is_some(),
    })
}

/// ImageMagick's name for an EXIF orientation, as its tag number.
/// `Undefined` (no tag) is `None`.
fn orientation_tag(name: &str) -> Option<u16> {
    Some(match name {
        "TopLeft" => 1,
        "TopRight" => 2,
        "BottomRight" => 3,
        "BottomLeft" => 4,
        "LeftTop" => 5,
        "RightTop" => 6,
        "RightBottom" => 7,
        "LeftBottom" => 8,
        _ => return None,
    })
}

/// A note when the bytes are a different picture format from the one the
/// name says. conv goes by the name, so this is worth knowing before a
/// conversion behaves oddly.
fn mismatch_note(format: Format, coder: &str) -> Option<String> {
    let actual = match coder {
        "JPEG" => Format::Jpg,
        "PNG" => Format::Png,
        "GIF" => Format::Gif,
        "WEBP" => Format::Webp,
        "AVIF" => Format::Avif,
        "TIFF" => Format::Tiff,
        "BMP" | "BMP2" | "BMP3" => Format::Bmp,
        "ICO" => Format::Ico,
        "HEIC" | "HEIF" => return None,
        _ => return None,
    };
    (actual != format && !(format == Format::Heic || format == Format::Heif)).then(|| {
        format!(
            "The contents are {}, though the name says .{}. conv goes by the name, so rename it \
             to .{} first.",
            coder,
            format.ext(),
            actual.ext()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_video_reads_its_streams_with_rotation_rate_and_hdr() {
        let m = parse_media(
            r#"{"streams":[
              {"index":0,"codec_type":"video","codec_name":"hevc","profile":"Main 10",
               "width":1920,"height":1080,"pix_fmt":"yuv420p10le","color_transfer":"arib-std-b67",
               "avg_frame_rate":"30000/1001","r_frame_rate":"30000/1001",
               "side_data_list":[{"side_data_type":"Display Matrix","rotation":-90}],
               "tags":{"language":"und"}},
              {"index":1,"codec_type":"audio","codec_name":"aac","profile":"LC",
               "sample_rate":"48000","channels":2,"channel_layout":"stereo","bit_rate":"128000",
               "tags":{"language":"eng"}},
              {"index":2,"codec_type":"subtitle","codec_name":"mov_text","tags":{"language":"jpn"}},
              {"index":3,"codec_type":"data","codec_name":"bin_data"}],
             "chapters":[{"id":0},{"id":1}],
             "format":{"format_long_name":"QuickTime / MOV","duration":"12.345","bit_rate":"8000000",
               "tags":{"com.apple.quicktime.make":"Apple","com.apple.quicktime.model":"iPhone 15",
                       "com.apple.quicktime.location.ISO6709":"+51.5+000.1/",
                       "com.apple.quicktime.creationdate":"2025-06-01T10:00:00+0100"}}}"#,
        )
        .unwrap();
        assert_eq!(m.container.as_deref(), Some("QuickTime / MOV"));
        assert_eq!((m.duration_ms, m.bit_rate), (Some(12_345), Some(8_000_000)));
        assert_eq!(m.chapters, 2);
        assert!(m.location);
        assert_eq!(m.camera.as_deref(), Some("Apple iPhone 15"));
        assert_eq!(m.created.as_deref(), Some("2025-06-01T10:00:00+0100"));
        let v = &m.streams[0];
        assert_eq!(v.rotation, Some(90), "-90 is a quarter turn right");
        assert_eq!(v.fps, Some(29.97));
        assert_eq!((v.bit_depth, v.hdr.as_deref()), (Some(10), Some("HLG")));
        assert_eq!(v.language, None, "und is no language");
        let a = &m.streams[1];
        assert_eq!((a.sample_rate, a.channels), (Some(48_000), Some(2)));
        assert_eq!(a.language.as_deref(), Some("eng"));
        assert_eq!(a.fps, None);
        assert_eq!(m.streams[2].kind, StreamKind::Subtitle);
        assert_eq!(m.streams[3].kind, StreamKind::Data);
    }

    #[test]
    fn an_mp3_reads_its_tags_and_cover_art() {
        let m = parse_media(
            r#"{"streams":[
              {"index":0,"codec_type":"audio","codec_name":"mp3","sample_rate":"44100","channels":2},
              {"index":1,"codec_type":"video","codec_name":"mjpeg","width":500,"height":500,
               "avg_frame_rate":"0/0","r_frame_rate":"90000/1","disposition":{"attached_pic":1}}],
             "format":{"format_long_name":"MP2/3 (MPEG audio layer 2/3)","duration":"200.0",
               "tags":{"title":"Song","ARTIST":"Band","album":"Record","encoder":"Lavf"}}}"#,
        )
        .unwrap();
        assert_eq!(m.tags.get("artist").map(String::as_str), Some("Band"));
        assert_eq!(m.tags.len(), 3, "{:?}", m.tags);
        assert!(m.streams[1].cover_art);
        assert_eq!(m.streams[1].fps, None, "cover art has no frame rate");
        assert!(!m.location);
    }

    #[test]
    fn variable_frame_rate_is_noticed() {
        let m = parse_media(
            r#"{"streams":[{"codec_type":"video","codec_name":"h264",
               "avg_frame_rate":"16567/600","r_frame_rate":"600/1"}],"format":{}}"#,
        )
        .unwrap();
        assert!(m.streams[0].variable_frame_rate);
        assert_eq!(m.streams[0].fps, Some(27.612));
    }

    #[test]
    fn a_report_with_no_streams_is_unreadable() {
        assert_eq!(parse_media("{}"), None);
        assert_eq!(parse_media(r#"{"streams":[]}"#), None);
        assert_eq!(parse_media("not json"), None);
    }

    #[test]
    fn pixel_formats_name_their_depth() {
        assert_eq!(bit_depth_of("yuv420p"), Some(8));
        assert_eq!(bit_depth_of("yuv420p10le"), Some(10));
        assert_eq!(bit_depth_of("p010le"), Some(10));
        assert_eq!(bit_depth_of("yuv444p12be"), Some(12));
        assert_eq!(bit_depth_of("rgb24"), None);
    }

    #[test]
    fn a_photo_reads_its_camera_date_location_and_orientation() {
        let i = parse_image(
            "HEIC\t4032\t3024\t8\tsRGB\tUndefined\tRightTop\tApple\tiPhone 14 Pro\t\
             2025:12:27 13:59:08\t+01:00\t35/1,43/1,2388/100\tDisplay P3\n",
        )
        .unwrap();
        assert_eq!((i.width, i.height, i.bit_depth), (4032, 3024, Some(8)));
        assert_eq!(i.alpha, Some(false));
        assert_eq!(i.profile.as_deref(), Some("Display P3"));
        assert_eq!(i.camera.as_deref(), Some("Apple iPhone 14 Pro"));
        assert_eq!(i.taken.as_deref(), Some("2025-12-27 13:59:08 +01:00"));
        assert!(i.location);
        let o = i.orientation.unwrap();
        assert_eq!((o.tag, o.clockwise, o.mirrored), (6, 90, false));
        assert!(o.on_its_side());
        assert!(!i.animated);
    }

    #[test]
    fn a_gif_with_frames_is_animated_and_a_tiff_says_nothing_of_alpha() {
        let gif = parse_image(
            "GIF\t20\t20\t8\tsRGB\tBlend\tUndefined\n\
                               GIF\t20\t20\t8\tsRGB\tUndefined\tUndefined\n",
        )
        .unwrap();
        assert_eq!((gif.frames, gif.animated, gif.alpha), (2, true, Some(true)));
        assert_eq!(gif.orientation, None);
        assert!(!gif.location);
        let tiff = parse_image(
            "TIFF\t10\t10\t16\tsRGB\tUndefined\tTopLeft\n\
                                TIFF\t10\t10\t16\tsRGB\tUndefined\tTopLeft\n",
        )
        .unwrap();
        assert_eq!((tiff.alpha, tiff.animated, tiff.frames), (None, false, 2));
    }

    #[test]
    fn a_model_that_repeats_the_make_is_not_doubled() {
        let i =
            parse_image("JPEG\t1\t1\t8\tsRGB\tUndefined\tTopLeft\tCanon\tCanon EOS R5\n").unwrap();
        assert_eq!(i.camera.as_deref(), Some("Canon EOS R5"));
    }

    #[test]
    fn contents_that_do_not_match_the_name_get_a_note() {
        let note = mismatch_note(Format::Jpg, "PNG").unwrap();
        assert!(
            note.contains("The contents are PNG, though the name says .jpg"),
            "{note}"
        );
        assert_eq!(mismatch_note(Format::Jpg, "JPEG"), None);
        assert_eq!(mismatch_note(Format::Heif, "HEIC"), None);
        assert_eq!(mismatch_note(Format::Svg, "SVG"), None);
    }

    #[test]
    fn a_missing_file_is_an_error_and_an_unknown_one_is_just_its_size() {
        let dir = tempfile::tempdir().unwrap();
        let r = Resolver::new();
        let gone = describe(&dir.path().join("gone.mp4"), &r);
        assert_eq!(gone.error.unwrap().code, ErrorCode::InputNotFound);
        let zip = dir.path().join("a.zip");
        std::fs::write(&zip, b"12345").unwrap();
        let info = describe(&zip, &r);
        assert_eq!((info.format, info.bytes), (None, 5));
        assert!(info.targets.is_empty() && info.missing.is_empty() && info.error.is_none());
    }

    /// A backend that is not there is reported with its fix, and the rest
    /// of the answer still comes back.
    #[test]
    fn a_missing_backend_is_reported_with_the_format_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        std::fs::write(&clip, b"not really").unwrap();
        let mut r = Resolver::new();
        r.overrides_only();
        let info = describe(&clip, &r);
        assert_eq!((info.format, info.bytes), (Some(Format::Mp4), 10));
        assert!(info.targets.contains(&Format::Gif));
        assert_eq!(info.missing.len(), 1, "{:?}", info.missing);
        assert_eq!(info.missing[0].backend, Some(Backend::Ffprobe));
        assert!(info.media.is_none());
    }

    #[test]
    fn office_files_say_whether_they_are_encrypted() {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/encrypted.docx");
        let info = describe(&fixture, &Resolver::new());
        assert!(info.office.unwrap().encrypted);
        let plain = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample.docx");
        assert!(!describe(&plain, &Resolver::new()).office.unwrap().encrypted);
    }
}
