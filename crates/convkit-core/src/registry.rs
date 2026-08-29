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

/// `-background white` plus `-alpha remove -alpha off` composites onto a
/// white canvas and then drops the alpha channel outright before `-flatten`.
/// JPEG cannot store transparency; the previous `-background none` composited
/// onto a transparent canvas, and the JPEG coder then discarded that alpha
/// and left the underlying black RGB behind, so every SVG-to-JPEG conversion
/// silently rendered on a black field.
const SVG_TO_LOSSY: Recipe = Recipe {
    steps: &[step!(
        Backend::Magick,
        [
            Arg::Lit("-density"),
            Arg::Lit(SVG_DENSITY),
            Arg::Lit("-background"),
            Arg::Lit("white"),
            Arg::Input,
            Arg::Lit("-alpha"),
            Arg::Lit("remove"),
            Arg::Lit("-alpha"),
            Arg::Lit("off"),
            Arg::Lit("-flatten"),
            Arg::Lit("-quality"),
            Arg::Lit(IMAGE_QUALITY),
            Arg::Output,
        ]
    )],
    warnings: &["Transparency is flattened onto a white background; JPEG has no alpha channel."],
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

/// GIF frame rate and maximum width, each spelled once and spliced into
/// `GIF_FILTER` via `concat!`. `concat!` requires every argument to expand to
/// a literal token; a zero-arg macro satisfies that, a path to a `const` item
/// does not — hence macros here instead of `const GIF_FPS`/`GIF_MAX_W`.
/// Capped, never upscaled.
macro_rules! gif_fps {
    () => {
        "15"
    };
}
macro_rules! gif_max_w {
    () => {
        "640"
    };
}

/// Escaped comma is required: ffmpeg's filter parser splits unescaped commas
/// into separate filters. No shell is involved, so the backslash is literal.
const GIF_FILTER: &str = concat!(
    "fps=",
    gif_fps!(),
    ",scale=w=min(",
    gif_max_w!(),
    r"\,iw):h=-2:flags=lanczos,split[a][b];",
    "[a]palettegen=stats_mode=diff[p];",
    "[b][p]paletteuse=dither=bayer:bayer_scale=3"
);

/// `-sn` disables default subtitle-stream selection. Without it, `mkv → mp4`
/// — the flagship pair in this table — fails outright on a source carrying a
/// bitmap subtitle track (PGS), since ffmpeg tries to encode it to the MP4
/// default `mov_text` and cannot. With no explicit `-map`, any audio track
/// beyond the first is also silently dropped, hence the warning.
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
            Arg::Lit("-sn"),
            Arg::Lit("-movflags"),
            Arg::Lit("+faststart"),
            Arg::Lit("-y"),
            Arg::Output,
        ]
    )],
    warnings: &["Subtitle tracks and any audio tracks beyond the first are dropped."],
};

/// `-row-mt 1` enables libvpx's row-based multithreading and `-threads 0`
/// lets it use every core. libvpx does not enable row multithreading by
/// default and otherwise encodes single-threaded, so a 1080p VP9 encode runs
/// at low single-digit fps without these.
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
            Arg::Lit("-row-mt"),
            Arg::Lit("1"),
            Arg::Lit("-threads"),
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

/// Stream-copy remux to MP4. Selected dynamically by `plan::build` when the
/// source codecs are already legal in the target container. See Task 7.
/// `-sn` matters here exactly as it does on `VIDEO_TO_MP4`: `-c copy` still
/// triggers default stream selection, which happily copies a PGS subtitle
/// track that the mp4 muxer cannot carry, and ffmpeg then dies with "Could
/// not find tag for codec".
pub const REMUX_MP4: Recipe = Recipe {
    steps: &[step!(
        Backend::Ffmpeg,
        [
            Arg::Lit("-i"),
            Arg::Input,
            Arg::Lit("-c"),
            Arg::Lit("copy"),
            Arg::Lit("-sn"),
            Arg::Lit("-movflags"),
            Arg::Lit("+faststart"),
            Arg::Lit("-y"),
            Arg::Output,
        ]
    )],
    warnings: &[],
};

/// Stream-copy remux to WebM. `-movflags` is a private AVOption of the
/// mov/mp4 muxer: passed to the webm muxer it trips `assert_avoptions` and
/// ffmpeg exits 1 with "Option movflags not found". This recipe exists
/// specifically so a WebM target never gets that flag; it does not share
/// argv with `REMUX_MP4`.
pub const REMUX_WEBM: Recipe = Recipe {
    steps: &[step!(
        Backend::Ffmpeg,
        [
            Arg::Lit("-i"),
            Arg::Input,
            Arg::Lit("-c"),
            Arg::Lit("copy"),
            Arg::Lit("-sn"),
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
    warnings: &[
        "The whole filtered stream is buffered in memory for palette generation, \
         so very long inputs are slow and memory-hungry rather than being \
         silently truncated.",
    ],
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
    warnings: &[
        "A looping GIF becomes a single play in MP4; there is no container-level \
         loop flag to carry it over.",
    ],
};

/// `drop_video` is for a video source, or for a target that cannot carry a
/// video stream at all: `-vn` strips it outright. `keep_art` is for an audio
/// source converting to a target that *can* carry one: plain `ffmpeg -i
/// in.flac out.mp3` keeps an attached-picture cover art stream, and `-vn`
/// was throwing it away on every audio-to-audio pair — the single most
/// common audio conversion — making our default strictly worse than the
/// naive command. `-map 0:v?` is optional so a source with no picture stream
/// doesn't fail.
macro_rules! audio_recipe {
    (drop_video, [$($codec:expr),* $(,)?]) => {
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
    (keep_art, [$($codec:expr),* $(,)?]) => {
        Recipe {
            steps: &[step!(
                Backend::Ffmpeg,
                [
                    Arg::Lit("-i"), Arg::Input,
                    Arg::Lit("-map"), Arg::Lit("0:a"),
                    Arg::Lit("-map"), Arg::Lit("0:v?"),
                    Arg::Lit("-c:v"), Arg::Lit("copy"),
                    $($codec,)*
                    Arg::Lit("-y"), Arg::Output,
                ]
            )],
            warnings: &[],
        }
    };
}

const TO_MP3: Recipe = audio_recipe!(
    drop_video,
    [
        Arg::Lit("-c:a"),
        Arg::Lit("libmp3lame"),
        Arg::Lit("-q:a"),
        Arg::Lit("2")
    ]
);
const TO_M4A: Recipe = audio_recipe!(
    drop_video,
    [
        Arg::Lit("-c:a"),
        Arg::Lit("aac"),
        Arg::Lit("-b:a"),
        Arg::Lit("192k")
    ]
);
const TO_FLAC: Recipe = audio_recipe!(drop_video, [Arg::Lit("-c:a"), Arg::Lit("flac")]);

/// WAV cannot carry an attached-picture stream at all, so unlike the other
/// audio targets this always strips it with `-vn` — leaving the stream in
/// fails rather than degrading gracefully, in either direction. `pcm_s16le`
/// is also 16-bit only, which loses precision from a 24-bit source; that is
/// the one thing this recipe cannot route around, hence the warning.
const TO_WAV: Recipe = Recipe {
    steps: &[step!(
        Backend::Ffmpeg,
        [
            Arg::Lit("-i"),
            Arg::Input,
            Arg::Lit("-vn"),
            Arg::Lit("-c:a"),
            Arg::Lit("pcm_s16le"),
            Arg::Lit("-y"),
            Arg::Output,
        ]
    )],
    warnings: &["A 24-bit source is downconverted to 16-bit PCM by pcm_s16le."],
};

const TO_MP3_KEEP_ART: Recipe = audio_recipe!(
    keep_art,
    [
        Arg::Lit("-c:a"),
        Arg::Lit("libmp3lame"),
        Arg::Lit("-q:a"),
        Arg::Lit("2")
    ]
);
const TO_M4A_KEEP_ART: Recipe = audio_recipe!(
    keep_art,
    [
        Arg::Lit("-c:a"),
        Arg::Lit("aac"),
        Arg::Lit("-b:a"),
        Arg::Lit("192k")
    ]
);
const TO_FLAC_KEEP_ART: Recipe = audio_recipe!(keep_art, [Arg::Lit("-c:a"), Arg::Lit("flac")]);

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

/// Audio targets reached from a *video* source: the video stream (and any
/// embedded art on the video container) is always dropped with `-vn`.
const AUDIO_TARGETS: &[(Format, Recipe)] = &[
    (Format::Mp3, TO_MP3),
    (Format::M4a, TO_M4A),
    (Format::Wav, TO_WAV),
    (Format::Flac, TO_FLAC),
];

/// Same target formats, but for an *audio* source: mp3/m4a/flac preserve an
/// attached-picture cover art stream instead of stripping it with `-vn`. WAV
/// still can't carry one, so it reuses `TO_WAV` unchanged in both tables.
const AUDIO_TARGETS_KEEP_ART: &[(Format, Recipe)] = &[
    (Format::Mp3, TO_MP3_KEEP_ART),
    (Format::M4a, TO_M4A_KEEP_ART),
    (Format::Wav, TO_WAV),
    (Format::Flac, TO_FLAC_KEEP_ART),
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

    // Audio sources transcode between audio targets, preserving an
    // attached-picture cover art stream where the target can carry one.
    for &from in &[Format::Mp3, Format::M4a, Format::Wav, Format::Flac] {
        for &(to, recipe) in AUDIO_TARGETS_KEEP_ART {
            if from != to {
                t.insert((from, to), recipe);
            }
        }
    }

    t.insert((Format::Gif, Format::Mp4), GIF_TO_MP4);
}

// --- Document family ---------------------------------------------------------

/// LibreOffice writes into a directory and names the file itself. The second
/// arm adds `--infilter` for a source format whose default reader isn't the
/// one we need (see `PDF_TO_DOCX`).
macro_rules! soffice_step {
    ($filter:expr) => {
        Step {
            backend: Backend::Soffice,
            args: &[
                Arg::Lit("--headless"),
                Arg::Lit("--norestore"),
                Arg::Lit("--convert-to"),
                Arg::Lit($filter),
                Arg::Lit("--outdir"),
                Arg::OutDir,
                Arg::Input,
            ],
            output: OutputMode::OutDir,
            intermediate_ext: None,
        }
    };
    ($filter:expr, infilter: $infilter:expr) => {
        Step {
            backend: Backend::Soffice,
            args: &[
                Arg::Lit("--headless"),
                Arg::Lit("--norestore"),
                Arg::Lit($infilter),
                Arg::Lit("--convert-to"),
                Arg::Lit($filter),
                Arg::Lit("--outdir"),
                Arg::OutDir,
                Arg::Input,
            ],
            output: OutputMode::OutDir,
            intermediate_ext: None,
        }
    };
}

const OFFICE_TO_PDF: Recipe = Recipe {
    steps: &[soffice_step!("pdf")],
    warnings: &[],
};

/// The bare `docx` filter yields a LibreOffice Draw document; the explicit
/// export filter name is required to get real Word output. That alone still
/// isn't enough, though: LibreOffice's default PDF import is
/// `draw_pdf_import`, so without `--infilter=writer_pdf_import` forcing the
/// Writer importer, the source is read as a Draw model before the export
/// filter ever sees it. Flagged for empirical verification against a real
/// LibreOffice in Task 15.
const PDF_TO_DOCX: Recipe = Recipe {
    steps: &[soffice_step!(
        "docx:MS Word 2007 XML",
        infilter: "--infilter=writer_pdf_import"
    )],
    warnings: &[
        "PDF stores positioned glyphs, not paragraphs, so the result is a set of \
         text boxes rather than a flowing document. Expect to reflow it by hand.",
    ],
};

/// Mirrors `soffice_step!`: the plain pandoc invocation shared by
/// `MD_TO_DOCX` and `MD_TO_HTML`, plus a variant that tags an intermediate
/// file's extension for a multi-step recipe like `MD_TO_PDF`.
macro_rules! pandoc_step {
    () => {
        step!(
            Backend::Pandoc,
            [
                Arg::Input,
                Arg::Lit("--standalone"),
                Arg::Lit("-o"),
                Arg::Output
            ]
        )
    };
    (intermediate_ext: $ext:expr) => {
        Step {
            backend: Backend::Pandoc,
            args: &[
                Arg::Input,
                Arg::Lit("--standalone"),
                Arg::Lit("-o"),
                Arg::Output,
            ],
            output: OutputMode::Path,
            intermediate_ext: Some($ext),
        }
    };
}

const MD_TO_DOCX: Recipe = Recipe {
    steps: &[pandoc_step!()],
    warnings: &[],
};

const MD_TO_HTML: Recipe = Recipe {
    steps: &[pandoc_step!()],
    warnings: &[],
};

/// Two hardcoded steps, not a routing graph: pandoc emits .docx, LibreOffice
/// renders it. This avoids a ~400MB LaTeX toolchain entirely.
const MD_TO_PDF: Recipe = Recipe {
    steps: &[pandoc_step!(intermediate_ext: "docx"), soffice_step!("pdf")],
    warnings: &[],
};

const OFFICE_SOURCES: &[Format] = &[
    Format::Docx,
    Format::Xlsx,
    Format::Pptx,
    Format::Odt,
    Format::Ods,
];

fn insert_document_family(t: &mut Table) {
    for &from in OFFICE_SOURCES {
        t.insert((from, Format::Pdf), OFFICE_TO_PDF);
    }
    t.insert((Format::Pdf, Format::Docx), PDF_TO_DOCX);
    t.insert((Format::Md, Format::Docx), MD_TO_DOCX);
    t.insert((Format::Md, Format::Html), MD_TO_HTML);
    t.insert((Format::Md, Format::Pdf), MD_TO_PDF);
}

static TABLE: LazyLock<Table> = LazyLock::new(|| {
    let mut t = Table::new();
    insert_image_family(&mut t);
    insert_media_family(&mut t);
    insert_document_family(&mut t);
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

/// True when the pair might be satisfiable by a stream copy, so the caller
/// should run ffprobe before building a plan.
pub fn needs_probe(from: Format, to: Format) -> bool {
    let container_change = matches!(to, Format::Mp4 | Format::Webm)
        && matches!(
            from,
            Format::Mp4 | Format::Mov | Format::Mkv | Format::Webm | Format::Avi
        );
    container_change && from != to
}

/// Whether the probed codecs are legal in the target container.
pub fn can_remux(to: Format, probe: &crate::MediaProbe) -> bool {
    let (video_ok, audio_ok) = match to {
        Format::Mp4 => (MP4_COMPATIBLE_VIDEO, MP4_COMPATIBLE_AUDIO),
        Format::Webm => (WEBM_COMPATIBLE_VIDEO, WEBM_COMPATIBLE_AUDIO),
        _ => return false,
    };
    let v = probe
        .video_codec
        .as_deref()
        .is_some_and(|c| video_ok.contains(&c));
    // A file with no audio stream remuxes fine.
    let a = probe
        .audio_codec
        .as_deref()
        .is_none_or(|c| audio_ok.contains(&c));
    v && a
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
    fn svg_to_png_is_lossless_with_transparent_background() {
        let r = lookup(Format::Svg, Format::Png).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.svg")], Path::new("out.png"));
        assert_eq!(
            argv,
            vec![
                "-density",
                "384",
                "-background",
                "none",
                "in.svg",
                "out.png"
            ]
        );
    }

    #[test]
    fn svg_to_jpg_flattens_transparency_onto_white() {
        let r = lookup(Format::Svg, Format::Jpg).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.svg")], Path::new("out.jpg"));
        assert_eq!(
            argv,
            vec![
                "-density",
                "384",
                "-background",
                "white",
                "in.svg",
                "-alpha",
                "remove",
                "-alpha",
                "off",
                "-flatten",
                "-quality",
                "92",
                "out.jpg",
            ]
        );
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].contains("white"), "{:?}", r.warnings);
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
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
    }

    #[test]
    fn video_transcode_uses_the_spec_quality_anchors() {
        let r = lookup(Format::Mkv, Format::Mp4).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.mkv")], Path::new("out.mp4"));
        assert!(argv.windows(2).any(|w| w == ["-crf", "20"]), "{argv:?}");
        assert!(argv.windows(2).any(|w| w == ["-b:a", "160k"]), "{argv:?}");
        assert!(argv.contains(&"+faststart".to_string()), "{argv:?}");
        assert!(argv.contains(&"-sn".to_string()), "{argv:?}");
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
    }

    #[test]
    fn video_to_webm_enables_row_multithreading() {
        let r = lookup(Format::Mkv, Format::Webm).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.mkv")], Path::new("out.webm"));
        assert!(argv.windows(2).any(|w| w == ["-row-mt", "1"]), "{argv:?}");
        assert!(argv.windows(2).any(|w| w == ["-threads", "0"]), "{argv:?}");
    }

    #[test]
    fn audio_extraction_drops_the_video_stream() {
        let r = lookup(Format::Mp4, Format::Mp3).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.mp4")], Path::new("out.mp3"));
        assert_eq!(
            argv,
            vec![
                "-i",
                "in.mp4",
                "-vn",
                "-c:a",
                "libmp3lame",
                "-q:a",
                "2",
                "-y",
                "out.mp3"
            ]
        );
    }

    #[test]
    fn audio_to_audio_preserves_embedded_cover_art() {
        let r = lookup(Format::Flac, Format::Mp3).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.flac")], Path::new("out.mp3"));
        assert_eq!(
            argv,
            vec![
                "-i",
                "in.flac",
                "-map",
                "0:a",
                "-map",
                "0:v?",
                "-c:v",
                "copy",
                "-c:a",
                "libmp3lame",
                "-q:a",
                "2",
                "-y",
                "out.mp3",
            ]
        );
    }

    #[test]
    fn audio_to_wav_still_drops_video_and_warns_about_bit_depth() {
        let r = lookup(Format::Flac, Format::Wav).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.flac")], Path::new("out.wav"));
        assert!(argv.contains(&"-vn".to_string()), "{argv:?}");
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].contains("16-bit"), "{:?}", r.warnings);
    }

    #[test]
    fn gif_to_mp4_forces_even_dimensions() {
        let r = lookup(Format::Gif, Format::Mp4).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.gif")], Path::new("out.mp4"));
        let joined = argv.join(" ");
        assert!(joined.contains("trunc(iw/2)*2"), "{joined}");
        assert!(joined.contains("yuv420p"), "{joined}");
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].contains("loop"), "{:?}", r.warnings);
    }

    #[test]
    fn remux_mp4_keeps_faststart_and_drops_subtitle_selection() {
        let argv = REMUX_MP4.steps[0].render(&[Path::new("in.mkv")], Path::new("out.mp4"));
        assert_eq!(
            argv,
            vec![
                "-i",
                "in.mkv",
                "-c",
                "copy",
                "-sn",
                "-movflags",
                "+faststart",
                "-y",
                "out.mp4"
            ]
        );
    }

    #[test]
    fn remux_webm_omits_the_mp4_only_movflags_option() {
        let argv = REMUX_WEBM.steps[0].render(&[Path::new("in.mkv")], Path::new("out.webm"));
        assert_eq!(
            argv,
            vec!["-i", "in.mkv", "-c", "copy", "-sn", "-y", "out.webm"]
        );
    }

    #[test]
    fn office_to_pdf_uses_outdir_mode() {
        let r = lookup(Format::Docx, Format::Pdf).unwrap();
        assert_eq!(r.steps[0].output, OutputMode::OutDir);
        let argv = r.steps[0].render(&[Path::new("a/in.docx")], Path::new("b/out.pdf"));
        assert!(argv.contains(&"--headless".to_string()), "{argv:?}");
        assert!(
            argv.windows(2).any(|w| w == ["--convert-to", "pdf"]),
            "{argv:?}"
        );
        assert!(argv.windows(2).any(|w| w == ["--outdir", "b"]), "{argv:?}");
    }

    #[test]
    fn pdf_to_docx_names_the_word_filter_and_warns_about_fidelity() {
        let r = lookup(Format::Pdf, Format::Docx).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.pdf")], Path::new("out.docx"));
        assert!(
            argv.contains(&"docx:MS Word 2007 XML".to_string()),
            "the bare `docx` filter produces a Draw document: {argv:?}"
        );
        assert!(
            argv.contains(&"--infilter=writer_pdf_import".to_string()),
            "default PDF import is draw_pdf_import, not Writer: {argv:?}"
        );
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].contains("text boxes"), "{:?}", r.warnings);
    }

    #[test]
    fn md_to_pdf_is_two_steps_via_docx_and_never_touches_latex() {
        let r = lookup(Format::Md, Format::Pdf).unwrap();
        assert_eq!(r.steps.len(), 2);
        assert_eq!(r.steps[0].backend, Backend::Pandoc);
        assert_eq!(r.steps[0].intermediate_ext, Some("docx"));
        assert_eq!(r.steps[1].backend, Backend::Soffice);
        assert_eq!(r.steps[1].output, OutputMode::OutDir);
    }

    #[test]
    fn md_to_html_is_standalone() {
        let r = lookup(Format::Md, Format::Html).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.md")], Path::new("out.html"));
        assert!(argv.contains(&"--standalone".to_string()), "{argv:?}");
    }

    #[test]
    fn no_recipe_targets_heic_or_heif() {
        for (from, to) in all_pairs() {
            assert!(
                !matches!(to, Format::Heic | Format::Heif),
                "{from:?}->{to:?} targets a read-only, encode-unsupported format"
            );
        }
    }

    #[test]
    fn no_recipe_sets_user_installation() {
        for (from, to) in all_pairs() {
            let r = lookup(from, to).unwrap();
            for step in r.steps {
                for arg in step.args {
                    if let Arg::Lit(s) = arg {
                        assert!(
                            !s.contains("UserInstallation"),
                            "{from:?}->{to:?} recipe sets -env:UserInstallation; \
                             exec injects this for every Soffice call already"
                        );
                    }
                }
            }
        }
    }
}
