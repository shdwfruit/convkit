//! `conv info FILE...`: what each file is, one block per file.
//!
//! The reading is `convkit_core::info::describe`; this only lays it out.
//! A missing backend is shown in the file's block, with how to install it,
//! next to what could be read without it. Nothing is installed from here:
//! a description is not worth an install prompt in the middle of a list.

use std::io::Write;
use std::path::{Path, PathBuf};

use convkit_core::info::{self, FileInfo, ImageInfo, MediaInfo, StreamInfo, StreamKind};
use convkit_core::pdf::inspect::PdfInfo;
use convkit_core::{Backend, ConvError, ErrorCode, Kind};
use serde_json::json;

use crate::cli::Cli;
use crate::input::{expand_globs, natural_cmp};
use crate::render;

/// Exit codes, worst first: a path that is not there (2), then a backend
/// that is missing (3), then a file a backend could not read (1). Every
/// file that could be described is still printed.
pub fn run(cli: &Cli, paths: &[PathBuf]) -> i32 {
    // Every positional is an input, so all of them are globbable, as in
    // `conv scan`.
    let roots = expand_globs(paths, paths.len());
    let mut files = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    for root in &roots {
        if root.is_dir() {
            match std::fs::read_dir(root) {
                Ok(entries) => {
                    let mut here: Vec<PathBuf> = entries
                        .flatten()
                        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                        .map(|e| e.path())
                        .collect();
                    here.sort_by(|a, b| {
                        natural_cmp(
                            &a.file_name().unwrap_or_default().to_string_lossy(),
                            &b.file_name().unwrap_or_default().to_string_lossy(),
                        )
                    });
                    files.extend(here);
                }
                Err(e) => problems.push(format!("cannot read directory {}: {e}", root.display())),
            }
        } else if root.is_file() {
            files.push(root.clone());
        } else if root.exists() {
            problems.push(format!("not a regular file: {}", root.display()));
        } else {
            problems.push(format!("not found: {}", root.display()));
        }
    }

    let resolver = cli.resolver();
    let infos: Vec<FileInfo> = files.iter().map(|f| info::describe(f, &resolver)).collect();

    let code = if !problems.is_empty() {
        ErrorCode::InvalidInvocation.exit_code()
    } else if infos.iter().any(|i| !i.missing.is_empty()) {
        ErrorCode::BackendMissing.exit_code()
    } else if infos.iter().any(|i| i.error.is_some()) {
        ErrorCode::ConversionFailed.exit_code()
    } else {
        0
    };

    if cli.json {
        let envelope = json!({ "ok": code == 0, "files": infos });
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let _ = writeln!(out, "{}", serde_json::to_string_pretty(&envelope).unwrap());
        if !problems.is_empty() {
            render::print_error(
                true,
                &ConvError::new(ErrorCode::InvalidInvocation, problems.join("; ")),
            );
        }
    } else {
        for problem in &problems {
            eprintln!("error: {problem}");
        }
        if !cli.quiet {
            let styled = render::stdout_styled();
            let text: Vec<String> = infos.iter().map(|i| block(i, styled)).collect();
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            // A closed pipe (`conv info * | head`) ends the listing quietly.
            let _ = write!(out, "{}", text.join("\n"));
        }
    }
    code
}

/// Labels are padded to this, so every block lines up with the others.
const LABEL: usize = 14;

/// One file's block: its name, then a line for each thing known about it.
fn block(info: &FileInfo, styled: bool) -> String {
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut row = |label: &str, value: String| rows.push((label.to_string(), value));

    row("Format", format_line(info));
    if let Some(m) = &info.media {
        media_rows(m, &mut row);
    }
    if let Some(i) = &info.image {
        image_rows(i, &mut row);
    }
    if let Some(p) = &info.pdf {
        pdf_rows(p, &mut row);
    }
    if let Some(o) = &info.office {
        row("Encrypted", yes_no(o.encrypted).into());
        if o.macros {
            row("Macros", "yes".into());
        }
    }
    if info.location() {
        row(
            "Location",
            "a GPS position is recorded; --strip-metadata removes it".into(),
        );
    }
    for note in &info.notes {
        row("Note", note.clone());
    }
    if let Some(e) = &info.error {
        row("Error", e.message.clone());
    }
    for e in &info.missing {
        row("Missing", missing_line(e, info.kind));
    }
    if info.format.is_some() {
        row(
            "Converts to",
            if info.targets.is_empty() {
                "nothing yet".to_string()
            } else {
                info.targets
                    .iter()
                    .map(|t| t.ext())
                    .collect::<Vec<_>>()
                    .join(" ")
            },
        );
    }

    let mut s = format!("{}\n", render::bold(&info.path, styled));
    for (label, value) in rows {
        s.push_str(&format!("  {label:<LABEL$}{value}\n"));
    }
    s
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

/// `HEIC image, 1.5 MB`, or what conv makes of a name it does not know.
fn format_line(info: &FileInfo) -> String {
    let size = convkit_core::size::human(info.bytes);
    match (info.format, info.kind) {
        (Some(f), Some(kind)) => {
            let kind = match kind {
                Kind::Video => "video",
                Kind::Audio => "audio",
                Kind::Image => "image",
                Kind::Document => "document",
            };
            format!("{} {kind}, {size}", f.ext().to_ascii_uppercase())
        }
        _ => {
            let ext = Path::new(&info.path)
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()));
            match ext {
                Some(ext) => format!("{ext} is not a format conv knows, {size}"),
                None => format!("no extension, so conv cannot tell the format, {size}"),
            }
        }
    }
}

/// What the missing backend would have shown, and how to get it.
fn missing_line(e: &ConvError, kind: Option<Kind>) -> String {
    let what = match (e.backend, kind) {
        (Some(Backend::Ffprobe), _) => "streams, duration and bitrate are not shown",
        (Some(Backend::Magick), _) => "dimensions, colour, camera and location are not shown",
        _ => "some details are not shown",
    };
    let fix = e
        .remediation
        .as_ref()
        .and_then(|r| r.managed.clone().or_else(|| r.manual.clone()));
    match fix {
        Some(fix) => format!("{}, so {what}. Try: {fix}", e.message),
        None => format!("{}, so {what}", e.message),
    }
}

// ---------------------------------------------------------------------------
// Video and audio

fn media_rows(m: &MediaInfo, row: &mut impl FnMut(&str, String)) {
    if let Some(c) = &m.container {
        row("Container", c.clone());
    }
    if let Some(d) = m.duration_ms {
        row("Duration", duration(d));
    }
    if let Some(b) = m.bit_rate {
        row("Bitrate", bitrate(b));
    }
    let of = |k: StreamKind| m.streams.iter().filter(move |s| s.kind == k);
    for s in of(StreamKind::Video).filter(|s| !s.cover_art) {
        row("Video", video_line(s));
    }
    for s in of(StreamKind::Audio) {
        row("Audio", audio_line(s));
    }
    let subs: Vec<String> = of(StreamKind::Subtitle).map(track_name).collect();
    if !subs.is_empty() {
        row("Subtitles", format!("{} ({})", subs.len(), subs.join(", ")));
    }
    for s in of(StreamKind::Video).filter(|s| s.cover_art) {
        let size = match (s.width, s.height) {
            (Some(w), Some(h)) => format!(", {w} x {h}"),
            _ => String::new(),
        };
        row("Cover art", format!("{}{size}", s.codec));
    }
    if m.chapters > 0 {
        row("Chapters", m.chapters.to_string());
    }
    let attachments = of(StreamKind::Attachment).count();
    if attachments > 0 {
        row("Attachments", attachments.to_string());
    }
    let data: Vec<String> = of(StreamKind::Data).map(|s| s.codec.clone()).collect();
    if !data.is_empty() {
        row("Data", format!("{} ({})", data.len(), data.join(", ")));
    }
    for (key, label) in [
        ("title", "Title"),
        ("artist", "Artist"),
        ("album", "Album"),
        ("album_artist", "Album artist"),
        ("genre", "Genre"),
        ("date", "Date"),
        ("track", "Track"),
    ] {
        if let Some(v) = m.tags.get(key) {
            row(label, v.clone());
        }
    }
    if let Some(c) = &m.camera {
        row("Camera", c.clone());
    }
    if let Some(c) = &m.created {
        row("Created", c.clone());
    }
}

/// A subtitle track by its language, or its codec when it names none.
fn track_name(s: &StreamInfo) -> String {
    let mut name = match &s.language {
        Some(l) => format!("{l} {}", s.codec),
        None => s.codec.clone(),
    };
    if let Some(t) = &s.title {
        name.push_str(&format!(" \"{t}\""));
    }
    name
}

fn codec(s: &StreamInfo) -> String {
    match &s.profile {
        Some(p) => format!("{} ({p})", s.codec),
        None => s.codec.clone(),
    }
}

fn video_line(s: &StreamInfo) -> String {
    let mut parts = vec![codec(s)];
    if let (Some(w), Some(h)) = (s.width, s.height) {
        match s.rotation {
            Some(r @ (90 | 270)) => {
                parts.push(format!("{w} x {h}, {}, shown {h} x {w}", turned(r)))
            }
            Some(r) => parts.push(format!("{w} x {h}, {}", turned(r))),
            None => parts.push(format!("{w} x {h}")),
        }
    }
    if let Some(fps) = s.fps {
        let fps = trim_float(fps, 2);
        parts.push(if s.variable_frame_rate {
            format!("about {fps} fps (variable)")
        } else {
            format!("{fps} fps")
        });
    }
    if let Some(p) = &s.pixel_format {
        parts.push(p.clone());
    }
    match (&s.hdr, s.bit_depth) {
        (Some(hdr), Some(d)) => parts.push(format!("{d}-bit {hdr}")),
        (Some(hdr), None) => parts.push(hdr.clone()),
        (None, Some(d)) if d > 8 => parts.push(format!("{d}-bit")),
        _ => {}
    }
    stream_tail(s, &mut parts);
    parts.join(", ")
}

fn audio_line(s: &StreamInfo) -> String {
    let mut parts = vec![codec(s)];
    if let Some(r) = s.sample_rate {
        parts.push(format!("{} kHz", trim_float(f64::from(r) / 1000.0, 1)));
    }
    match (&s.channel_layout, s.channels) {
        (Some(l), _) => parts.push(l.clone()),
        (None, Some(1)) => parts.push("1 channel".into()),
        (None, Some(c)) => parts.push(format!("{c} channels")),
        _ => {}
    }
    stream_tail(s, &mut parts);
    parts.join(", ")
}

/// The bitrate, language and title every stream may have.
fn stream_tail(s: &StreamInfo, parts: &mut Vec<String>) {
    if let Some(b) = s.bit_rate {
        parts.push(bitrate(b));
    }
    if let Some(l) = &s.language {
        parts.push(l.clone());
    }
    if let Some(t) = &s.title {
        parts.push(format!("\"{t}\""));
    }
}

fn turned(clockwise: u16) -> String {
    match clockwise {
        180 => "turned 180°".to_string(),
        270 => "turned 90° counter-clockwise".to_string(),
        r => format!("turned {r}° clockwise"),
    }
}

/// `2.0 s`, `1:02`, `1:02:03`.
fn duration(ms: u64) -> String {
    if ms < 60_000 {
        return format!("{:.1} s", ms as f64 / 1000.0);
    }
    let secs = (ms + 500) / 1000;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// `128 kb/s`, `12.5 Mb/s`.
fn bitrate(bps: u64) -> String {
    if bps >= 10_000_000 {
        format!("{} Mb/s", trim_float(bps as f64 / 1_000_000.0, 1))
    } else {
        format!("{} kb/s", (bps + 500) / 1000)
    }
}

/// `places` decimals, without trailing zeros: 29.97, 30, 44.1.
fn trim_float(v: f64, places: usize) -> String {
    let s = format!("{v:.places$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// Pictures

fn image_rows(i: &ImageInfo, row: &mut impl FnMut(&str, String)) {
    let shown = match i.orientation {
        Some(o) if o.on_its_side() => format!(" (shown {} x {})", i.height, i.width),
        _ => String::new(),
    };
    row("Dimensions", format!("{} x {}{shown}", i.width, i.height));
    let mut colour = Vec::new();
    if let Some(c) = &i.colorspace {
        colour.push(c.clone());
    }
    if let Some(d) = i.bit_depth {
        colour.push(format!("{d}-bit"));
    }
    colour.push(match &i.profile {
        Some(p) => format!("{p} profile"),
        None => "no colour profile".to_string(),
    });
    row("Colour", colour.join(", "));
    if let Some(a) = i.alpha {
        row("Transparency", yes_no(a).into());
    }
    if i.animated {
        row("Animation", format!("{} frames", i.frames));
    } else if !i.frame_sizes.is_empty() {
        let sizes: Vec<String> = i
            .frame_sizes
            .iter()
            .map(|(w, h)| format!("{w}x{h}"))
            .collect();
        row("Sizes", sizes.join(" "));
    } else if i.frames > 1 {
        row("Frames", format!("{} (pages or layers)", i.frames));
    }
    if let Some(o) = i.orientation {
        let how = match (o.mirrored, o.clockwise) {
            (false, 0) => "shown as stored".to_string(),
            (true, 0) => "shown mirrored".to_string(),
            (false, r) => format!("shown {}", turned(r)),
            (true, r) => format!("shown mirrored and {}", turned(r)),
        };
        row("Orientation", format!("tag {}, {how}", o.tag));
    }
    if let Some(c) = &i.camera {
        row("Camera", c.clone());
    }
    if let Some(t) = &i.taken {
        row("Taken", t.clone());
    }
}

// ---------------------------------------------------------------------------
// PDFs

fn pdf_rows(p: &PdfInfo, row: &mut impl FnMut(&str, String)) {
    if let Some(v) = &p.version {
        row("Version", format!("PDF {v}"));
    }
    match p.pages {
        Some(n) => row("Pages", n.to_string()),
        None if p.encrypted => row("Pages", "unknown: the page list is encrypted".into()),
        None => {}
    }
    if let Some(size) = p.page_size {
        let mm = |pt: f64| (pt * 25.4 / 72.0).round();
        let dims = format!("{} x {} mm", mm(size.width_pt), mm(size.height_pt));
        let mut s = match size.paper() {
            Some(paper) => format!("{paper}, {dims}"),
            None => format!(
                "{} x {} pt, {dims}",
                trim_float(size.width_pt, 1),
                trim_float(size.height_pt, 1)
            ),
        };
        if p.mixed_sizes {
            s.push_str(" (first page; others differ)");
        }
        row("Page size", s);
    }
    row("Encrypted", yes_no(p.encrypted).into());
    if let Some(t) = &p.title {
        row("Title", t.clone());
    }
    if let Some(c) = &p.creator {
        row("Creator", c.clone());
    }
    if let Some(prod) = &p.producer {
        row("Producer", prod.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use convkit_core::info::OrientationInfo;
    use convkit_core::pdf::inspect::PageSize;
    use convkit_core::Format;

    fn file(format: Option<Format>) -> FileInfo {
        FileInfo {
            path: "x".into(),
            format,
            kind: format.map(|f| f.kind()),
            bytes: 1_600_000,
            media: None,
            image: None,
            pdf: None,
            office: None,
            targets: vec![Format::Jpg, Format::Png],
            notes: vec![],
            missing: vec![],
            error: None,
        }
    }

    fn stream(kind: StreamKind, codec: &str) -> StreamInfo {
        StreamInfo {
            index: 0,
            kind,
            codec: codec.into(),
            profile: None,
            width: None,
            height: None,
            rotation: None,
            frame_rate: None,
            fps: None,
            variable_frame_rate: false,
            pixel_format: None,
            bit_depth: None,
            hdr: None,
            sample_rate: None,
            channels: None,
            channel_layout: None,
            bit_rate: None,
            language: None,
            title: None,
            cover_art: false,
        }
    }

    #[test]
    fn durations_bitrates_and_rates_read_plainly() {
        assert_eq!(duration(2_000), "2.0 s");
        assert_eq!(duration(62_400), "1:02");
        assert_eq!(duration(3_723_000), "1:02:03");
        assert_eq!(bitrate(128_000), "128 kb/s");
        assert_eq!(bitrate(12_500_000), "12.5 Mb/s");
        assert_eq!(trim_float(29.97, 2), "29.97");
        assert_eq!(trim_float(30.0, 2), "30");
        assert_eq!(trim_float(44.1, 1), "44.1");
    }

    #[test]
    fn a_turned_hdr_video_line_says_how_it_is_shown() {
        let mut v = stream(StreamKind::Video, "hevc");
        v.profile = Some("Main 10".into());
        (v.width, v.height, v.rotation) = (Some(1920), Some(1080), Some(90));
        (v.fps, v.pixel_format) = (Some(29.97), Some("yuv420p10le".into()));
        (v.bit_depth, v.hdr) = (Some(10), Some("HLG".into()));
        assert_eq!(
            video_line(&v),
            "hevc (Main 10), 1920 x 1080, turned 90° clockwise, shown 1080 x 1920, \
             29.97 fps, yuv420p10le, 10-bit HLG"
        );
        let mut a = stream(StreamKind::Audio, "aac");
        (a.sample_rate, a.channel_layout) = (Some(44_100), Some("stereo".into()));
        (a.bit_rate, a.language) = (Some(128_000), Some("eng".into()));
        assert_eq!(audio_line(&a), "aac, 44.1 kHz, stereo, 128 kb/s, eng");
    }

    #[test]
    fn a_photo_block_lines_up_and_mentions_its_location() {
        let mut f = file(Some(Format::Heic));
        f.path = "photo.heic".into();
        f.image = Some(ImageInfo {
            coder: "HEIC".into(),
            width: 4032,
            height: 3024,
            bit_depth: Some(8),
            colorspace: Some("sRGB".into()),
            profile: Some("Display P3".into()),
            alpha: Some(false),
            frames: 1,
            animated: false,
            frame_sizes: vec![],
            orientation: OrientationInfo::from_tag(6),
            camera: Some("Apple iPhone 14 Pro".into()),
            taken: Some("2025-12-27 13:59:08".into()),
            location: true,
        });
        let text = block(&f, false);
        assert_eq!(
            text,
            "photo.heic\n\
             \x20 Format        HEIC image, 1.5 MB\n\
             \x20 Dimensions    4032 x 3024 (shown 3024 x 4032)\n\
             \x20 Colour        sRGB, 8-bit, Display P3 profile\n\
             \x20 Transparency  no\n\
             \x20 Orientation   tag 6, shown turned 90° clockwise\n\
             \x20 Camera        Apple iPhone 14 Pro\n\
             \x20 Taken         2025-12-27 13:59:08\n\
             \x20 Location      a GPS position is recorded; --strip-metadata removes it\n\
             \x20 Converts to   jpg png\n"
        );
    }

    #[test]
    fn a_pdf_block_names_its_paper() {
        let mut f = file(Some(Format::Pdf));
        f.pdf = Some(PdfInfo {
            version: Some("1.7".into()),
            pages: Some(3),
            page_size: Some(PageSize {
                width_pt: 595.28,
                height_pt: 841.89,
            }),
            mixed_sizes: true,
            encrypted: false,
            producer: Some("LibreOffice".into()),
            creator: None,
            title: None,
        });
        let text = block(&f, false);
        assert!(text.contains("  Pages         3\n"), "{text}");
        assert!(
            text.contains("  Page size     A4, 210 x 297 mm (first page; others differ)\n"),
            "{text}"
        );
        assert!(text.contains("  Encrypted     no\n"), "{text}");
        assert!(text.contains("  Producer      LibreOffice\n"), "{text}");
    }

    /// A missing backend says what it would have shown and how to get it,
    /// under what could be shown without it.
    #[test]
    fn a_missing_backend_says_what_is_not_shown_and_how_to_install_it() {
        let mut f = file(Some(Format::Mp4));
        f.missing = vec![ConvError::backend_missing(Backend::Ffprobe)];
        let text = block(&f, false);
        assert!(
            text.contains("  Format        MP4 video, 1.5 MB\n"),
            "{text}"
        );
        assert!(
            text.contains("  Missing       ffprobe not found, so streams, duration and bitrate are not shown. Try: "),
            "{text}"
        );
    }

    #[test]
    fn an_unknown_extension_says_so() {
        let mut f = file(None);
        f.path = "a.zip".into();
        f.targets = vec![];
        let text = block(&f, false);
        assert!(
            text.contains(".zip is not a format conv knows, 1.5 MB"),
            "{text}"
        );
        assert!(!text.contains("Converts to"), "{text}");
    }
}
