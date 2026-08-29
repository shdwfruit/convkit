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

/// Canonical extension first, then accepted aliases.
const TABLE: &[(Format, Kind, &[&str])] = &[
    (Format::Mp4, Kind::Video, &["mp4", "m4v"]),
    (Format::Mov, Kind::Video, &["mov", "qt"]),
    (Format::Mkv, Kind::Video, &["mkv"]),
    (Format::Webm, Kind::Video, &["webm"]),
    (Format::Avi, Kind::Video, &["avi"]),
    (Format::Mp3, Kind::Audio, &["mp3"]),
    (Format::M4a, Kind::Audio, &["m4a", "aac"]),
    (Format::Wav, Kind::Audio, &["wav", "wave"]),
    (Format::Flac, Kind::Audio, &["flac"]),
    (Format::Gif, Kind::Image, &["gif"]),
    (Format::Heic, Kind::Image, &["heic"]),
    (Format::Heif, Kind::Image, &["heif"]),
    (Format::Jpg, Kind::Image, &["jpg", "jpeg", "jpe"]),
    (Format::Png, Kind::Image, &["png"]),
    (Format::Webp, Kind::Image, &["webp"]),
    (Format::Avif, Kind::Image, &["avif"]),
    (Format::Tiff, Kind::Image, &["tiff", "tif"]),
    (Format::Bmp, Kind::Image, &["bmp"]),
    (Format::Svg, Kind::Image, &["svg"]),
    (Format::Pdf, Kind::Document, &["pdf"]),
    (Format::Docx, Kind::Document, &["docx"]),
    (Format::Xlsx, Kind::Document, &["xlsx"]),
    (Format::Pptx, Kind::Document, &["pptx"]),
    (Format::Odt, Kind::Document, &["odt"]),
    (Format::Ods, Kind::Document, &["ods"]),
    (Format::Html, Kind::Document, &["html", "htm"]),
    (Format::Md, Kind::Document, &["md", "markdown"]),
];

/// Levenshtein distance at or below this counts as a near miss worth suggesting.
const SUGGEST_MAX_DISTANCE: usize = 2;

impl Format {
    /// Accepts `"mp4"`, `".mp4"`, `"MP4"`, and known aliases such as `"jpeg"`.
    pub fn from_ext(ext: &str) -> Option<Format> {
        let needle = ext.trim_start_matches('.').to_ascii_lowercase();
        TABLE
            .iter()
            .find(|(_, _, exts)| exts.contains(&needle.as_str()))
            .map(|(f, _, _)| *f)
    }

    pub fn from_path(path: &Path) -> Option<Format> {
        Format::from_ext(path.extension()?.to_str()?)
    }

    /// The canonical extension, without a leading dot.
    pub fn ext(&self) -> &'static str {
        TABLE
            .iter()
            .find(|(f, _, _)| f == self)
            .map(|(_, _, exts)| exts[0])
            .expect("every Format variant has a TABLE row")
    }

    pub fn kind(&self) -> Kind {
        TABLE
            .iter()
            .find(|(f, _, _)| f == self)
            .map(|(_, k, _)| *k)
            .expect("every Format variant has a TABLE row")
    }

    /// All known formats, derived from `TABLE` so this can never drift out of
    /// sync with the enum's actual extension/kind mappings.
    pub fn all() -> &'static [Format] {
        static ALL: LazyLock<Vec<Format>> =
            LazyLock::new(|| TABLE.iter().map(|(f, _, _)| *f).collect());
        &ALL
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
            .flat_map(|(f, _, exts)| exts.iter().map(move |e| (*f, *e)))
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

    #[test]
    fn classifies_kinds() {
        assert_eq!(Format::Mp4.kind(), Kind::Video);
        assert_eq!(Format::Flac.kind(), Kind::Audio);
        assert_eq!(Format::Gif.kind(), Kind::Image);
        assert_eq!(Format::Docx.kind(), Kind::Document);
    }
}
