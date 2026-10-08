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

/// Gives an mkv's attachments (fonts) their own tags back after
/// `ffmpeg_args` has cleared them: Matroska refuses an attachment without
/// its `filename` and `mimetype` ("Could not write header"), and a font's
/// file name says nothing about who made the video. Only where the probe
/// saw an attachment, since ffmpeg errors on a stream specifier that
/// matches no stream (6.1 and 9.0 alike).
pub(crate) const KEEP_ATTACHMENT_TAGS: [&str; 2] = ["-map_metadata:s:t", "0:s:t"];

/// Said when an mkv is stripped into its own format with no probe: without
/// one conv cannot tell whether it has attachments, which cannot be copied
/// once their tags are cleared, so they are left out.
pub(crate) const ATTACHMENTS_UNREAD_NOTE: &str = "Any attachments (fonts) are left out, \
     since ffprobe could not read the file and they cannot be copied without their tags.";

/// Whether a file of format `f` can be stripped into a file of its own
/// format (`photo.jpg` -> `photo-stripped.jpg`), and if not, why, with the
/// fix. Shared by the planner and the command line, so both say the same.
pub fn in_place(f: Format) -> Result<(), String> {
    match f {
        Format::Jpg
        | Format::Png
        | Format::Webp
        | Format::Avif
        | Format::Tiff
        | Format::Bmp
        | Format::Mp4
        | Format::Mov
        | Format::Mkv
        | Format::Webm
        | Format::Mp3
        | Format::M4a
        | Format::Flac
        | Format::Wav => Ok(()),
        Format::Heic | Format::Heif => Err(format!(
            "conv cannot write {}; add --to jpg to strip it into a jpg",
            f.ext()
        )),
        _ => Err(match strip_target_for(f) {
            Some(to) => format!(
                "conv cannot keep {0} files as {0}; add --to {1} to strip them",
                f.ext(),
                to.ext()
            ),
            None => format!(
                "--strip-metadata does not apply to {} files: it covers image, video and \
                 audio conversions",
                f.ext()
            ),
        }),
    }
}

/// For a file conv cannot strip into its own format, the format to name in
/// the fix: one it converts that file to with the flag. `None` for a
/// document, which the flag does not cover.
pub fn strip_target_for(f: Format) -> Option<Format> {
    match f {
        Format::Heic | Format::Heif => Some(Format::Jpg),
        Format::Svg => Some(Format::Png),
        // gif -> mp4 is the only conversion conv has for a gif.
        Format::Gif | Format::Avi => Some(Format::Mp4),
        _ => None,
    }
}

/// The note on a lossy image stripped into its own format: ImageMagick
/// decodes and encodes again, so the picture pays a generation.
pub(crate) fn reencode_note(to: Format, tuning: &Tuning) -> String {
    let quality = tuning.quality.map_or_else(
        || crate::registry::IMAGE_QUALITY.to_string(),
        |q| q.to_string(),
    );
    format!(
        "The {0} is re-encoded at quality {quality} to remove its metadata; ImageMagick \
         cannot take it out of a {0} without re-encoding.",
        to.ext()
    )
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

/// The note on labels the flag clears that the target would otherwise have
/// kept, as measured with conv: track languages survive into mp4, mov, m4a,
/// mkv and webm, and chapter names into those and mp3; flac and wav keep
/// neither. Writing them back would mean matching every output stream to
/// its input across each mapping, which a wrong guess turns into a
/// mislabelled track, so the loss is said instead.
pub(crate) fn labels_note(
    to: Format,
    tuning: &Tuning,
    probe: Option<&MediaProbe>,
) -> Option<String> {
    let p = probe.filter(|_| tuning.strip_metadata)?;
    let languages = p.track_languages
        && matches!(
            to,
            Format::Mp4 | Format::Mov | Format::M4a | Format::Mkv | Format::Webm
        );
    let chapters = p.chapter_titles
        && matches!(
            to,
            Format::Mp4 | Format::Mov | Format::M4a | Format::Mkv | Format::Webm | Format::Mp3
        );
    let what = match (languages, chapters) {
        (true, true) => "Track languages and chapter names are",
        (true, false) => "Track languages are",
        (false, true) => "Chapter names are",
        (false, false) => return None,
    };
    Some(format!("{what} cleared with the rest of the tags."))
}

/// Said when the flag cleared a video or audio file's tags with no probe to
/// read the kept ones from: ffprobe is missing, or could not read the file.
pub(crate) const TAGS_UNREAD_NOTE: &str = "ffprobe could not read the tags, so all of them \
     were removed, title and artist included.";

/// `TAGS_UNREAD_NOTE` when it applies: the flag is on, both ends hold tags
/// (a gif holds none, so it is not probed for any), and there is no probe.
pub(crate) fn tags_unread_note(
    from: Format,
    to: Format,
    tuning: &Tuning,
    probe: Option<&MediaProbe>,
) -> Option<String> {
    let holds_tags = matches!(to.kind(), Kind::Video | Kind::Audio) && from != Format::Gif;
    (tuning.strip_metadata && holds_tags && probe.is_none()).then(|| TAGS_UNREAD_NOTE.to_string())
}

/// Where the flag cleared every tag, or left out an mkv's attachments, for
/// want of a probe because ffprobe itself is missing, says so as a missing
/// backend is said everywhere else, with the same fix (`conv install
/// ffprobe`, or the package manager's command). A probe that ran and could
/// not read the file keeps the plain note. Called by whoever planned with a
/// resolver in hand, since planning itself never resolves a backend.
pub fn explain_missing_ffprobe(plan: &mut crate::ConversionPlan, resolver: &crate::Resolver) {
    let unread = |w: &String| w == TAGS_UNREAD_NOTE || w == ATTACHMENTS_UNREAD_NOTE;
    if !plan.warnings.iter().any(unread) {
        return;
    }
    let Err(missing) = resolver.resolve(crate::Backend::Ffprobe) else {
        return;
    };
    let fix = missing
        .remediation
        .as_ref()
        .and_then(|r| r.managed.clone().or_else(|| r.manual.clone()));
    for note in &mut plan.warnings {
        if note == TAGS_UNREAD_NOTE {
            *note = format!(
                "{}, so every tag was removed, title and artist included.",
                missing.message
            );
            if let Some(fix) = &fix {
                note.push_str(&format!(" To keep them: {fix}"));
            }
        } else if note == ATTACHMENTS_UNREAD_NOTE {
            *note = format!(
                "{}, so any attachments (fonts) are left out.",
                missing.message
            );
        }
    }
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

    /// A format conv cannot strip into itself is pointed at one it can
    /// convert to with the flag: a real pair that carries the strip, never
    /// a guess. A document has none.
    #[test]
    fn the_fix_for_a_format_kept_as_itself_is_a_real_pair() {
        for &f in Format::all() {
            if in_place(f).is_ok() {
                continue;
            }
            if let Some(to) = strip_target_for(f) {
                let recipe = crate::registry::lookup(f, to)
                    .unwrap_or_else(|| panic!("{f:?} -> {to:?} is not a pair"));
                assert!(
                    recipe
                        .steps
                        .iter()
                        .any(|s| s.args.contains(&crate::Arg::StripMetadata)),
                    "{f:?} -> {to:?}"
                );
            }
        }
        assert_eq!(strip_target_for(Format::Gif), Some(Format::Mp4));
        assert_eq!(strip_target_for(Format::Heic), Some(Format::Jpg));
        assert_eq!(strip_target_for(Format::Svg), Some(Format::Png));
        assert_eq!(strip_target_for(Format::Docx), None);
        assert_eq!(
            in_place(Format::Gif).unwrap_err(),
            "conv cannot keep gif files as gif; add --to mp4 to strip them"
        );
        assert_eq!(
            in_place(Format::Docx).unwrap_err(),
            "--strip-metadata does not apply to docx files: it covers image, video and \
             audio conversions"
        );
    }

    /// A gif holds no tags to keep, so stripping one into an mp4 needs no
    /// probe, and says nothing about one.
    #[test]
    fn a_gif_source_has_no_tags_to_be_unread() {
        let on = Tuning {
            strip_metadata: true,
            ..Tuning::default()
        };
        assert!(tags_unread_note(Format::Gif, Format::Mp4, &on, None).is_none());
        assert!(tags_unread_note(Format::Mov, Format::Mp4, &on, None).is_some());
    }

    /// Labels the target would have kept are said to go, and only those:
    /// flac and wav keep neither, mp3 keeps chapters but no languages.
    #[test]
    fn the_flag_says_when_it_clears_track_languages_or_chapter_names() {
        let on = Tuning {
            strip_metadata: true,
            ..Tuning::default()
        };
        let movie = MediaProbe {
            track_languages: true,
            chapter_titles: true,
            ..MediaProbe::default()
        };
        assert_eq!(
            labels_note(Format::Mkv, &on, Some(&movie)).unwrap(),
            "Track languages and chapter names are cleared with the rest of the tags."
        );
        assert_eq!(
            labels_note(Format::Mp3, &on, Some(&movie)).unwrap(),
            "Chapter names are cleared with the rest of the tags."
        );
        assert!(labels_note(Format::Flac, &on, Some(&movie)).is_none());
        assert!(labels_note(Format::Mkv, &Tuning::default(), Some(&movie)).is_none());
        assert!(labels_note(Format::Mkv, &on, Some(&MediaProbe::default())).is_none());
        assert!(labels_note(Format::Mkv, &on, None).is_none());
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
