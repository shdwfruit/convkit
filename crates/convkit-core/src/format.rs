use std::path::Path;
use std::sync::LazyLock;

use serde::Serialize;

/// Broad family a format belongs to. Used for grouping in `conv capabilities`
/// and for expanding family-wide registry entries; it never selects a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Video,
    Audio,
    Image,
    Document,
}

/// Every format convkit v1 knows about. Adding a variant here requires a
/// matching row in `TABLE` below (`ext()`/`kind()` panic otherwise), and even
/// then that alone is not enough to make it convertible — `registry.rs` must
/// also gain at least one recipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    // Video containers
    Mp4,
    Mov,
    Mkv,
    Webm,
    Avi,
    // Audio
    Mp3,
    M4a,
    Wav,
    Flac,
    // Images (Gif is an image; gif<->video routing is the registry's business)
    Gif,
    Heic,
    Heif,
    Jpg,
    Png,
    Webp,
    Avif,
    Tiff,
    Bmp,
    Svg,
    // Documents
    Pdf,
    Docx,
    Xlsx,
    Pptx,
    Odt,
    Ods,
    Html,
    Md,
}

/// Every format, its family, the extensions it can be **written** as
/// (`[0]` is canonical), and the extensions convkit will **read** but never
/// write.
///
/// The fourth column exists because an extension is not just a label: it is
/// what the backend uses to pick a coder. ImageMagick registers no JFIF
/// coder (`magick -list format` names JPE/JPEG/JPG/JPS/MPO and no JFIF), so
/// `magick in.png out.jfif` silently writes **PNG bytes** into a file named
/// `.jfif`. Reading one is completely safe -- magick sniffs content, not the
/// name -- and `.jfif` is what Windows and several browsers hand you, so the
/// useful direction is the safe one. Listing it here makes that direction
/// work while `Format::is_read_only_ext` keeps the other one honest.
const TABLE: &[(Format, Kind, &[&str], &[&str])] = &[
    (Format::Mp4, Kind::Video, &["mp4", "m4v"], &[]),
    (Format::Mov, Kind::Video, &["mov", "qt"], &[]),
    (Format::Mkv, Kind::Video, &["mkv"], &[]),
    (Format::Webm, Kind::Video, &["webm"], &[]),
    (Format::Avi, Kind::Video, &["avi"], &[]),
    (Format::Mp3, Kind::Audio, &["mp3"], &[]),
    (Format::M4a, Kind::Audio, &["m4a", "aac"], &[]),
    (Format::Wav, Kind::Audio, &["wav", "wave"], &[]),
    (Format::Flac, Kind::Audio, &["flac"], &[]),
    (Format::Gif, Kind::Image, &["gif"], &[]),
    (Format::Heic, Kind::Image, &["heic"], &[]),
    (Format::Heif, Kind::Image, &["heif"], &[]),
    (Format::Jpg, Kind::Image, &["jpg", "jpeg", "jpe"], &["jfif"]),
    (Format::Png, Kind::Image, &["png"], &[]),
    (Format::Webp, Kind::Image, &["webp"], &[]),
    (Format::Avif, Kind::Image, &["avif"], &[]),
    (Format::Tiff, Kind::Image, &["tiff", "tif"], &[]),
    (Format::Bmp, Kind::Image, &["bmp"], &[]),
    (Format::Svg, Kind::Image, &["svg"], &[]),
    (Format::Pdf, Kind::Document, &["pdf"], &[]),
    (Format::Docx, Kind::Document, &["docx"], &[]),
    (Format::Xlsx, Kind::Document, &["xlsx"], &[]),
    (Format::Pptx, Kind::Document, &["pptx"], &[]),
    (Format::Odt, Kind::Document, &["odt"], &[]),
    (Format::Ods, Kind::Document, &["ods"], &[]),
    (Format::Html, Kind::Document, &["html", "htm"], &[]),
    (Format::Md, Kind::Document, &["md", "markdown"], &[]),
];

/// Levenshtein distance at or below this counts as a near miss worth suggesting.
const SUGGEST_MAX_DISTANCE: usize = 2;

impl Format {
    /// Accepts `"mp4"`, `".mp4"`, `"MP4"`, and known aliases such as `"jpeg"`.
    pub fn from_ext(ext: &str) -> Option<Format> {
        let needle = ext.trim_start_matches('.').to_ascii_lowercase();
        TABLE
            .iter()
            .find(|(_, _, exts, read_only)| {
                exts.contains(&needle.as_str()) || read_only.contains(&needle.as_str())
            })
            .map(|(f, _, _, _)| *f)
    }

    pub fn from_path(path: &Path) -> Option<Format> {
        Format::from_ext(path.extension()?.to_str()?)
    }

    /// The canonical extension, without a leading dot.
    pub fn ext(&self) -> &'static str {
        TABLE
            .iter()
            .find(|(f, _, _, _)| f == self)
            .map(|(_, _, exts, _)| exts[0])
            .expect("every Format variant has a TABLE row")
    }

    pub fn kind(&self) -> Kind {
        TABLE
            .iter()
            .find(|(f, _, _, _)| f == self)
            .map(|(_, k, _, _)| *k)
            .expect("every Format variant has a TABLE row")
    }

    /// All known formats, derived from `TABLE` so this can never drift out of
    /// sync with the enum's actual extension/kind mappings.
    pub fn all() -> &'static [Format] {
        static ALL: LazyLock<Vec<Format>> =
            LazyLock::new(|| TABLE.iter().map(|(f, _, _, _)| *f).collect());
        &ALL
    }

    /// True when `ext` names a format convkit reads but cannot write under
    /// that spelling -- the backend has no coder registered for it, so
    /// honouring it as an output would put the wrong bytes in a file with
    /// the requested name. Callers parsing an *output* extension reject
    /// these; callers parsing an input accept them via `from_ext`.
    pub fn is_read_only_ext(ext: &str) -> bool {
        let needle = ext.trim_start_matches('.').to_ascii_lowercase();
        TABLE
            .iter()
            .any(|(_, _, _, read_only)| read_only.contains(&needle.as_str()))
    }

    /// Nearest known format for an unrecognised extension, for "did you mean".
    /// Returns `None` when nothing is close enough, so we never suggest nonsense.
    pub fn suggest(ext: &str) -> Option<Format> {
        let needle = ext.trim_start_matches('.').to_ascii_lowercase();
        if needle.is_empty() {
            return None;
        }
        TABLE
            .iter()
            .flat_map(|(f, _, exts, read_only)| {
                exts.iter().chain(read_only.iter()).map(move |e| (*f, *e))
            })
            .map(|(f, e)| (strsim::levenshtein(&needle, e), f))
            .filter(|(d, _)| *d <= SUGGEST_MAX_DISTANCE)
            .min_by_key(|(d, _)| *d)
            .map(|(_, f)| f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_extensions_case_insensitively_and_with_dots() {
        assert_eq!(Format::from_ext("mp4"), Some(Format::Mp4));
        assert_eq!(Format::from_ext(".MP4"), Some(Format::Mp4));
        assert_eq!(Format::from_ext("JPEG"), Some(Format::Jpg));
        assert_eq!(Format::from_ext("nonsense"), None);
    }

    #[test]
    fn round_trips_canonical_extension() {
        for f in Format::all() {
            assert_eq!(Format::from_ext(f.ext()), Some(*f), "{f:?}");
        }
    }

    #[test]
    fn suggests_near_misses_but_not_nonsense() {
        assert_eq!(Format::suggest("mp3v"), Some(Format::Mp3));
        assert_eq!(Format::suggest("docs"), Some(Format::Docx));
        assert_eq!(Format::suggest("zzzzzzzz"), None);
    }

    #[test]
    fn suggest_returns_none_for_empty_needle() {
        assert_eq!(Format::suggest(""), None);
        assert_eq!(Format::suggest("."), None);
    }

    /// `.jfif` is what Windows and several browsers hand you for a JPEG.
    /// It resolves to the same format, so it converts like one.
    #[test]
    fn a_read_only_alias_parses_as_its_format() {
        assert_eq!(Format::from_ext("jfif"), Some(Format::Jpg));
        assert_eq!(Format::from_ext(".JFIF"), Some(Format::Jpg));
        assert_eq!(
            Format::from_path(Path::new("holiday.jfif")),
            Some(Format::Jpg)
        );
    }

    /// ...but it is never what convkit *writes*: ImageMagick has no JFIF
    /// coder, so a `.jfif` output would receive the input's bytes under a
    /// name that lies about them.
    #[test]
    fn a_read_only_alias_is_flagged_and_never_canonical() {
        assert!(Format::is_read_only_ext("jfif"));
        assert!(Format::is_read_only_ext(".JFIF"));
        assert!(!Format::is_read_only_ext("jpg"));
        assert!(!Format::is_read_only_ext("jpeg"));
        assert_eq!(Format::Jpg.ext(), "jpg");
    }

    /// The two columns must stay disjoint: an extension that is both
    /// writable and read-only would make `is_read_only_ext` refuse a spelling
    /// the same row promises it can write.
    #[test]
    fn no_extension_is_both_writable_and_read_only() {
        for (f, _, writable, read_only) in TABLE {
            for ro in *read_only {
                assert!(
                    !writable.contains(ro),
                    "{f:?} lists {ro:?} as both writable and read-only"
                );
                assert!(
                    !Format::all().iter().any(|other| other.ext() == *ro),
                    "{ro:?} is some format's canonical extension and cannot be read-only"
                );
            }
        }
    }

    /// A near miss on a read-only spelling still points at the canonical one,
    /// because `suggest` answers with the format and the caller prints
    /// `ext()`.
    #[test]
    fn suggestions_for_a_read_only_alias_name_the_writable_spelling() {
        assert_eq!(Format::suggest("jfi"), Some(Format::Jpg));
        assert_eq!(Format::suggest("jfif").map(|f| f.ext()), Some("jpg"));
    }

    #[test]
    fn classifies_kinds() {
        assert_eq!(Format::Mp4.kind(), Kind::Video);
        assert_eq!(Format::Flac.kind(), Kind::Audio);
        assert_eq!(Format::Gif.kind(), Kind::Image);
        assert_eq!(Format::Docx.kind(), Kind::Document);
    }
}
