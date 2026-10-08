//! The icon decisions that depend on the source, made while planning from
//! what was read of it: the square an icon centers its picture on, the
//! density an SVG renders at, which of an .ico's frames to read, and the
//! icon sizes that come out enlarged.

use crate::probe::MediaProbe;
use crate::video::{Enlargement, ResolvedVideo};
use crate::{registry, Format};

/// The sizes an .ico target holds, largest first: what Windows uses for
/// file and desktop icons, and the 16, 32 and 48 a browser tab uses.
/// `registry`'s `-define icon:auto-resize` spells the same list.
pub const SIZES: [u32; 7] = [256, 128, 64, 48, 32, 24, 16];

/// Fills in `r` for an icon pair. A source that was not read changes
/// nothing, and the recipes fall back to what needs no size.
pub(crate) fn resolve(from: Format, to: Format, probe: Option<&MediaProbe>, r: &mut ResolvedVideo) {
    if from == Format::Ico {
        r.frame = probe.and_then(|p| largest_frame(&p.frames));
    }
    if to != Format::Ico {
        return;
    }
    let Some((w, h)) = probe.and_then(|p| Some((p.width?, p.height?))) else {
        return;
    };
    if from == Format::Svg {
        // Read at the recipe's own density; render it so its longer side
        // is the largest icon, rather than enlarge a smaller rendering. It
        // then still goes through the fallback square, since ImageMagick's
        // rounding of an SVG's size is not this module's to predict, and a
        // canvas a pixel too small would crop it.
        r.density = Some(svg_density(w.max(h)));
        return;
    }
    r.icon_canvas = Some(w.max(h));
    r.enlarged = enlargement(w, h);
}

/// The density at which an SVG whose longer side is `long` pixels at
/// `registry::SVG_DENSITY` renders at the largest icon size or just over.
fn svg_density(long: u32) -> String {
    let base: f64 = registry::SVG_DENSITY
        .parse()
        .expect("an authored density is a number");
    let wanted = base * f64::from(SIZES[0]) / f64::from(long);
    format!("{}", wanted.ceil() as u64)
}

/// The largest frame by area, then by bit depth: an old icon holds the
/// same size at 4 and at 32 bits. The first wins a tie.
fn largest_frame(frames: &[(u32, u32, u32)]) -> Option<usize> {
    (0..frames.len()).max_by_key(|&i| {
        let (w, h, depth) = frames[i];
        (u64::from(w) * u64::from(h), depth, std::cmp::Reverse(i))
    })
}

/// The warning for an icon whose largest sizes are bigger than its
/// source. Not a question: the sizes are what an .ico is for, and a person
/// converting a small picture to one wants it either way.
fn enlargement(w: u32, h: u32) -> Option<Enlargement> {
    let long = w.max(h);
    let bigger: Vec<u32> = SIZES.iter().rev().copied().filter(|&s| s > long).collect();
    let (last, rest) = bigger.split_last()?;
    let (sizes, they) = if rest.is_empty() {
        (format!("{last} px icon size"), "it looks")
    } else {
        let rest: Vec<String> = rest.iter().map(u32::to_string).collect();
        (
            format!("{} and {last} px icon sizes", rest.join(", ")),
            "they look",
        )
    };
    Some(Enlargement {
        warning: format!(
            "The {w}x{h} source is enlarged for the {sizes}: enlarging adds no detail, \
             so {they} soft. A source {} px or more on its longer side fills every size.",
            SIZES[0]
        ),
        pixel_ratio: Some((f64::from(SIZES[0]) / f64::from(long)).powi(2)),
        source: Some([w, h]),
        output: Some([SIZES[0], SIZES[0]]),
        estimated_bytes: None,
        needs_confirmation: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(w: u32, h: u32) -> MediaProbe {
        MediaProbe {
            width: Some(w),
            height: Some(h),
            ..MediaProbe::default()
        }
    }

    fn resolved(from: Format, to: Format, probe: Option<&MediaProbe>) -> ResolvedVideo {
        let mut r = ResolvedVideo::default();
        resolve(from, to, probe, &mut r);
        r
    }

    #[test]
    fn a_raster_icon_is_squared_at_its_own_longer_side() {
        let r = resolved(Format::Png, Format::Ico, Some(&read(1200, 630)));
        assert_eq!(r.icon_canvas, Some(1200));
        assert_eq!(r.enlarged, None, "1200 px covers every size");
        let r = resolved(Format::Png, Format::Ico, Some(&read(256, 256)));
        assert_eq!(r.icon_canvas, Some(256));
        assert_eq!(r.enlarged, None);
    }

    #[test]
    fn the_warning_names_every_size_bigger_than_the_source() {
        let warn = |w, h| {
            resolved(Format::Jpg, Format::Ico, Some(&read(w, h)))
                .enlarged
                .unwrap()
        };
        let e = warn(32, 32);
        assert!(
            e.warning.starts_with(
                "The 32x32 source is enlarged for the 48, 64, 128 and 256 px icon sizes: \
                 enlarging adds no detail, so they look soft."
            ),
            "{}",
            e.warning
        );
        assert!(!e.needs_confirmation, "a warning, never a question");
        assert_eq!(e.pixel_ratio, Some(64.0));
        assert!(
            warn(100, 60)
                .warning
                .contains("for the 128 and 256 px icon sizes"),
            "{}",
            warn(100, 60).warning
        );
        assert!(
            warn(200, 120)
                .warning
                .contains("for the 256 px icon size: enlarging adds no detail, so it looks soft"),
            "{}",
            warn(200, 120).warning
        );
    }

    /// An SVG is rendered at the size the icon needs, so nothing is
    /// enlarged and there is nothing to warn about.
    #[test]
    fn an_svg_icon_renders_at_the_largest_size() {
        // 24 units at 384 dpi is 96 px; 1024 dpi renders 256.
        let r = resolved(Format::Svg, Format::Ico, Some(&read(96, 96)));
        assert_eq!(r.density.as_deref(), Some("1024"));
        assert_eq!(r.enlarged, None);
        assert_eq!(r.icon_canvas, None, "the fallback square, see resolve");
        // A large drawing is rendered smaller rather than at a size that
        // is only shrunk again.
        let r = resolved(Format::Svg, Format::Ico, Some(&read(2048, 1024)));
        assert_eq!(r.density.as_deref(), Some("48"));
    }

    #[test]
    fn an_icos_largest_frame_is_chosen_wherever_it_sits() {
        let probe = |frames: Vec<(u32, u32, u32)>| MediaProbe {
            frames,
            ..MediaProbe::default()
        };
        let ascending = probe(vec![(16, 16, 8), (32, 32, 8), (48, 48, 8)]);
        assert_eq!(
            resolved(Format::Ico, Format::Png, Some(&ascending)).frame,
            Some(2)
        );
        let descending = probe(vec![(256, 256, 8), (16, 16, 8)]);
        assert_eq!(
            resolved(Format::Ico, Format::Png, Some(&descending)).frame,
            Some(0)
        );
        let depths = probe(vec![(32, 32, 4), (32, 32, 8), (16, 16, 8)]);
        assert_eq!(
            resolved(Format::Ico, Format::Png, Some(&depths)).frame,
            Some(1)
        );
        assert_eq!(resolved(Format::Ico, Format::Png, None).frame, None);
    }

    #[test]
    fn an_unread_source_changes_nothing() {
        assert_eq!(
            resolved(Format::Png, Format::Ico, None),
            ResolvedVideo::default()
        );
        assert_eq!(
            resolved(Format::Png, Format::Jpg, Some(&read(32, 32))),
            ResolvedVideo::default()
        );
    }
}
