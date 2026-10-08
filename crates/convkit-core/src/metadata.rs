//! `--strip-metadata`: what it removes and what it keeps.
//!
//! What is kept is listed, not what is removed. A list of what to remove
//! misses the keys nobody thought of: real iPhone files also carry
//! `com.apple.quicktime.location.accuracy.horizontal`, and GoPro, DJI and
//! Android each have their own. A privacy flag has to fail closed.

use crate::probe::LocationTags;
use crate::{Format, Kind, MediaProbe, Tuning};

/// ImageMagick's half, rendered right after `-auto-orient`, which has to
/// read the EXIF orientation before it goes.
///
/// `+profile '!icc,*'` deletes every profile whose name is not `icc`: EXIF
/// (GPS, camera, serial numbers, owner, capture time), XMP, IPTC and 8BIM.
/// `-strip` is not used because it drops the colour profile too, which
/// shifts the colours of a wide-gamut (Display P3) phone photo. The two
/// `+set`s remove what is left as properties rather than profiles: the
/// JPEG writer emits `comment` as a COM marker, and the TIFF writer emits
/// `comment` and `label` as tags.
pub(crate) const MAGICK_STRIP: &[&str] =
    &["+profile", "!icc,*", "+set", "comment", "+set", "label"];

/// The tags an ffmpeg output keeps: the ones that name the content, not
/// when, where, with what or by whom it was made. `date` is left out on
/// purpose: on a recording it is the capture date.
pub const KEPT_TAGS: &[&str] = &[
    "title",
    "artist",
    "album",
    "album_artist",
    "composer",
    "genre",
    "track",
    "disc",
];

/// ffmpeg's half. `-map_metadata -1` clears the global tags, every
/// stream's tags and the chapter titles in one go (the same on ffmpeg 6.1
/// and 9.0), and leaves a stream copy working. Each kept tag the probe
/// read is then written back.
pub(crate) fn ffmpeg_args(kept: &[(String, String)]) -> Vec<String> {
    let mut argv = vec!["-map_metadata".to_string(), "-1".to_string()];
    for (key, value) in kept {
        argv.push("-metadata".to_string());
        argv.push(format!("{key}={value}"));
    }
    argv
}

/// Whether ImageMagick carries an EXIF GPS from `from` into `to`, as
/// measured: jpg, webp, avif and pdf keep the EXIF profile (a pdf inside its
/// embedded JPEG), and png keeps it three times over (an eXIf chunk, a raw
/// profile, and a text chunk per field). The tiff writer never writes EXIF
/// and bmp holds none. A TIFF source keeps its EXIF in IFDs, not a profile,
/// which only the png writer turns into text.
pub(crate) fn keeps_exif_location(from: Format, to: Format) -> bool {
    let has_exif = matches!(
        from,
        Format::Heic
            | Format::Heif
            | Format::Jpg
            | Format::Png
            | Format::Webp
            | Format::Avif
            | Format::Tiff
    );
    has_exif
        && match to {
            Format::Png => true,
            Format::Jpg | Format::Webp | Format::Avif | Format::Pdf => from != Format::Tiff,
            _ => false,
        }
}

/// Whether `to` keeps the location tags `tags` names, as measured: every
/// tagged target keeps the classic `location`, and Apple's keys survive only
/// where the muxer writes keys it does not know (Matroska, WebM, ID3,
/// Vorbis comments). wav and gif keep neither.
fn keeps_location_tags(to: Format, tags: LocationTags) -> bool {
    match to {
        Format::Mp4 | Format::Mov | Format::M4a => tags.classic,
        Format::Mkv | Format::Webm | Format::Mp3 | Format::Flac => tags.classic || tags.apple,
        _ => false,
    }
}

/// The note for a source that records where it was made, when the output
/// will keep it and the flag is off. Decided from what the probe read; an
/// unread source gets no note, since a note on every conversion that might
/// carry a location would be on nearly every photo and video.
pub(crate) fn location_note(
    from: Format,
    to: Format,
    probe: Option<&MediaProbe>,
    tuning: &Tuning,
) -> Option<String> {
    let p = probe?;
    if tuning.strip_metadata {
        return None;
    }
    let image = p.image.is_some_and(|i| i.location) && keeps_exif_location(from, to);
    (image || keeps_location_tags(to, p.location)).then(|| {
        format!(
            "The source records a GPS location, and the {} keeps it; add --strip-metadata \
             to remove it.",
            to.ext()
        )
    })
}

/// Said when the flag cleared a video or audio file's tags with no probe to
/// read the kept ones from: ffprobe is missing, or could not read the file.
pub(crate) const TAGS_UNREAD_NOTE: &str = "ffprobe could not read the tags, so all of them \
     were removed, title and artist included.";

/// `TAGS_UNREAD_NOTE` when it applies: the flag is on, the target holds
/// tags (a gif holds none), and there is no probe.
pub(crate) fn tags_unread_note(
    to: Format,
    tuning: &Tuning,
    probe: Option<&MediaProbe>,
) -> Option<String> {
    let holds_tags = matches!(to.kind(), Kind::Video | Kind::Audio);
    (tuning.strip_metadata && holds_tags && probe.is_none()).then(|| TAGS_UNREAD_NOTE.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{ImageTraits, LocationTags};

    fn photo(location: bool) -> MediaProbe {
        MediaProbe {
            image: Some(ImageTraits {
                alpha: Some(false),
                multi_frame: false,
                location,
            }),
            ..MediaProbe::default()
        }
    }

    fn tagged(classic: bool, apple: bool) -> MediaProbe {
        MediaProbe {
            location: LocationTags { classic, apple },
            ..MediaProbe::default()
        }
    }

    /// Each target as measured: ImageMagick carries an EXIF GPS into jpg,
    /// webp, avif, pdf and png, and a TIFF's (held in IFDs) only into png.
    #[test]
    fn an_image_note_follows_what_each_target_keeps() {
        let off = Tuning::default();
        let note = |from, to, p: &MediaProbe| location_note(from, to, Some(p), &off);
        assert_eq!(
            note(Format::Heic, Format::Jpg, &photo(true)).unwrap(),
            "The source records a GPS location, and the jpg keeps it; add \
             --strip-metadata to remove it."
        );
        for to in [Format::Png, Format::Webp, Format::Avif, Format::Pdf] {
            assert!(note(Format::Heic, to, &photo(true)).is_some(), "{to:?}");
        }
        for to in [Format::Tiff, Format::Bmp] {
            assert!(note(Format::Heic, to, &photo(true)).is_none(), "{to:?}");
        }
        assert!(note(Format::Tiff, Format::Jpg, &photo(true)).is_none());
        assert!(note(Format::Tiff, Format::Png, &photo(true)).is_some());
        assert!(note(Format::Heic, Format::Jpg, &photo(false)).is_none());
        assert!(location_note(Format::Heic, Format::Jpg, None, &off).is_none());
        let on = Tuning {
            strip_metadata: true,
            ..Tuning::default()
        };
        assert!(location_note(Format::Heic, Format::Jpg, Some(&photo(true)), &on).is_none());
    }

    /// The mov muxer writes `location` but drops Apple's keys; Matroska,
    /// WebM, ID3 and Vorbis comments keep both; wav and gif keep neither.
    #[test]
    fn a_media_note_follows_which_tag_each_target_keeps() {
        let off = Tuning::default();
        let note = |to, p: &MediaProbe| location_note(Format::Mov, to, Some(p), &off);
        let (classic, apple) = (tagged(true, false), tagged(false, true));
        for to in [Format::Mp4, Format::Mov, Format::M4a] {
            assert!(note(to, &classic).is_some(), "{to:?}");
            assert!(note(to, &apple).is_none(), "{to:?}");
        }
        for to in [Format::Mkv, Format::Webm, Format::Mp3, Format::Flac] {
            assert!(note(to, &classic).is_some(), "{to:?}");
            assert!(note(to, &apple).is_some(), "{to:?}");
        }
        for to in [Format::Wav, Format::Gif] {
            assert!(note(to, &tagged(true, true)).is_none(), "{to:?}");
        }
        assert!(note(Format::Mp4, &tagged(false, false)).is_none());
    }

    #[test]
    fn ffmpeg_clears_everything_then_writes_back_what_it_keeps() {
        assert_eq!(ffmpeg_args(&[]), ["-map_metadata", "-1"]);
        let kept = [("title".to_string(), "A = B".to_string())];
        assert_eq!(
            ffmpeg_args(&kept),
            ["-map_metadata", "-1", "-metadata", "title=A = B"]
        );
    }
}
