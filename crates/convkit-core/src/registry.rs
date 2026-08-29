use std::collections::BTreeMap;
#[cfg(test)]
use std::path::Path;
use std::sync::LazyLock;

use crate::{Arg, Backend, Format, OutputMode, Recipe, Step};

/// JPEG/WebP/AVIF quality. Visually transparent without bloat; see spec §7.4.
const IMAGE_QUALITY: &str = "92";
/// DPI used when rasterising vectors. 384 gives a crisp result at 4x a 96dpi
/// nominal size without producing an absurd bitmap.
const SVG_DENSITY: &str = "384";

/// Lossy raster targets take a quality flag; lossless ones must not.
fn is_lossy(f: Format) -> bool {
    matches!(f, Format::Jpg | Format::Webp | Format::Avif)
}

macro_rules! step {
    ($backend:expr, [$($arg:expr),* $(,)?]) => {
        Step {
            backend: $backend,
            args: &[$($arg),*],
            output: OutputMode::Path,
            intermediate_ext: None,
        }
    };
}

// --- Image family ------------------------------------------------------------

const IMG_LOSSY: Recipe = Recipe {
    steps: &[step!(
        Backend::Magick,
        [
            Arg::Input,
            Arg::Lit("-auto-orient"),
            Arg::Lit("-quality"),
            Arg::Lit(IMAGE_QUALITY),
            Arg::Output,
        ]
    )],
    warnings: &[],
};

const IMG_LOSSLESS: Recipe = Recipe {
    steps: &[step!(
        Backend::Magick,
        [Arg::Input, Arg::Lit("-auto-orient"), Arg::Output]
    )],
    warnings: &[],
};

const SVG_TO_LOSSY: Recipe = Recipe {
    steps: &[step!(
        Backend::Magick,
        [
            Arg::Lit("-density"),
            Arg::Lit(SVG_DENSITY),
            Arg::Lit("-background"),
            Arg::Lit("none"),
            Arg::Input,
            Arg::Lit("-flatten"),
            Arg::Lit("-quality"),
            Arg::Lit(IMAGE_QUALITY),
            Arg::Output,
        ]
    )],
    warnings: &[],
};

const SVG_TO_LOSSLESS: Recipe = Recipe {
    steps: &[step!(
        Backend::Magick,
        [
            Arg::Lit("-density"),
            Arg::Lit(SVG_DENSITY),
            Arg::Lit("-background"),
            Arg::Lit("none"),
            Arg::Input,
            Arg::Output,
        ]
    )],
    warnings: &[],
};

const IMG_TO_PDF: Recipe = Recipe {
    steps: &[step!(
        Backend::Magick,
        [Arg::Inputs, Arg::Lit("-auto-orient"), Arg::Output]
    )],
    warnings: &[],
};

/// Raster image formats that participate in the all-directions image family.
const RASTER: &[Format] = &[
    Format::Heic,
    Format::Heif,
    Format::Jpg,
    Format::Png,
    Format::Webp,
    Format::Avif,
    Format::Tiff,
    Format::Bmp,
];

/// Raster formats we are willing to *write*. HEIC/HEIF are read-only in v1:
/// encoding them needs a patent-encumbered encoder most ImageMagick builds omit.
const RASTER_WRITABLE: &[Format] = &[
    Format::Jpg,
    Format::Png,
    Format::Webp,
    Format::Avif,
    Format::Tiff,
    Format::Bmp,
];

type Table = BTreeMap<(Format, Format), Recipe>;

fn insert_image_family(t: &mut Table) {
    for &from in RASTER {
        for &to in RASTER_WRITABLE {
            if from == to {
                continue;
            }
            t.insert(
                (from, to),
                if is_lossy(to) {
                    IMG_LOSSY
                } else {
                    IMG_LOSSLESS
                },
            );
        }
        t.insert((from, Format::Pdf), IMG_TO_PDF);
    }

    for &to in &[Format::Png, Format::Jpg] {
        t.insert(
            (Format::Svg, to),
            if is_lossy(to) {
                SVG_TO_LOSSY
            } else {
                SVG_TO_LOSSLESS
            },
        );
    }
}

// --- Media family ------------------------------------------------------------

/// Constant-quality anchor for H.264. Visually transparent at normal viewing
/// distance; see spec §7.4.
const CRF: &str = "20";
const AUDIO_BITRATE: &str = "160k";
/// GIF frame rate and maximum width. Capped, never upscaled.
///
/// Documents the value baked directly into `GIF_FILTER` below: `concat!` only
/// accepts literals, not paths to other consts, so this cannot be spliced in
/// programmatically. Kept for the reader; not referenced by code.
#[allow(dead_code)]
const GIF_FPS: &str = "15";

/// Escaped comma is required: ffmpeg's filter parser splits unescaped commas
/// into separate filters. No shell is involved, so the backslash is literal.
const GIF_FILTER: &str = concat!(
    r"fps=15,scale=w=min(640\,iw):h=-2:flags=lanczos,split[a][b];",
    "[a]palettegen=stats_mode=diff[p];",
    "[b][p]paletteuse=dither=bayer:bayer_scale=3"
);

const VIDEO_TO_MP4: Recipe = Recipe {
    steps: &[step!(
        Backend::Ffmpeg,
        [
            Arg::Lit("-i"),
            Arg::Input,
            Arg::Lit("-c:v"),
            Arg::Lit("libx264"),
            Arg::Lit("-crf"),
            Arg::Lit(CRF),
            Arg::Lit("-preset"),
            Arg::Lit("medium"),
            Arg::Lit("-pix_fmt"),
            Arg::Lit("yuv420p"),
            Arg::Lit("-c:a"),
            Arg::Lit("aac"),
            Arg::Lit("-b:a"),
            Arg::Lit(AUDIO_BITRATE),
            Arg::Lit("-movflags"),
            Arg::Lit("+faststart"),
            Arg::Lit("-y"),
            Arg::Output,
        ]
    )],
    warnings: &[],
};

const VIDEO_TO_WEBM: Recipe = Recipe {
    steps: &[step!(
        Backend::Ffmpeg,
        [
            Arg::Lit("-i"),
            Arg::Input,
            Arg::Lit("-c:v"),
            Arg::Lit("libvpx-vp9"),
            Arg::Lit("-crf"),
            Arg::Lit("32"),
            Arg::Lit("-b:v"),
            Arg::Lit("0"),
            Arg::Lit("-c:a"),
            Arg::Lit("libopus"),
            Arg::Lit("-b:a"),
            Arg::Lit("128k"),
            Arg::Lit("-y"),
            Arg::Output,
        ]
    )],
    warnings: &[],
};

/// Stream-copy remux. Selected dynamically by `plan::build` when the source
/// codecs are already legal in the target container. See Task 7.
pub const REMUX: Recipe = Recipe {
    steps: &[step!(
        Backend::Ffmpeg,
        [
            Arg::Lit("-i"),
            Arg::Input,
            Arg::Lit("-c"),
            Arg::Lit("copy"),
            Arg::Lit("-movflags"),
            Arg::Lit("+faststart"),
            Arg::Lit("-y"),
            Arg::Output,
        ]
    )],
    warnings: &[],
};

const TO_GIF: Recipe = Recipe {
    steps: &[step!(
        Backend::Ffmpeg,
        [
            Arg::Lit("-i"),
            Arg::Input,
            Arg::Lit("-vf"),
            Arg::Lit(GIF_FILTER),
            Arg::Lit("-loop"),
            Arg::Lit("0"),
            Arg::Lit("-y"),
            Arg::Output,
        ]
    )],
    warnings: &[],
};

const GIF_TO_MP4: Recipe = Recipe {
    steps: &[step!(
        Backend::Ffmpeg,
        [
            Arg::Lit("-i"),
            Arg::Input,
            Arg::Lit("-vf"),
            Arg::Lit("scale=trunc(iw/2)*2:trunc(ih/2)*2"),
            Arg::Lit("-c:v"),
            Arg::Lit("libx264"),
            Arg::Lit("-crf"),
            Arg::Lit(CRF),
            Arg::Lit("-pix_fmt"),
            Arg::Lit("yuv420p"),
            Arg::Lit("-movflags"),
            Arg::Lit("+faststart"),
            Arg::Lit("-y"),
            Arg::Output,
        ]
    )],
    warnings: &[],
};

macro_rules! audio_recipe {
    ([$($codec:expr),* $(,)?]) => {
        Recipe {
            steps: &[step!(
                Backend::Ffmpeg,
                [
                    Arg::Lit("-i"), Arg::Input,
                    Arg::Lit("-vn"),
                    $($codec,)*
                    Arg::Lit("-y"), Arg::Output,
                ]
            )],
            warnings: &[],
        }
    };
}

const TO_MP3: Recipe = audio_recipe!([
    Arg::Lit("-c:a"),
    Arg::Lit("libmp3lame"),
    Arg::Lit("-q:a"),
    Arg::Lit("2")
]);
const TO_M4A: Recipe = audio_recipe!([
    Arg::Lit("-c:a"),
    Arg::Lit("aac"),
    Arg::Lit("-b:a"),
    Arg::Lit("192k")
]);
const TO_WAV: Recipe = audio_recipe!([Arg::Lit("-c:a"), Arg::Lit("pcm_s16le")]);
const TO_FLAC: Recipe = audio_recipe!([Arg::Lit("-c:a"), Arg::Lit("flac")]);

pub const MP4_COMPATIBLE_VIDEO: &[&str] = &["h264", "hevc", "mpeg4", "av1"];
pub const MP4_COMPATIBLE_AUDIO: &[&str] = &["aac", "mp3", "ac3", "alac"];
pub const WEBM_COMPATIBLE_VIDEO: &[&str] = &["vp8", "vp9", "av1"];
pub const WEBM_COMPATIBLE_AUDIO: &[&str] = &["opus", "vorbis"];

const VIDEO: &[Format] = &[
    Format::Mp4,
    Format::Mov,
    Format::Mkv,
    Format::Webm,
    Format::Avi,
];
const AUDIO_TARGETS: &[(Format, Recipe)] = &[
    (Format::Mp3, TO_MP3),
    (Format::M4a, TO_M4A),
    (Format::Wav, TO_WAV),
    (Format::Flac, TO_FLAC),
];

fn insert_media_family(t: &mut Table) {
    for &from in VIDEO {
        if from != Format::Mp4 {
            t.insert((from, Format::Mp4), VIDEO_TO_MP4);
        }
        if from != Format::Webm {
            t.insert((from, Format::Webm), VIDEO_TO_WEBM);
        }
        t.insert((from, Format::Gif), TO_GIF);
        for &(to, recipe) in AUDIO_TARGETS {
            t.insert((from, to), recipe);
        }
    }

    // Audio sources transcode between audio targets.
    for &from in &[Format::Mp3, Format::M4a, Format::Wav, Format::Flac] {
        for &(to, recipe) in AUDIO_TARGETS {
            if from != to {
                t.insert((from, to), recipe);
            }
        }
    }

    t.insert((Format::Gif, Format::Mp4), GIF_TO_MP4);
}

static TABLE: LazyLock<Table> = LazyLock::new(|| {
    let mut t = Table::new();
    insert_image_family(&mut t);
    insert_media_family(&mut t);
    // Task 6 adds insert_document_family(&mut t);
    t
});

pub fn lookup(from: Format, to: Format) -> Option<Recipe> {
    TABLE.get(&(from, to)).copied()
}

pub fn all_pairs() -> Vec<(Format, Format)> {
    TABLE.keys().copied().collect()
}

pub fn backends_for(from: Format, to: Format) -> Vec<Backend> {
    lookup(from, to)
        .map(|r| r.steps.iter().map(|s| s.backend).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heic_to_jpg_auto_orients_and_sets_quality() {
        let r = lookup(Format::Heic, Format::Jpg).expect("heic->jpg must exist");
        assert_eq!(r.steps.len(), 1);
        let argv = r.steps[0].render(&[Path::new("in.heic")], Path::new("out.jpg"));
        assert_eq!(
            argv,
            vec!["in.heic", "-auto-orient", "-quality", "92", "out.jpg"]
        );
    }

    #[test]
    fn lossless_targets_do_not_carry_a_quality_flag() {
        let r = lookup(Format::Jpg, Format::Png).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.jpg")], Path::new("out.png"));
        assert!(!argv.contains(&"-quality".to_string()), "{argv:?}");
        assert!(argv.contains(&"-auto-orient".to_string()));
    }

    #[test]
    fn svg_rasterises_at_high_density_with_transparent_background() {
        let r = lookup(Format::Svg, Format::Png).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.svg")], Path::new("out.png"));
        assert_eq!(argv[0], "-density");
        assert_eq!(argv[1], "384");
        assert!(argv.contains(&"none".to_string()));
    }

    #[test]
    fn images_merge_into_pdf_using_every_input() {
        let r = lookup(Format::Png, Format::Pdf).unwrap();
        let argv = r.steps[0].render(
            &[Path::new("a.png"), Path::new("b.png")],
            Path::new("out.pdf"),
        );
        assert!(argv.contains(&"a.png".to_string()));
        assert!(argv.contains(&"b.png".to_string()));
    }

    #[test]
    fn no_pair_converts_a_format_to_itself() {
        for (from, to) in all_pairs() {
            assert_ne!(from, to, "self-pair {from:?} must not be registered");
        }
    }

    #[test]
    fn no_pdf_route_uses_pandoc() {
        for (from, to) in all_pairs() {
            if from == Format::Pdf {
                assert!(
                    !backends_for(from, to).contains(&Backend::Pandoc),
                    "pandoc cannot read PDF, but {from:?}->{to:?} routes to it"
                );
            }
        }
    }

    #[test]
    fn gif_uses_a_generated_palette_not_the_default_web_palette() {
        let r = lookup(Format::Mp4, Format::Gif).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.mp4")], Path::new("out.gif"));
        let vf = argv.join(" ");
        assert!(vf.contains("palettegen=stats_mode=diff"), "{vf}");
        assert!(vf.contains("paletteuse=dither=bayer"), "{vf}");
        assert!(vf.contains("fps=15"), "{vf}");
        assert!(
            vf.contains(r"min(640\,iw)"),
            "comma must stay escaped: {vf}"
        );
    }

    #[test]
    fn video_transcode_uses_the_spec_quality_anchors() {
        let r = lookup(Format::Mkv, Format::Mp4).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.mkv")], Path::new("out.mp4"));
        assert!(argv.windows(2).any(|w| w == ["-crf", "20"]), "{argv:?}");
        assert!(argv.windows(2).any(|w| w == ["-b:a", "160k"]), "{argv:?}");
        assert!(argv.contains(&"+faststart".to_string()), "{argv:?}");
    }

    #[test]
    fn audio_extraction_drops_the_video_stream() {
        let r = lookup(Format::Mp4, Format::Mp3).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.mp4")], Path::new("out.mp3"));
        assert!(argv.contains(&"-vn".to_string()), "{argv:?}");
    }

    #[test]
    fn gif_to_mp4_forces_even_dimensions() {
        let r = lookup(Format::Gif, Format::Mp4).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.gif")], Path::new("out.mp4"));
        let joined = argv.join(" ");
        assert!(joined.contains("trunc(iw/2)*2"), "{joined}");
        assert!(joined.contains("yuv420p"), "{joined}");
    }

    #[test]
    fn remux_copies_streams_instead_of_re_encoding() {
        let argv = REMUX.steps[0].render(&[Path::new("in.mkv")], Path::new("out.mp4"));
        assert!(argv.windows(2).any(|w| w == ["-c", "copy"]), "{argv:?}");
    }
}
