//! `--rotate`: a quarter, half or three-quarter turn, clockwise, of the
//! picture as it is shown now.
//!
//! Whatever the turn, the result's note on how to show it (a JPEG's EXIF
//! orientation tag, an mp4 or mov track's display matrix, any XMP copy)
//! says to show the picture as it is stored, so every app shows it the same
//! way. No turn ever writes any other note.
//!
//! Mostly the pixels are turned: ImageMagick's `-rotate` after
//! `-auto-orient`, or ffmpeg's `transpose` in a re-encode, after its
//! autorotation has applied the source's note. Then any XMP copy of the old
//! orientation, which ImageMagick carries over, is reset (`xmp`). Where the
//! source's note was what made it sideways, as in a video filmed with the
//! rotation locked, the turn brings it back to its stored picture: a JPEG
//! kept as a JPEG, or an mp4 or mov copied, then only has its note reset
//! (`exif`, `mov`), and nothing is re-encoded.

use serde::{Serialize, Serializer};

use crate::Format;

/// A turn as typed: `right`, `left` or `180`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Turn {
    /// 90 degrees clockwise.
    Right,
    /// 180 degrees.
    Half,
    /// 90 degrees counterclockwise.
    Left,
}

const FORMS: &str = "expected right, left or 180 (or 90, 270 degrees clockwise)";

/// Parses `right`, `left`, `180`, or a clockwise angle: `90`, `270`, `-90`.
pub fn parse(text: &str) -> Result<Turn, String> {
    let bad = || format!("invalid turn '{text}'; {FORMS}");
    let degrees: i32 = match text.to_ascii_lowercase().as_str() {
        "right" => 90,
        "left" => 270,
        number => {
            let digits = number.strip_prefix('-').unwrap_or(number);
            if digits.is_empty() || digits.len() > 3 || !digits.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(bad());
            }
            number.parse().map_err(|_| bad())?
        }
    };
    match degrees.rem_euclid(360) {
        _ if degrees.abs() > 360 => Err(bad()),
        0 => Err(format!(
            "invalid turn '{text}'; that leaves the picture as it is"
        )),
        90 => Ok(Turn::Right),
        180 => Ok(Turn::Half),
        270 => Ok(Turn::Left),
        _ => Err(bad()),
    }
}

impl Turn {
    /// The turn in degrees clockwise: 90, 180 or 270.
    pub fn clockwise(&self) -> u16 {
        match self {
            Turn::Right => 90,
            Turn::Half => 180,
            Turn::Left => 270,
        }
    }

    /// Whether it swaps the picture's width and height.
    pub fn is_quarter(&self) -> bool {
        *self != Turn::Half
    }

    /// The turn as typed back: `right`, `left`, `180`.
    pub fn text(&self) -> &'static str {
        match self {
            Turn::Right => "right",
            Turn::Half => "180",
            Turn::Left => "left",
        }
    }
}

/// How a stored picture is shown: mirrored left to right or not, then
/// turned clockwise. One of the eight orientations an EXIF tag names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Orientation {
    mirrored: bool,
    clockwise: u16,
}

impl Orientation {
    pub const UPRIGHT: Orientation = Orientation {
        mirrored: false,
        clockwise: 0,
    };

    /// The orientation an EXIF tag (1 to 8) names.
    pub fn from_exif(tag: u16) -> Option<Orientation> {
        let (mirrored, clockwise) = match tag {
            1 => (false, 0),
            2 => (true, 0),
            3 => (false, 180),
            4 => (true, 180),
            5 => (true, 270),
            6 => (false, 90),
            7 => (true, 90),
            8 => (false, 270),
            _ => return None,
        };
        Some(Orientation {
            mirrored,
            clockwise,
        })
    }

    /// The orientation an mp4 or mov display matrix shows, from its 2x2
    /// part `[a, b, c, d]` in 16.16 fixed point. QuickTime's matrices act on
    /// row vectors, `[x y] * [[a b] [c d]]`, with y pointing down the
    /// screen, so a quarter turn right is `[0, 1, -1, 0]`. `None` for a
    /// matrix that scales or skews the picture as well.
    pub fn from_matrix(m: [i32; 4]) -> Option<Orientation> {
        ALL.into_iter().find(|o| o.matrix() == m)
    }

    /// The 2x2 part of the display matrix that shows a picture this way: the
    /// mirror, then a quarter turn right `clockwise / 90` times.
    fn matrix(&self) -> [i32; 4] {
        const ONE: i32 = 0x10000;
        let mut m = if self.mirrored {
            [-ONE, 0, 0, ONE]
        } else {
            [ONE, 0, 0, ONE]
        };
        for _ in 0..self.clockwise / 90 {
            // m * [[0 1] [-1 0]]
            let [a, b, c, d] = m;
            m = [-b, a, -d, c];
        }
        m
    }

    /// This, then `turn`.
    pub fn then(self, turn: Turn) -> Orientation {
        Orientation {
            clockwise: (self.clockwise + turn.clockwise()) % 360,
            ..self
        }
    }

    /// Whether the picture is shown as it is stored.
    pub fn is_upright(&self) -> bool {
        *self == Orientation::UPRIGHT
    }
}

const ALL: [Orientation; 8] = {
    let mut all = [Orientation::UPRIGHT; 8];
    let mut i = 0;
    while i < 8 {
        all[i] = Orientation {
            mirrored: i >= 4,
            clockwise: (i % 4) as u16 * 90,
        };
        i += 1;
    }
    all
};

impl Serialize for Turn {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.text())
    }
}

impl Turn {
    /// The ffmpeg filter that turns the picture. ffmpeg has already applied
    /// any note the source had (its autorotation), so this turns the
    /// picture as it is shown.
    pub(crate) fn filter(&self) -> &'static str {
        match self {
            Turn::Right => "transpose=clock",
            Turn::Half => "hflip,vflip",
            Turn::Left => "transpose=cclock",
        }
    }

    /// ImageMagick's `-rotate` value: degrees clockwise.
    pub(crate) fn degrees(&self) -> &'static str {
        match self {
            Turn::Right => "90",
            Turn::Half => "180",
            Turn::Left => "270",
        }
    }
}

/// How a file was turned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum By {
    /// The picture was already stored the way the turn shows it, so only
    /// its orientation tag or display matrix was reset; nothing was
    /// re-encoded.
    Tag,
    /// The picture itself was turned, and its tag says it is shown as
    /// stored.
    Pixels,
}

/// What a `--rotate` conversion did, for `--json` and the executor, which
/// resets the tag of a file turned `By::Tag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RotationReport {
    pub turn: Turn,
    pub by: By,
}

/// How a file of `format` shows its stored picture, read by conv itself: a
/// JPEG's EXIF tag, an mp4 or mov's display matrix. `None` for any other
/// format, and where there is nothing plain to read.
pub fn read(format: Format, path: &std::path::Path) -> Option<Orientation> {
    match format {
        Format::Jpg => crate::exif::read(path),
        Format::Mp4 | Format::Mov => crate::mov::read(path),
        _ => None,
    }
}

/// Adds how the source shows its stored picture (`read`) to what was read
/// of it before planning, for a turn. A picture's goes on a probe made for
/// it if none ran, since nothing else of one is needed; a video's only on
/// one ffprobe made, so that a missing ffprobe still reads as one.
pub fn attach(
    from: Format,
    input: &std::path::Path,
    tuning: &crate::Tuning,
    probed: &mut Option<crate::MediaProbe>,
) {
    if tuning.rotate.is_none() {
        return;
    }
    let Some(orientation) = read(from, input) else {
        return;
    };
    match probed {
        Some(p) => p.orientation = Some(orientation),
        None if from.kind() == crate::Kind::Image => {
            *probed = Some(crate::MediaProbe {
                orientation: Some(orientation),
                ..crate::MediaProbe::default()
            });
        }
        None => {}
    }
}

/// Resets a JPEG's tag or an mp4 or mov's display matrices to show the
/// stored picture as it is, in place.
pub(crate) fn set_upright(format: Format, path: &std::path::Path) -> std::io::Result<()> {
    match format {
        Format::Jpg => crate::exif::set_upright(path),
        Format::Mp4 | Format::Mov => crate::mov::set_upright(path),
        other => unreachable!("a turn by the tag planned for {other:?}"),
    }
}

/// Whether `turn` brings a source shown as `probe` read it back to its
/// stored picture, so that resetting its note turns it: a JPEG tagged 6
/// turned left, a video filmed with the rotation locked.
pub(crate) fn undoes_note(turn: Turn, probe: Option<&crate::MediaProbe>) -> bool {
    probe
        .and_then(|p| p.orientation)
        .is_some_and(|o| o.then(turn).is_upright())
}

/// The probe as the turned picture is shown: a quarter turn swaps the
/// width and height every cap and estimate is decided against.
pub(crate) fn turned_probe(turn: Turn, probe: &crate::MediaProbe) -> crate::MediaProbe {
    let rotation = probe.rotation.unwrap_or(0) - i32::from(turn.clockwise());
    crate::MediaProbe {
        rotation: Some(rotation),
        ..probe.clone()
    }
}

/// The note on a file turned by its tag alone.
pub(crate) const TAG_NOTE: &str = "The picture was already stored this way up, so only its \
     orientation tag changed; nothing was re-encoded.";

/// The note on a video turned by re-encoding it.
pub(crate) const VIDEO_NOTE: &str = "The video is re-encoded to turn it.";

/// The note on a lossy image turned into its own format.
pub(crate) fn image_note(to: Format, tuning: &crate::Tuning) -> String {
    format!(
        "The {} is re-encoded at quality {} to turn it.",
        to.ext(),
        crate::metadata::quality_for(to, tuning)
    )
}

/// Whether a file of `f` can be turned into its own format by
/// `plan::in_place`; `Err` says what to do instead. mkv and webm are turned
/// by the media paths, and are not asked.
pub fn in_place(f: Format) -> Result<(), String> {
    match f {
        Format::Jpg
        | Format::Png
        | Format::Webp
        | Format::Avif
        | Format::Heic
        | Format::Tiff
        | Format::Bmp
        | Format::Gif
        | Format::Mp4
        | Format::Mov
        | Format::Mkv
        | Format::Webm => Ok(()),
        _ => match target_for(f) {
            Some(to) => Err(format!(
                "conv cannot keep {0} files as {0}; add --to {1} to turn them",
                f.ext(),
                to.ext()
            )),
            None => Err(no_picture(f, f)),
        },
    }
}

/// The format a file conv cannot turn into its own is turned into instead.
pub fn target_for(f: Format) -> Option<Format> {
    match f {
        Format::Heif => Some(Format::Heic),
        Format::Svg | Format::Ico => Some(Format::Png),
        Format::Avi => Some(Format::Mp4),
        _ => None,
    }
}

fn no_picture(from: Format, to: Format) -> String {
    format!(
        "--rotate does not apply to {} -> {}: it turns images, video and GIFs",
        from.ext(),
        to.ext()
    )
}

/// The refusal of `--rotate` on a pair whose recipe cannot turn anything.
pub(crate) fn not_applicable(from: Format, to: Format) -> crate::ConvError {
    crate::ConvError::new(crate::ErrorCode::InvalidInvocation, no_picture(from, to))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_parses_from_a_word_or_clockwise_degrees() {
        for (text, want) in [
            ("right", Turn::Right),
            ("RIGHT", Turn::Right),
            ("90", Turn::Right),
            ("-270", Turn::Right),
            ("left", Turn::Left),
            ("270", Turn::Left),
            ("-90", Turn::Left),
            ("180", Turn::Half),
            ("-180", Turn::Half),
        ] {
            assert_eq!(parse(text), Ok(want), "{text}");
        }
    }

    /// A phone that thought it was held upright tags a landscape shot 6
    /// (show it a quarter turn right); turning that left is the picture as
    /// stored, so only the tag needs to change.
    #[test]
    fn a_turn_that_undoes_the_note_leaves_the_picture_upright_as_stored() {
        let shown = |tag| Orientation::from_exif(tag).unwrap();
        assert!(shown(6).then(Turn::Left).is_upright());
        assert!(shown(8).then(Turn::Right).is_upright());
        assert!(shown(3).then(Turn::Half).is_upright());
        assert!(!shown(1).then(Turn::Right).is_upright());
        assert!(!shown(6).then(Turn::Right).is_upright());
        // A mirrored picture is never upright after a turn alone.
        for tag in [2, 4, 5, 7] {
            for turn in [Turn::Right, Turn::Half, Turn::Left] {
                assert!(!shown(tag).then(turn).is_upright(), "{tag} {turn:?}");
            }
        }
        assert_eq!(Orientation::from_exif(0), None);
        assert_eq!(Orientation::from_exif(9), None);
    }

    /// The matrices ffmpeg writes for `-display_rotation -90` and `90`, and
    /// the flip, read the way the EXIF tags for them do.
    #[test]
    fn a_display_matrix_reads_as_the_orientation_it_shows() {
        const ONE: i32 = 0x10000;
        assert_eq!(
            Orientation::from_matrix([ONE, 0, 0, ONE]),
            Orientation::from_exif(1)
        );
        assert_eq!(
            Orientation::from_matrix([0, ONE, -ONE, 0]),
            Orientation::from_exif(6)
        );
        assert_eq!(
            Orientation::from_matrix([0, -ONE, ONE, 0]),
            Orientation::from_exif(8)
        );
        assert_eq!(
            Orientation::from_matrix([-ONE, 0, 0, -ONE]),
            Orientation::from_exif(3)
        );
        assert_eq!(
            Orientation::from_matrix([-ONE, 0, 0, ONE]),
            Orientation::from_exif(2)
        );
        // A scaled or skewed picture is no orientation of these.
        assert_eq!(Orientation::from_matrix([2 * ONE, 0, 0, ONE]), None);
        assert_eq!(Orientation::from_matrix([ONE, ONE, 0, ONE]), None);
    }

    #[test]
    fn a_turn_that_is_not_a_quarter_or_turns_nothing_is_refused() {
        for text in ["45", "up", "", "90.0", "450"] {
            let e = parse(text).unwrap_err();
            assert!(e.contains(FORMS), "{text}: {e}");
        }
        for text in ["0", "360", "-360"] {
            let e = parse(text).unwrap_err();
            assert!(e.contains("leaves the picture as it is"), "{text}: {e}");
        }
    }

    /// A JPEG needs nothing else of a probe, so its orientation is kept
    /// even where none ran; a video's only goes on one ffprobe made, so a
    /// missing ffprobe is still told apart from a file it read.
    #[test]
    fn the_orientation_goes_on_a_real_probe_or_a_pictures() {
        use crate::mov::tests::{movie, trak, IDENTITY, RIGHT};
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        std::fs::write(&clip, movie(IDENTITY, &[trak(0, RIGHT, b"vide")])).unwrap();
        let t = crate::Tuning {
            rotate: Some(Turn::Left),
            ..crate::Tuning::default()
        };
        let mut none = None;
        attach(Format::Mp4, &clip, &t, &mut none);
        assert_eq!(none, None);
        let mut probed = Some(crate::MediaProbe {
            video_streams: 1,
            ..crate::MediaProbe::default()
        });
        attach(Format::Mp4, &clip, &t, &mut probed);
        assert_eq!(probed.unwrap().orientation, Orientation::from_exif(6));
        // Without a turn, nothing is read.
        let mut probed = Some(crate::MediaProbe::default());
        attach(Format::Mp4, &clip, &crate::Tuning::default(), &mut probed);
        assert_eq!(probed.unwrap().orientation, None);

        let photo = dir.path().join("photo.jpg");
        let tiff = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1, 0, (tiff.len() + 8) as u8];
        jpeg.extend(b"Exif\0\0");
        jpeg.extend(tiff);
        jpeg.extend([0xFF, 0xDA, 0, 2]);
        std::fs::write(&photo, jpeg).unwrap();
        let mut none = None;
        attach(Format::Jpg, &photo, &t, &mut none);
        assert_eq!(none.unwrap().orientation, Orientation::from_exif(6));
    }
}
