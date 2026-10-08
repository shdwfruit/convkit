//! `--strip-metadata`: what it removes and what it keeps.
//!
//! What is kept is listed, not what is removed. A list of what to remove
//! misses the keys nobody thought of: real iPhone files also carry
//! `com.apple.quicktime.location.accuracy.horizontal`, and GoPro, DJI and
//! Android each have their own. A privacy flag has to fail closed.

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

#[cfg(test)]
mod tests {
    use super::*;

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
