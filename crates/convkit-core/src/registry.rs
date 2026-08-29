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

static TABLE: LazyLock<Table> = LazyLock::new(|| {
    let mut t = Table::new();
    insert_image_family(&mut t);
    // Task 5 adds insert_media_family(&mut t);
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
}
