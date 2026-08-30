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

/// `mov` is the same muxer family as `mp4` (both are handled by ffmpeg's
/// mov/mp4/tgp/psp/tg2/ipod/ismv/f4v muxer, confirmed via `ffmpeg -h
/// muxer=mov`), so the exact same argv that produces a compliant `.mp4`
/// also produces a compliant `.mov` -- the only thing that differs is the
/// extension on `Arg::Output`, which this recipe never spells itself. So
/// this is a literal alias, not independent argv that happens to match:
/// diverging from `VIDEO_TO_MP4` here without a *reason* the mov muxer
/// actually enforces would just be a copy that silently drifts out of sync.
///
/// Unlike `VIDEO_TO_MKV` (see its own docs), aliasing `-sn` along with
/// everything else here is *not* the same bug: verified live against a real
/// ffmpeg (9.0) by transcoding an `h264 + aac + aac + mov_text` mp4 source
/// with no `-map`/`-c:s`/`-sn` at all, to both a bare `-f mp4` and a bare
/// `-f mov` output -- the `Stream mapping:` ffmpeg prints was byte-identical
/// between the two (`Stream #0:0 -> #0:0`, `Stream #0:1 -> #0:1`, nothing
/// else), and both dropped the second audio track and the subtitle track
/// the same way. Mapping a subtitle explicitly, the default codec ffmpeg
/// picks for it is `mov_text` for both `-f mp4` and `-f mov` outputs too.
/// mp4 and mov aren't just "the same muxer family" for the codecs they
/// accept (already established above) -- they run the exact same default
/// stream-selection and default-codec code, because it *is* the exact same
/// muxer. mkv's problem was that its genuinely broader capabilities made
/// mp4's constraints foreign to it; mov has no broader capabilities than
/// mp4 to begin with, so there is nothing here for mp4's constraints to be
/// foreign to.
const VIDEO_TO_MOV: Recipe = VIDEO_TO_MP4;

/// Shared argv for both mkv-transcode recipes below: `-map 0` carries every
/// stream through the re-encode (not just the first video/audio track --
/// the bug this pair of recipes replaces let mkv inherit mp4's "one audio
/// track, no subtitles" default selection, which mkv itself never needed).
/// Video and audio use the same quality anchors as `VIDEO_TO_MP4`, applied
/// to *every* video/audio stream `-map 0` selects, not just the first of
/// each. `-movflags` is never added, for the same reason `REMUX_MKV` never
/// adds it: it is a private AVOption of the mov/mp4 muxer family that makes
/// ffmpeg exit 1 against any other muxer -- see `REMUX_WEBM`'s docs, which
/// hit the identical error against the webm muxer. The one thing that
/// varies between the two consts below is `$sub_codec`, spliced into
/// `-c:s`; see each const's own docs for which sources get which.
macro_rules! video_to_mkv_recipe {
    ($sub_codec:expr) => {
        step!(
            Backend::Ffmpeg,
            [
                Arg::Lit("-i"),
                Arg::Input,
                Arg::Lit("-map"),
                Arg::Lit("0"),
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
                Arg::Lit("-c:s"),
                Arg::Lit($sub_codec),
                Arg::Lit("-y"),
                Arg::Output,
            ]
        )
    };
}

/// Transcode fallback for `* -> mkv` when the source codecs don't fit even
/// matroska's own broad compatibility table (see `MKV_COMPATIBLE_VIDEO`/
/// `MKV_COMPATIBLE_AUDIO`) or no probe was available to check -- reachable
/// whenever ffprobe is unavailable or the probe fails, so it is a static
/// choice, never one made from a live `MediaProbe`. This is the copy-subs
/// variant: `-c:s copy` for whatever subtitle codec the source already
/// carries. That is safe for every source this recipe is actually
/// registered for (`video_to_mkv_recipe_for` below sends `mp4`/`mov` -- the
/// one case where it would not be safe -- to `VIDEO_TO_MKV_SRT_SUBS`
/// instead), and a no-op when there is no subtitle stream at all.
///
/// This used to carry `-sn` and a matching "subtitles and extra audio
/// tracks are dropped" warning, copied verbatim from `VIDEO_TO_MP4`. That
/// was a real bug: `-sn` disables subtitle selection because *mp4* cannot
/// hold most subtitle codecs, and the old argv's lack of `-map 0` meant
/// ffmpeg's automatic selection also silently dropped every audio track
/// past the first -- both are mp4-specific limitations mkv does not share
/// (see `REMUX_MKV`'s own docs for why mkv is worth adding as a target at
/// all). `-map 0` plus a per-stream-type codec for every type now carries
/// everything through, so nothing is lost -- just re-encoded (video, audio)
/// or copied untouched (subtitles) -- and there is no warning here for the
/// same reason `REMUX_MKV` and `VIDEO_TO_WEBM` (also a full transcode) have
/// none: a warning describing a loss that no longer happens is worse than
/// no warning at all.
const VIDEO_TO_MKV: Recipe = Recipe {
    steps: &[video_to_mkv_recipe!("copy")],
    warnings: &[],
};

/// `VIDEO_TO_MKV`'s sibling for the two sources whose subtitle codec, if
/// they carry one at all, is guaranteed to be `mov_text`: mp4 and mov. That
/// is a structural fact about those two containers, not something that
/// varies per file -- `REMUX_MKV_SRT_SUBS`'s docs record the live-verified
/// error a plain `-c:s copy` of `mov_text` hits against matroska ("Subtitle
/// codec mov_text (94213) is not supported"), and matroska still has no
/// codec ID for it here. Because the fact is about the *container*, not a
/// particular file, `video_to_mkv_recipe_for` below can select this recipe
/// from `from` alone, with no probe involved -- which matters specifically
/// because this recipe only ever runs on the no-probe-available path (see
/// `VIDEO_TO_MKV`'s own docs): there is no `MediaProbe` here to consult even
/// if the choice needed one.
///
/// Unlike `REMUX_MKV_SRT_SUBS`, video and audio are genuinely re-encoded on
/// this path, not stream-copied, so the warning says that plainly instead
/// of reusing wording that would no longer be true here.
const VIDEO_TO_MKV_SRT_SUBS: Recipe = Recipe {
    steps: &[video_to_mkv_recipe!("srt")],
    warnings: &[
        "Subtitle tracks stored as MP4/MOV's mov_text are re-encoded to SRT text; \
         matroska has no codec for mov_text itself. Video and audio are also \
         re-encoded (libx264/AAC) on this transcode path, not stream-copied.",
    ],
};

/// Picks between `VIDEO_TO_MKV` and `VIDEO_TO_MKV_SRT_SUBS` for a `* ->
/// mkv` transcode, purely from the source format -- see
/// `VIDEO_TO_MKV_SRT_SUBS`'s docs for why `from` alone is enough. The only
/// reader is `insert_media_family`, at table-construction time; this is
/// deliberately not shaped like `mkv_remux_for(probe: &MediaProbe)` because
/// there is no probe on this path for it to take.
fn video_to_mkv_recipe_for(from: Format) -> Recipe {
    match from {
        Format::Mp4 | Format::Mov => VIDEO_TO_MKV_SRT_SUBS,
        _ => VIDEO_TO_MKV,
    }
}

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

/// Stream-copy remux to MOV. `mov` shares its muxer implementation with
/// `mp4` (verified via `ffmpeg -h muxer=mov`: "mov/mp4/tgp/psp/tg2/ipod/
/// ismv/f4v muxer AVOptions"), so `-movflags +faststart` is legal here the
/// same way it is on `REMUX_MP4` -- this is argv-identical to it, not an
/// alias, because a future divergence (say, a mov-specific option) should
/// be easy to add without disturbing `REMUX_MP4`.
pub const REMUX_MOV: Recipe = Recipe {
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

/// Stream-copy remux to MKV -- the one recipe in this table that keeps
/// *everything*. Every other remux/transcode target in this file carries
/// `-sn` (or, for the lossy `* -> mp4` family, drops every audio track past
/// the first too) because its container genuinely cannot hold what a
/// source might carry: MP4 rejects a PGS bitmap subtitle outright, and so
/// does WebM's default subtitle codec. Matroska was designed to hold
/// anything, so none of that applies here: `-map 0` selects every stream
/// on the input -- every audio track, every subtitle track (bitmap or
/// text), chapters -- and `-c copy` copies each one as-is. No warning,
/// because unlike the rest of this table, nothing here is actually lost.
/// This is the reason mkv is worth adding as a target at all: it is the
/// lossless escape hatch for content (multiple audio tracks, PGS/bitmap
/// subtitles) that would otherwise have to be degraded to fit mp4.
///
/// One real exception, handled by a sibling recipe rather than here: `mkv`
/// has no codec ID for `mov_text`, mp4/mov's own (and *only*) subtitle
/// codec, so a plain `-c copy` of a `mov_text` stream into matroska fails
/// outright -- verified live: `ffmpeg -i <mov_text source> -c copy -f
/// matroska` exits 1 with `"Subtitle codec mov_text (94213) is not
/// supported."`. `mkv_remux_for` is what picks between this recipe and that
/// one; callers should go through it rather than reaching for `REMUX_MKV`
/// directly.
pub const REMUX_MKV: Recipe = Recipe {
    steps: &[step!(
        Backend::Ffmpeg,
        [
            Arg::Lit("-i"),
            Arg::Input,
            Arg::Lit("-map"),
            Arg::Lit("0"),
            Arg::Lit("-c"),
            Arg::Lit("copy"),
            Arg::Lit("-y"),
            Arg::Output,
        ]
    )],
    warnings: &[],
};

/// `REMUX_MKV`'s sibling for the one subtitle codec matroska rejects:
/// `mov_text`. Text-to-text, not bitmap-to-text, so re-encoding it to SRT
/// (also plain text) is a safe, essentially lossless substitution -- unlike
/// a hypothetical bitmap source, where ffmpeg has no encoder to convert
/// bitmap pixels into text at all. Video and audio are still stream-copied
/// (`-c:v copy -c:a copy`), so this keeps the actually-expensive part of
/// "remux" (no re-encoding the picture or the audio) while sidestepping the
/// one codec matroska won't take. See `REMUX_MKV`'s own docs for how the
/// live "not supported" error was verified, and `mkv_remux_for` for the
/// selection logic between the two.
pub const REMUX_MKV_SRT_SUBS: Recipe = Recipe {
    steps: &[step!(
        Backend::Ffmpeg,
        [
            Arg::Lit("-i"),
            Arg::Input,
            Arg::Lit("-map"),
            Arg::Lit("0"),
            Arg::Lit("-c:v"),
            Arg::Lit("copy"),
            Arg::Lit("-c:a"),
            Arg::Lit("copy"),
            Arg::Lit("-c:s"),
            Arg::Lit("srt"),
            Arg::Lit("-y"),
            Arg::Output,
        ]
    )],
    warnings: &[
        "Subtitle tracks stored as MP4/MOV's mov_text are re-encoded to SRT text; \
         matroska has no codec for mov_text itself. Video and audio are still \
         stream-copied untouched.",
    ],
};

/// Picks between `REMUX_MKV` and `REMUX_MKV_SRT_SUBS`: the latter only when
/// the probe found a `mov_text` subtitle stream, since that's the one
/// codec `-map 0 -c copy` cannot carry into matroska. Every other case --
/// no subtitle stream at all, or one already in a matroska-native codec
/// (srt, ass, PGS, ...) -- gets the plain stream copy. The only reader is
/// `plan::select`'s `Format::Mkv` arm; callers should never reach for
/// `REMUX_MKV` directly once a probe is in hand.
pub fn mkv_remux_for(probe: &crate::MediaProbe) -> Recipe {
    if probe.subtitle_codec.as_deref() == Some("mov_text") {
        REMUX_MKV_SRT_SUBS
    } else {
        REMUX_MKV
    }
}

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

/// Verified against a real `ffmpeg` (9.0) by encoding a one-second clip in
/// each candidate codec and stream-copying it into a bare `-f mov` output,
/// rather than guessed from the mp4 set: `mov` and `mp4` share a muxer
/// implementation, but they do not share an identical codec allowlist.
/// Concretely, mov's encoder rejected every av1/vp8/vp9 clip with muxer
/// errors naming the restriction outright -- `"av1 only supported in MP4
/// and AVIF"`, `"VP8 muxing is currently not supported."`, `"vp9 only
/// supported in MP4."` -- so unlike `MP4_COMPATIBLE_VIDEO`, `av1` is
/// deliberately absent here. `prores` (`prores_ks`, reported by ffprobe as
/// `codec_name: "prores"`) muxed cleanly, and is mov's own genuine
/// specialty: QuickTime-family editing workflows are the reason ProRes
/// exists at all.
pub const MOV_COMPATIBLE_VIDEO: &[&str] = &["h264", "hevc", "mpeg4", "prores"];
/// Same verification method as `MOV_COMPATIBLE_VIDEO`. `aac`/`mp3`/`ac3`/
/// `alac` all muxed cleanly, matching `MP4_COMPATIBLE_AUDIO` exactly; `pcm`
/// (`pcm_s16le`) additionally muxed cleanly into mov where it does *not*
/// appear in the mp4 set -- uncompressed PCM inside a QuickTime container
/// is a long-standing, still-common combination (audio captured straight
/// off a camera, or produced by pro-audio tooling) that mp4 in practice
/// rarely carries. `flac`, `opus`, and `wavpack` were also tried and mov's
/// muxer rejected all three outright ("Could not write header (incorrect
/// codec parameters?)").
pub const MOV_COMPATIBLE_AUDIO: &[&str] = &["aac", "mp3", "ac3", "alac", "pcm_s16le"];

/// Verified the same way as the mov sets above: every codec listed here was
/// actually encoded and stream-copied into a bare `-f matroska` output and
/// confirmed to mux without error. Matroska's design goal is to hold
/// essentially any codec, and the live test bore that out -- every video
/// candidate tried (h264, hevc, mpeg4, av1, vp8, vp9, prores, mjpeg,
/// huffyuv, ffv1, rawvideo, wmv2) muxed cleanly, including every codec this
/// module rejected for mov or restricts for mp4/webm. This list is the
/// union of what the five video *sources* in `VIDEO` below can plausibly
/// carry (mp4's set, mov's set, webm's set, plus legacy codecs `avi` and
/// `mkv` sources commonly carry) rather than every codec ffmpeg happens to
/// support, since an unbounded list would just as accurately be described
/// as "matroska accepts anything" without actually helping `can_remux`
/// decide anything.
pub const MKV_COMPATIBLE_VIDEO: &[&str] = &[
    "h264", "hevc", "mpeg4", "av1", "vp8", "vp9", "prores", "mjpeg", "huffyuv", "ffv1", "rawvideo",
    "wmv2",
];
/// Same verification method and same "union of what the source containers
/// plausibly carry" reasoning as `MKV_COMPATIBLE_VIDEO`. Every candidate
/// tried -- including `dts`, `truehd`, and `wavpack`, three codecs mov
/// and/or mp4/webm reject outright -- stream-copied into `-f matroska`
/// without error.
pub const MKV_COMPATIBLE_AUDIO: &[&str] = &[
    "aac",
    "mp3",
    "ac3",
    "eac3",
    "alac",
    "flac",
    "opus",
    "vorbis",
    "pcm_s16le",
    "dts",
    "truehd",
    "wavpack",
];

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

/// `avi` is deliberately never inserted as a *target* below, only ever as a
/// source: it's a legacy (OpenDML/RIFF) container whose codec support is
/// stuck where the format was frozen. It has no standard way to carry
/// modern codecs at all -- H.264/HEVC-in-avi only exists via undocumented,
/// inconsistently-supported FourCC hacks, and it cannot express the
/// variable frame rates, more-than-two-channel audio, or per-stream
/// metadata modern sources routinely carry. Writing a *new* `.avi` today
/// would therefore be a downgrade dressed up as a conversion, not a genuine
/// alternative to `mov`/`mkv`/`webm`/`mp4` the way those four are genuine
/// alternatives to each other. `avi` stays readable as a source (its
/// existing codecs decode fine) and simply never appears on the left of a
/// `Format::Avi` insertion below.
fn insert_media_family(t: &mut Table) {
    for &from in VIDEO {
        if from != Format::Mp4 {
            t.insert((from, Format::Mp4), VIDEO_TO_MP4);
        }
        if from != Format::Mov {
            t.insert((from, Format::Mov), VIDEO_TO_MOV);
        }
        if from != Format::Mkv {
            t.insert((from, Format::Mkv), video_to_mkv_recipe_for(from));
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

/// Fallback for `docx`/`odt` → `pdf` when LibreOffice isn't installed:
/// pandoc parses the document and hands the result to a managed Typst as
/// its `--pdf-engine`, verified end to end against a real `.docx` fixture
/// (`pandoc sample.docx --pdf-engine <typst> -o out.pdf` → a valid 28 KB
/// PDF). `--pdf-engine <path>` works as two separate argv tokens — pandoc
/// does not require the `=`-joined form — which is what lets the path sit
/// in its own `Arg::BackendPath` slot rather than needing string
/// concatenation baked into the `Arg` itself.
///
/// pandoc can read `.docx` and `.odt` but not `.xlsx`/`.pptx` (no spreadsheet
/// or slide-deck reader), so this recipe — and therefore this fallback —
/// only ever gets registered for those two source formats; `xlsx`/`pptx` →
/// `pdf` stay LibreOffice-only, see `FALLBACK_TABLE` below.
///
/// Because pandoc re-renders parsed content rather than reproducing Word's
/// own layout engine, exact positioning and some styling do not survive —
/// the warning says so and points at installing LibreOffice for higher
/// fidelity, the same way `PDF_TO_DOCX`'s warning does for its own
/// fidelity caveat.
const PANDOC_TYPST_TO_PDF: Recipe = Recipe {
    steps: &[step!(
        Backend::Pandoc,
        [
            Arg::Input,
            Arg::Lit("--pdf-engine"),
            Arg::BackendPath(Backend::Typst),
            Arg::Lit("-o"),
            Arg::Output,
        ]
    )],
    warnings: &[
        "The document is re-rendered from parsed content rather than laid out by \
         Word's own model, so exact positioning and some styling will not survive. \
         Install LibreOffice for higher fidelity.",
    ],
};

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

/// Fallback recipes usable only when the canonical backend for a pair is
/// unavailable. Sparser than `TABLE` — an entry exists only where a genuine
/// second route exists, currently `docx`/`odt` → `pdf` — and every entry
/// here names a pair `TABLE` already covers too; `plan::select` is the only
/// reader, and only consults this when the caller supplies an
/// `AvailableBackends` hint saying soffice is absent. `all_pairs` is
/// unaffected: it walks `TABLE` alone, so this table can never change which
/// pairs convkit advertises as supported, only which recipe answers one
/// that's already listed.
static FALLBACK_TABLE: LazyLock<Table> = LazyLock::new(|| {
    let mut t = Table::new();
    t.insert((Format::Docx, Format::Pdf), PANDOC_TYPST_TO_PDF);
    t.insert((Format::Odt, Format::Pdf), PANDOC_TYPST_TO_PDF);
    t
});

/// The backends `plan::select` needs an availability answer for on a pair
/// where `lookup_fallback` might return something. Fixed today because
/// there is exactly one fallback decision in the registry; a second one
/// with a different backend set would need this to vary per pair.
pub const FALLBACK_BACKENDS: &[Backend] = &[Backend::Soffice, Backend::Pandoc, Backend::Typst];

pub fn lookup(from: Format, to: Format) -> Option<Recipe> {
    TABLE.get(&(from, to)).copied()
}

/// The alternate recipe for `from -> to`, if one exists. `None` for every
/// pair but `docx`/`odt` → `pdf`.
pub fn lookup_fallback(from: Format, to: Format) -> Option<Recipe> {
    FALLBACK_TABLE.get(&(from, to)).copied()
}

/// True when `from -> to` has a second recipe `plan::select` could fall
/// back to — the gate callers use to decide whether checking backend
/// availability is even worth doing before building a plan (mirrors
/// `needs_probe`'s role for the media-remux decision).
pub fn has_fallback(from: Format, to: Format) -> bool {
    FALLBACK_TABLE.contains_key(&(from, to))
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
    let container_change = matches!(to, Format::Mp4 | Format::Mov | Format::Mkv | Format::Webm)
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
        Format::Mov => (MOV_COMPATIBLE_VIDEO, MOV_COMPATIBLE_AUDIO),
        Format::Mkv => (MKV_COMPATIBLE_VIDEO, MKV_COMPATIBLE_AUDIO),
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

    // --- mov/mkv as conversion targets --------------------------------------

    #[test]
    fn remux_mov_keeps_faststart_like_remux_mp4() {
        let argv = REMUX_MOV.steps[0].render(&[Path::new("in.mp4")], Path::new("out.mov"));
        assert_eq!(
            argv,
            vec![
                "-i",
                "in.mp4",
                "-c",
                "copy",
                "-sn",
                "-movflags",
                "+faststart",
                "-y",
                "out.mov"
            ]
        );
    }

    #[test]
    fn remux_mkv_maps_every_stream_and_carries_no_warning() {
        let argv = REMUX_MKV.steps[0].render(&[Path::new("in.mp4")], Path::new("out.mkv"));
        assert_eq!(
            argv,
            vec!["-i", "in.mp4", "-map", "0", "-c", "copy", "-y", "out.mkv"]
        );
        assert!(
            !argv.contains(&"-movflags".to_string()),
            "mkv remux must not carry the mp4-only -movflags option: {argv:?}"
        );
        assert!(
            !argv.contains(&"-sn".to_string()),
            "mkv can hold any subtitle codec, so it must not disable subtitle \
             stream selection: {argv:?}"
        );
        assert_eq!(
            REMUX_MKV.warnings.len(),
            0,
            "remuxing to mkv loses nothing, so it must not carry a lossy-conversion warning: {:?}",
            REMUX_MKV.warnings
        );
    }

    /// Real, live-verified gap: matroska has no codec ID for `mov_text`
    /// (mp4/mov's own, and only, subtitle codec), so a plain `-c copy`
    /// dies with "Subtitle codec mov_text (94213) is not supported."
    /// `REMUX_MKV_SRT_SUBS` is the fix -- video/audio stay stream-copied,
    /// only the subtitle re-encodes to SRT.
    #[test]
    fn remux_mkv_srt_subs_still_stream_copies_video_and_audio() {
        let argv = REMUX_MKV_SRT_SUBS.steps[0].render(&[Path::new("in.mp4")], Path::new("out.mkv"));
        assert_eq!(
            argv,
            vec![
                "-i", "in.mp4", "-map", "0", "-c:v", "copy", "-c:a", "copy", "-c:s", "srt", "-y",
                "out.mkv"
            ]
        );
        assert_eq!(
            REMUX_MKV_SRT_SUBS.warnings.len(),
            1,
            "{:?}",
            REMUX_MKV_SRT_SUBS.warnings
        );
        assert!(
            REMUX_MKV_SRT_SUBS.warnings[0].contains("mov_text"),
            "{:?}",
            REMUX_MKV_SRT_SUBS.warnings
        );
    }

    #[test]
    fn mkv_remux_for_picks_the_srt_subs_variant_only_for_mov_text() {
        let with_mov_text = crate::MediaProbe {
            video_codec: Some("h264".into()),
            audio_codec: Some("aac".into()),
            subtitle_codec: Some("mov_text".into()),
        };
        assert_eq!(mkv_remux_for(&with_mov_text), REMUX_MKV_SRT_SUBS);

        for subtitle_codec in [None, Some("subrip".to_string()), Some("ass".to_string())] {
            let probe = crate::MediaProbe {
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                subtitle_codec,
            };
            assert_eq!(mkv_remux_for(&probe), REMUX_MKV, "{probe:?}");
        }
    }

    #[test]
    fn video_to_mov_uses_the_spec_quality_anchors_and_faststart() {
        let r = lookup(Format::Mkv, Format::Mov).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.mkv")], Path::new("out.mov"));
        assert!(argv.windows(2).any(|w| w == ["-crf", "20"]), "{argv:?}");
        assert!(argv.windows(2).any(|w| w == ["-b:a", "160k"]), "{argv:?}");
        assert!(argv.contains(&"+faststart".to_string()), "{argv:?}");
        assert!(argv.contains(&"-sn".to_string()), "{argv:?}");
    }

    #[test]
    fn video_to_mkv_uses_the_spec_quality_anchors_and_omits_movflags() {
        let r = lookup(Format::Mkv, Format::Mkv);
        assert!(r.is_none(), "mkv must never convert to itself");
        // webm has no mov_text problem, so mkv's target picks the plain
        // copy-subs variant: `VIDEO_TO_MKV`.
        let r = lookup(Format::Webm, Format::Mkv).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.webm")], Path::new("out.mkv"));
        assert!(argv.windows(2).any(|w| w == ["-crf", "20"]), "{argv:?}");
        assert!(argv.windows(2).any(|w| w == ["-b:a", "160k"]), "{argv:?}");
        assert!(
            !argv.contains(&"-movflags".to_string()),
            "mkv is not part of the mov/mp4 muxer family: {argv:?}"
        );
    }

    /// The defect this whole recipe pair exists to fix: `VIDEO_TO_MKV` used
    /// to be a byte-for-byte copy of `VIDEO_TO_MP4`'s stream handling --
    /// `-sn` and a "subtitles and extra audio tracks are dropped" warning --
    /// even though matroska (unlike mp4) can hold every subtitle codec and
    /// every audio track a source carries. `-map 0` plus a per-stream codec
    /// must now carry everything through, with no lossy warning.
    #[test]
    fn video_to_mkv_preserves_every_stream_and_carries_no_lossy_warning() {
        let r = lookup(Format::Webm, Format::Mkv).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.webm")], Path::new("out.mkv"));
        assert_eq!(
            argv,
            vec![
                "-i", "in.webm", "-map", "0", "-c:v", "libx264", "-crf", "20", "-preset", "medium",
                "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "160k", "-c:s", "copy", "-y",
                "out.mkv",
            ]
        );
        assert!(
            !argv.contains(&"-sn".to_string()),
            "mkv can hold any subtitle codec, so it must not disable subtitle \
             stream selection: {argv:?}"
        );
        assert_eq!(
            r.warnings.len(),
            0,
            "nothing is lost on this path any more, so it must not carry a \
             lossy-conversion warning: {:?}",
            r.warnings
        );
    }

    /// `avi` gets the same copy-subs treatment as `webm`: neither source's
    /// subtitle codec (if any) is `mov_text`.
    #[test]
    fn avi_to_mkv_also_uses_the_copy_subs_variant() {
        let r = lookup(Format::Avi, Format::Mkv).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.avi")], Path::new("out.mkv"));
        assert!(argv.windows(2).any(|w| w == ["-c:s", "copy"]), "{argv:?}");
        assert_eq!(r.warnings.len(), 0, "{:?}", r.warnings);
    }

    /// mp4 and mov's *only* subtitle codec is `mov_text`, which matroska has
    /// no codec ID for (see `REMUX_MKV_SRT_SUBS`'s live-verified error), so
    /// both sources must route to the SRT-subs variant -- unlike `webm`/
    /// `avi` above, and unlike `mkv_remux_for`, this needs no probe: it is
    /// decided purely from the source format, since a probe is never
    /// available on the path that reaches this recipe at all.
    #[test]
    fn mp4_and_mov_to_mkv_use_the_srt_subs_variant() {
        for (from, ext) in [(Format::Mp4, "mp4"), (Format::Mov, "mov")] {
            let r = lookup(from, Format::Mkv).unwrap();
            let argv = r.steps[0].render(&[Path::new(&format!("in.{ext}"))], Path::new("out.mkv"));
            assert_eq!(
                argv,
                vec![
                    "-i",
                    &format!("in.{ext}"),
                    "-map",
                    "0",
                    "-c:v",
                    "libx264",
                    "-crf",
                    "20",
                    "-preset",
                    "medium",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-b:a",
                    "160k",
                    "-c:s",
                    "srt",
                    "-y",
                    "out.mkv",
                ],
                "{from:?}"
            );
            assert!(!argv.contains(&"-sn".to_string()), "{from:?}: {argv:?}");
            assert_eq!(r.warnings.len(), 1, "{from:?}: {:?}", r.warnings);
            assert!(
                r.warnings[0].contains("mov_text"),
                "{from:?}: {:?}",
                r.warnings
            );
            assert!(
                r.warnings[0].contains("re-encoded"),
                "the warning must not claim video/audio are stream-copied on \
                 this transcode path: {from:?}: {:?}",
                r.warnings
            );
        }
    }

    #[test]
    fn avi_is_never_registered_as_a_video_target() {
        // AVI cannot cleanly hold modern codecs; writing a new .avi would be
        // a downgrade, not a conversion. It stays a source only.
        for (from, to) in all_pairs() {
            assert_ne!(
                to,
                Format::Avi,
                "{from:?}->avi must not be registered: avi is source-only"
            );
        }
    }

    #[test]
    fn mov_and_mkv_are_registered_as_targets_for_every_other_video_source() {
        for from in [Format::Mp4, Format::Mkv, Format::Webm, Format::Avi] {
            assert!(lookup(from, Format::Mov).is_some(), "{from:?}->mov");
        }
        for from in [Format::Mp4, Format::Mov, Format::Webm, Format::Avi] {
            assert!(lookup(from, Format::Mkv).is_some(), "{from:?}->mkv");
        }
    }

    #[test]
    fn needs_probe_covers_mov_and_mkv_targets() {
        assert!(needs_probe(Format::Mp4, Format::Mov));
        assert!(needs_probe(Format::Mp4, Format::Mkv));
        assert!(needs_probe(Format::Mov, Format::Mkv));
        assert!(needs_probe(Format::Mkv, Format::Mov));
    }

    #[test]
    fn can_remux_accepts_prores_into_mov_but_rejects_av1() {
        let prores = crate::MediaProbe {
            video_codec: Some("prores".into()),
            audio_codec: Some("aac".into()),
            subtitle_codec: None,
        };
        assert!(can_remux(Format::Mov, &prores), "{prores:?}");

        // Verified live against ffmpeg 9.0: the mov muxer refuses av1
        // outright ("av1 only supported in MP4 and AVIF"), even though av1
        // is legal in MP4_COMPATIBLE_VIDEO.
        let av1 = crate::MediaProbe {
            video_codec: Some("av1".into()),
            audio_codec: Some("aac".into()),
            subtitle_codec: None,
        };
        assert!(!can_remux(Format::Mov, &av1), "{av1:?}");
    }

    #[test]
    fn can_remux_accepts_a_broad_range_of_codecs_into_mkv() {
        // Codecs mov and/or mp4/webm reject outright, all verified live to
        // mux cleanly into matroska.
        for (video, audio) in [
            ("av1", "flac"),
            ("vp9", "opus"),
            ("prores", "pcm_s16le"),
            ("h264", "truehd"),
            ("mjpeg", "dts"),
        ] {
            let probe = crate::MediaProbe {
                video_codec: Some(video.into()),
                audio_codec: Some(audio.into()),
                subtitle_codec: None,
            };
            assert!(can_remux(Format::Mkv, &probe), "{probe:?}");
        }
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

    // --- Task 2: pandoc+typst fallback for docx/odt -> pdf -----------------

    #[test]
    fn docx_and_odt_to_pdf_have_a_pandoc_typst_fallback() {
        for from in [Format::Docx, Format::Odt] {
            assert!(
                has_fallback(from, Format::Pdf),
                "{from:?}->pdf must have a fallback recipe"
            );
            let r = lookup_fallback(from, Format::Pdf)
                .unwrap_or_else(|| panic!("{from:?}->pdf fallback must exist"));
            assert_eq!(r.steps.len(), 1);
            assert_eq!(r.steps[0].backend, Backend::Pandoc);
        }
    }

    #[test]
    fn xlsx_and_pptx_to_pdf_have_no_fallback() {
        // pandoc cannot read spreadsheets or slide decks, so these two
        // office-source pairs must stay LibreOffice-only.
        for from in [Format::Xlsx, Format::Pptx] {
            assert!(
                !has_fallback(from, Format::Pdf),
                "{from:?}->pdf must not have a pandoc fallback"
            );
            assert!(lookup_fallback(from, Format::Pdf).is_none());
        }
    }

    #[test]
    fn fallback_recipe_renders_pdf_engine_pointed_at_typst() {
        let r = lookup_fallback(Format::Docx, Format::Pdf).unwrap();
        let argv = r.steps[0].render(&[Path::new("in.docx")], Path::new("out.pdf"));
        assert_eq!(
            argv,
            vec![
                "in.docx",
                "--pdf-engine",
                "<resolved typst path>",
                "-o",
                "out.pdf",
            ]
        );
    }

    #[test]
    fn fallback_recipe_warns_about_fidelity_and_suggests_libreoffice() {
        let r = lookup_fallback(Format::Docx, Format::Pdf).unwrap();
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].contains("LibreOffice"), "{:?}", r.warnings);
    }

    #[test]
    fn fallback_table_never_adds_a_pair_all_pairs_does_not_already_list() {
        // Every FALLBACK_TABLE key must already be a key in TABLE, i.e. in
        // all_pairs() -- this table must never introduce a new supported
        // pair, only offer a second recipe for one that already exists.
        for from in [Format::Docx, Format::Odt] {
            assert!(
                all_pairs().contains(&(from, Format::Pdf)),
                "{from:?}->pdf must already be listed by all_pairs"
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
