//! Which `(from, to)` pairs this harness exercises.
//!
//! Scoped to images on purpose (see the task brief: "A working images-only
//! harness is far more useful than a broken one that also claims to handle
//! documents"). convkit-core's registry has three families -- image, media,
//! video/audio, and document -- and this harness only drives the image
//! family, plus the two GIF pathways that specifically matter to the
//! imagequant engine-swap question named in the brief (ffmpeg's
//! `palettegen`/`paletteuse`, used only by the video/image -> GIF recipe):
//! a video source converting *to* GIF, and GIF converting back to video.
//! Everything else -- audio, documents, general video transcoding -- is out
//! of scope; see the harness report's "does not yet cover" section.

use convkit_core::{registry, Format, Kind};

/// Whether `(from, to)` is a pair this harness measures.
pub fn in_scope(from: Format, to: Format) -> bool {
    let both_image = from.kind() == Kind::Image && to.kind() == Kind::Image;
    let gif_pathway = to == Format::Gif || (from == Format::Gif && to.kind() == Kind::Video);
    both_image || gif_pathway
}

/// Every in-scope target format reachable from `from`, per
/// `registry::all_pairs()` -- i.e. every pair convkit itself actually
/// advertises as supported, filtered down to [`in_scope`]. Order is
/// whatever `all_pairs()` yields (a `BTreeMap`'s key order), which is
/// deterministic and good enough; callers that need a stable report order
/// sort separately.
pub fn applicable_targets(from: Format) -> Vec<Format> {
    registry::all_pairs()
        .into_iter()
        .filter(|&(f, t)| f == from && in_scope(f, t))
        .map(|(_, t)| t)
        .collect()
}

/// Whether ImageMagick's `identify`/`compare` can meaningfully inspect an
/// output in this format at all -- i.e. whether the pixel/dimensions/
/// colourspace/ICC/orientation axes apply to it. `false` only for the
/// gif->video pathway's `.mp4` output, which is not something `identify`
/// can decode.
pub fn is_magick_inspectable(to: Format) -> bool {
    to.kind() == Kind::Image
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_to_image_pairs_are_in_scope() {
        assert!(in_scope(Format::Heic, Format::Jpg));
        assert!(in_scope(Format::Jpg, Format::Png));
        assert!(in_scope(Format::Svg, Format::Png));
    }

    #[test]
    fn the_two_gif_pathways_are_in_scope() {
        assert!(in_scope(Format::Mp4, Format::Gif));
        assert!(in_scope(Format::Gif, Format::Mp4));
    }

    #[test]
    fn unrelated_video_and_document_pairs_are_out_of_scope() {
        assert!(!in_scope(Format::Mkv, Format::Mp4));
        assert!(!in_scope(Format::Docx, Format::Pdf));
        assert!(!in_scope(Format::Md, Format::Pdf));
        assert!(!in_scope(Format::Mp4, Format::Mp3));
    }

    #[test]
    fn applicable_targets_matches_registry_for_a_known_image_source() {
        let targets = applicable_targets(Format::Jpg);
        assert!(targets.contains(&Format::Png));
        assert!(targets.contains(&Format::Webp));
        assert!(!targets.contains(&Format::Jpg), "no self-pair");
    }

    #[test]
    fn applicable_targets_for_gif_is_exactly_mp4() {
        assert_eq!(applicable_targets(Format::Gif), vec![Format::Mp4]);
    }

    #[test]
    fn mp4_targets_include_gif_but_not_other_video_containers() {
        let targets = applicable_targets(Format::Mp4);
        assert!(targets.contains(&Format::Gif));
        assert!(!targets.contains(&Format::Mkv));
        assert!(!targets.contains(&Format::Webm));
    }

    #[test]
    fn inspectability_matches_kind() {
        assert!(is_magick_inspectable(Format::Png));
        assert!(is_magick_inspectable(Format::Gif));
        assert!(!is_magick_inspectable(Format::Mp4));
    }
}
