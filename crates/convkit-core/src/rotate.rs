//! `--rotate` and `--flip`: a quarter, half or three-quarter turn,
//! clockwise, and a mirror left to right or top to bottom, of the picture as
//! it is shown now. Given both, the turn comes first and the flip second, so
//! `horizontal` always means left and right as the result is shown.
//!
//! Whatever the change, the result's note on how to show it (a JPEG's EXIF
//! orientation tag, an mp4 or mov track's display matrix, any XMP copy)
//! says to show the picture as it is stored, so every app shows it the same
//! way. No change ever writes any other note.
//!
//! Mostly the pixels are turned or flipped: ImageMagick's `-rotate`,
//! `-flop` and `-flip` after `-auto-orient`, or ffmpeg's `transpose`,
//! `hflip` and `vflip` in a re-encode, after its autorotation has applied
//! the source's note. Then any XMP copy of the old
//! orientation, which ImageMagick carries over, is reset (`xmp`). Where the
//! source's note was what made it sideways or mirrored, as in a video filmed
//! with the rotation locked, the change brings it back to its stored
//! picture: a JPEG
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

/// A flip as typed: `horizontal` or `vertical`, or `h` or `v`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flip {
    /// Left and right swap, as in a mirror.
    Horizontal,
    /// Top and bottom swap.
    Vertical,
}

const FLIP_FORMS: &str = "expected horizontal or vertical (h or v)";

/// Parses `horizontal`, `vertical`, `h` or `v`.
pub fn parse_flip(text: &str) -> Result<Flip, String> {
    match text.to_ascii_lowercase().as_str() {
        "horizontal" | "h" => Ok(Flip::Horizontal),
        "vertical" | "v" => Ok(Flip::Vertical),
        _ => Err(format!("invalid flip '{text}'; {FLIP_FORMS}")),
    }
}

impl Flip {
    /// The flip as typed back: `horizontal`, `vertical`.
    pub fn text(&self) -> &'static str {
        match self {
            Flip::Horizontal => "horizontal",
            Flip::Vertical => "vertical",
        }
    }

    /// The ffmpeg filter that flips the picture as it is shown.
    fn filter(&self) -> &'static str {
        match self {
            Flip::Horizontal => "hflip",
            Flip::Vertical => "vflip",
        }
    }

    /// ImageMagick's operator: `-flop` mirrors left and right, `-flip` top
    /// and bottom.
    fn operator(&self) -> &'static str {
        match self {
            Flip::Horizontal => "-flop",
            Flip::Vertical => "-flip",
        }
    }

    fn other(&self) -> Flip {
        match self {
            Flip::Horizontal => Flip::Vertical,
            Flip::Vertical => Flip::Horizontal,
        }
    }
}

impl Serialize for Flip {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.text())
    }
}

/// What `--rotate` and `--flip` ask of the picture as it is shown: the
/// turn, then the flip. At least one of them is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub turn: Option<Turn>,
    pub flip: Option<Flip>,
}

impl Change {
    /// The change a tuning asks for, if any.
    pub fn of(tuning: &crate::Tuning) -> Option<Change> {
        (tuning.rotate.is_some() || tuning.flip.is_some()).then_some(Change {
            turn: tuning.rotate,
            flip: tuning.flip,
        })
    }

    /// Whether it swaps the picture's width and height.
    pub fn is_quarter(&self) -> bool {
        self.turn.is_some_and(|t| t.is_quarter())
    }

    /// The flag it was asked with, for messages: `--rotate` when a turn was
    /// asked for, `--flip` otherwise.
    pub fn flag(&self) -> &'static str {
        match self.turn {
            Some(_) => "--rotate",
            None => "--flip",
        }
    }

    /// What it does, as a verb: `turn`, `flip`, or `turn and flip`.
    pub fn verb(&self) -> &'static str {
        match (self.turn, self.flip) {
            (Some(_), Some(_)) => "turn and flip",
            (None, Some(_)) => "flip",
            _ => "turn",
        }
    }

    /// The same, said of it: `turns`, `flips`, `turns and flips`.
    fn does(&self) -> &'static str {
        match (self.turn, self.flip) {
            (Some(_), Some(_)) => "turns and flips",
            (None, Some(_)) => "flips",
            _ => "turns",
        }
    }

    /// What it was asked as: the turn, then the flip, with a half turn and
    /// a flip made as the one other flip they come to.
    fn steps(&self) -> (Option<Turn>, Option<Flip>) {
        match (self.turn, self.flip) {
            (Some(Turn::Half), Some(f)) => (None, Some(f.other())),
            other => other,
        }
    }

    /// The ffmpeg filters that make it, joined by commas. ffmpeg has
    /// already applied any note the source had (its autorotation), so these
    /// change the picture as it is shown.
    pub(crate) fn filter(&self) -> String {
        let (turn, flip) = self.steps();
        [turn.map(|t| t.filter()), flip.map(|f| f.filter())]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(",")
    }

    /// ImageMagick's arguments for it: `-rotate <degrees>`, then `-flop` or
    /// `-flip`.
    pub(crate) fn magick_args(&self) -> Vec<&'static str> {
        let (turn, flip) = self.steps();
        let mut args = Vec::new();
        if let Some(t) = turn {
            args.extend(["-rotate", t.degrees()]);
        }
        if let Some(f) = flip {
            args.push(f.operator());
        }
        args
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

    /// Whether the stored picture is mirrored left to right before it is
    /// turned.
    pub fn mirrored(&self) -> bool {
        self.mirrored
    }

    /// How far it is then turned clockwise: 0, 90, 180 or 270 degrees.
    pub fn clockwise(&self) -> u16 {
        self.clockwise
    }

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

    /// This, then `flip`. A mirror after a turn is the mirror first and the
    /// turn the other way, and a vertical flip is a horizontal one turned
    /// a half.
    pub fn flipped(self, flip: Flip) -> Orientation {
        let back = (360 - self.clockwise) % 360;
        Orientation {
            mirrored: !self.mirrored,
            clockwise: match flip {
                Flip::Horizontal => back,
                Flip::Vertical => (back + 180) % 360,
            },
        }
    }

    /// This, then `change`: its turn, then its flip.
    pub fn after(self, change: Change) -> Orientation {
        let turned = change.turn.map_or(self, |t| self.then(t));
        change.flip.map_or(turned, |f| turned.flipped(f))
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

/// How a file was turned or flipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum By {
    /// The picture was already stored the way the change shows it, so only
    /// its orientation tag or display matrix was reset; nothing was
    /// re-encoded.
    Tag,
    /// The picture itself was turned or flipped, and its tag says it is
    /// shown as stored.
    Pixels,
}

/// What a `--rotate` or `--flip` conversion did, for `--json` and the
/// executor, which resets the tag of a file changed `By::Tag`. `turn` and
/// `flip` are each left out when not asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RotationReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<Turn>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flip: Option<Flip>,
    pub by: By,
}

impl RotationReport {
    pub fn new(change: Change, by: By) -> RotationReport {
        RotationReport {
            turn: change.turn,
            flip: change.flip,
            by,
        }
    }
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
/// of it before planning, for a turn or a flip. A picture's goes on a probe made for
/// it if none ran, since nothing else of one is needed; a video's only on
/// one ffprobe made, so that a missing ffprobe still reads as one.
pub fn attach(
    from: Format,
    input: &std::path::Path,
    tuning: &crate::Tuning,
    probed: &mut Option<crate::MediaProbe>,
) {
    if Change::of(tuning).is_none() {
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
        other => unreachable!("a change by the tag planned for {other:?}"),
    }
}

/// Whether `change` brings a source shown as `probe` read it back to its
/// stored picture, so that resetting its note makes it: a JPEG tagged 6
/// turned left, one tagged 2 flipped horizontally, a video filmed with the
/// rotation locked.
pub(crate) fn undoes_note(change: Change, probe: Option<&crate::MediaProbe>) -> bool {
    probe
        .and_then(|p| p.orientation)
        .is_some_and(|o| o.after(change).is_upright())
}

/// The probe as the changed picture is shown: a quarter turn swaps the
/// width and height every cap and estimate is decided against. A flip
/// swaps nothing.
pub(crate) fn turned_probe(change: Change, probe: &crate::MediaProbe) -> crate::MediaProbe {
    let clockwise = change.turn.map_or(0, |t| t.clockwise());
    let rotation = probe.rotation.unwrap_or(0) - i32::from(clockwise);
    crate::MediaProbe {
        rotation: Some(rotation),
        ..probe.clone()
    }
}

/// The note on a file turned or flipped by its tag alone.
pub(crate) fn tag_note(change: Change) -> String {
    let way = match change.flip {
        Some(_) => "this way round",
        None => "this way up",
    };
    format!(
        "The picture was already stored {way}, so only its orientation tag changed; nothing \
         was re-encoded."
    )
}

/// The note on a video turned or flipped by re-encoding it.
pub(crate) fn video_note(change: Change) -> String {
    format!("The video is re-encoded to {} it.", change.verb())
}

/// The note on a lossy image turned or flipped into its own format.
pub(crate) fn image_note(to: Format, tuning: &crate::Tuning, change: Change) -> String {
    format!(
        "The {} is re-encoded at quality {} to {} it.",
        to.ext(),
        crate::metadata::quality_for(to, tuning),
        change.verb()
    )
}

/// Whether a file of `f` can be turned or flipped into its own format by
/// `plan::in_place`; `Err` says what to do instead. mkv and webm are
/// changed by the media paths, and are not asked.
pub fn in_place(f: Format, change: Change) -> Result<(), String> {
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
                "conv cannot keep {0} files as {0}; add --to {1} to {2} them",
                f.ext(),
                to.ext(),
                change.verb()
            )),
            None => Err(no_picture(f, f, change)),
        },
    }
}

/// The format a file conv cannot turn or flip into its own is changed into
/// instead.
pub fn target_for(f: Format) -> Option<Format> {
    match f {
        Format::Heif => Some(Format::Heic),
        Format::Svg | Format::Ico => Some(Format::Png),
        Format::Avi => Some(Format::Mp4),
        _ => None,
    }
}

fn no_picture(from: Format, to: Format, change: Change) -> String {
    format!(
        "{} does not apply to {} -> {}: it {} images, video and GIFs",
        change.flag(),
        from.ext(),
        to.ext(),
        change.does()
    )
}

/// The refusal of `--rotate` or `--flip` on a pair whose recipe cannot turn
/// anything.
pub(crate) fn not_applicable(from: Format, to: Format, change: Change) -> crate::ConvError {
    crate::ConvError::new(
        crate::ErrorCode::InvalidInvocation,
        no_picture(from, to, change),
    )
}

/// The refusal of a change to a video file with no video stream.
pub(crate) fn no_video_stream(change: Change, input: &std::path::Path) -> crate::ConvError {
    crate::ConvError::new(
        crate::ErrorCode::InvalidInvocation,
        format!(
            "{} {} a picture, and {} has no video stream",
            change.flag(),
            change.does(),
            input.display()
        ),
    )
}

/// The refusal of anything else asked of a gif kept as a gif.
pub(crate) fn gif_alone(change: Change) -> crate::ConvError {
    crate::ConvError::new(
        crate::ErrorCode::InvalidInvocation,
        format!(
            "{} on a gif kept as gif {} its frames and changes nothing else; add --to mp4 \
             to change it as well",
            change.flag(),
            change.does()
        ),
    )
}

/// The refusal of a change into webm without the probe its transcode needs.
pub(crate) fn webm_needs_probe(from: Format, change: Change) -> crate::ConvError {
    crate::ConvError::new(
        crate::ErrorCode::ConversionFailed,
        format!(
            "{} on {} -> webm needs ffprobe to read the source, and it could not; check that \
             ffprobe is installed and the input is a readable video",
            change.flag(),
            from.ext()
        ),
    )
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

    /// A 2x3 picture of distinct cells, so every one of the eight
    /// orientations moves it differently.
    type Grid = Vec<Vec<u8>>;

    fn picture() -> Grid {
        vec![vec![1, 2, 3], vec![4, 5, 6]]
    }

    fn clockwise(g: &Grid) -> Grid {
        let (h, w) = (g.len(), g[0].len());
        (0..w)
            .map(|x| (0..h).rev().map(|y| g[y][x]).collect())
            .collect()
    }

    fn mirror(g: &Grid) -> Grid {
        g.iter()
            .map(|row| row.iter().rev().copied().collect())
            .collect()
    }

    fn upside_down(g: &Grid) -> Grid {
        g.iter().rev().cloned().collect()
    }

    fn transpose(g: &Grid) -> Grid {
        let (h, w) = (g.len(), g[0].len());
        (0..w).map(|x| (0..h).map(|y| g[y][x]).collect()).collect()
    }

    fn times(g: &Grid, n: u16, f: fn(&Grid) -> Grid) -> Grid {
        (0..n).fold(g.clone(), |g, _| f(&g))
    }

    /// The stored picture as an EXIF tag says to show it, by the
    /// standard's own words for each value, not by `Orientation`.
    fn exif_shown(tag: u16, g: &Grid) -> Grid {
        match tag {
            1 => g.clone(),
            2 => mirror(g),
            3 => times(g, 2, clockwise),
            4 => upside_down(g),
            5 => transpose(g),
            6 => clockwise(g),
            7 => times(&transpose(g), 2, clockwise),
            8 => times(g, 3, clockwise),
            _ => unreachable!(),
        }
    }

    /// How `o` shows a stored picture: the mirror, then the turn.
    fn shown_by(o: Orientation, g: &Grid) -> Grid {
        let g = if o.mirrored { mirror(g) } else { g.clone() };
        times(&g, o.clockwise / 90, clockwise)
    }

    /// What the user sees after the change, done on the picture as shown:
    /// the turn, then the flip.
    fn changed(change: Change, g: &Grid) -> Grid {
        let g = times(g, change.turn.map_or(0, |t| t.clockwise()) / 90, clockwise);
        match change.flip {
            Some(Flip::Horizontal) => mirror(&g),
            Some(Flip::Vertical) => upside_down(&g),
            None => g,
        }
    }

    fn every_change() -> Vec<Change> {
        let turns = [None, Some(Turn::Right), Some(Turn::Half), Some(Turn::Left)];
        let flips = [None, Some(Flip::Horizontal), Some(Flip::Vertical)];
        turns
            .into_iter()
            .flat_map(|turn| flips.map(|flip| Change { turn, flip }))
            .filter(|c| c.turn.is_some() || c.flip.is_some())
            .collect()
    }

    #[test]
    fn each_exif_tag_reads_as_the_orientation_the_standard_gives_it() {
        for tag in 1..=8 {
            let o = Orientation::from_exif(tag).unwrap();
            assert_eq!(
                shown_by(o, &picture()),
                exif_shown(tag, &picture()),
                "{tag}"
            );
        }
    }

    /// Every stored orientation, then every turn and flip, in both orders
    /// a user could mean: the composed orientation shows the stored picture
    /// exactly as the source's note and the change, done one after the
    /// other, do.
    #[test]
    fn a_change_composes_with_every_stored_orientation() {
        for tag in 1..=8 {
            let source = Orientation::from_exif(tag).unwrap();
            for change in every_change() {
                let want = changed(change, &exif_shown(tag, &picture()));
                let got = source.after(change);
                assert_eq!(shown_by(got, &picture()), want, "{tag} {change:?}");
                // Upright exactly when the change undoes the note.
                assert_eq!(got.is_upright(), want == picture(), "{tag} {change:?}");
            }
        }
    }

    /// Which stored orientations a change brings back to the picture as
    /// stored, so that only the tag changes: the mirrored tags flip back.
    #[test]
    fn a_flip_undoes_a_mirrored_tag() {
        let undoes = |tag, turn, flip| {
            Orientation::from_exif(tag)
                .unwrap()
                .after(Change { turn, flip })
                .is_upright()
        };
        assert!(undoes(2, None, Some(Flip::Horizontal)));
        assert!(undoes(4, None, Some(Flip::Vertical)));
        assert!(!undoes(2, None, Some(Flip::Vertical)));
        assert!(!undoes(1, None, Some(Flip::Horizontal)));
        // 5 shows the stored picture mirrored across its diagonal: a
        // quarter turn right and a mirror put it back.
        assert!(undoes(5, Some(Turn::Right), Some(Flip::Horizontal)));
        assert!(undoes(7, Some(Turn::Left), Some(Flip::Horizontal)));
        assert!(undoes(7, Some(Turn::Right), Some(Flip::Vertical)));
        // A flip of an unmirrored tag never does.
        for tag in [1, 3, 6, 8] {
            for change in every_change().into_iter().filter(|c| c.flip.is_some()) {
                assert!(!Orientation::from_exif(tag)
                    .unwrap()
                    .after(change)
                    .is_upright());
            }
        }
    }

    /// `--rotate right --flip h`: the turn first, then left and right swap
    /// as the turned picture is shown. The top-left cell stays top left,
    /// and the picture is its own transpose.
    #[test]
    fn a_turn_and_a_flip_turn_first() {
        let change = Change {
            turn: Some(Turn::Right),
            flip: Some(Flip::Horizontal),
        };
        assert_eq!(changed(change, &picture()), transpose(&picture()));
        assert_eq!(
            Orientation::UPRIGHT.after(change),
            Orientation::from_exif(5).unwrap()
        );
    }

    /// The filters and operators that make a change do what the change
    /// says, checked on the grid: ffmpeg's `hflip`/`vflip`/`transpose` and
    /// ImageMagick's `-flop`/`-flip`/`-rotate` as each is documented.
    #[test]
    fn a_change_renders_as_the_filters_and_operators_that_make_it() {
        fn by_filters(filter: &str, g: &Grid) -> Grid {
            filter.split(',').fold(g.clone(), |g, f| match f {
                "hflip" => mirror(&g),
                "vflip" => upside_down(&g),
                "transpose=clock" => clockwise(&g),
                "transpose=cclock" => times(&g, 3, clockwise),
                other => panic!("{other}"),
            })
        }
        fn by_operators(args: &[&str], g: &Grid) -> Grid {
            let mut g = g.clone();
            let mut it = args.iter();
            while let Some(a) = it.next() {
                g = match *a {
                    "-flop" => mirror(&g),
                    "-flip" => upside_down(&g),
                    "-rotate" => {
                        let degrees: u16 = it.next().unwrap().parse().unwrap();
                        times(&g, degrees / 90, clockwise)
                    }
                    other => panic!("{other}"),
                };
            }
            g
        }
        for change in every_change() {
            let want = changed(change, &picture());
            assert_eq!(by_filters(&change.filter(), &picture()), want, "{change:?}");
            assert_eq!(
                by_operators(&change.magick_args(), &picture()),
                want,
                "{change:?}"
            );
        }
        // A half turn and a flip are the other flip alone.
        let half_h = Change {
            turn: Some(Turn::Half),
            flip: Some(Flip::Horizontal),
        };
        assert_eq!(half_h.filter(), "vflip");
        assert_eq!(half_h.magick_args(), ["-flip"]);
        let right_v = Change {
            turn: Some(Turn::Right),
            flip: Some(Flip::Vertical),
        };
        assert_eq!(right_v.filter(), "transpose=clock,vflip");
        assert_eq!(right_v.magick_args(), ["-rotate", "90", "-flip"]);
    }

    #[test]
    fn a_flip_parses_from_a_word_or_its_letter() {
        for (text, want) in [
            ("horizontal", Flip::Horizontal),
            ("Horizontal", Flip::Horizontal),
            ("h", Flip::Horizontal),
            ("vertical", Flip::Vertical),
            ("V", Flip::Vertical),
        ] {
            assert_eq!(parse_flip(text), Ok(want), "{text}");
        }
        for text in ["", "x", "both", "hv", "horiz"] {
            let e = parse_flip(text).unwrap_err();
            assert!(e.contains(FLIP_FORMS), "{text}: {e}");
        }
    }

    #[test]
    fn a_change_names_itself_by_what_was_asked() {
        let flip = Change {
            turn: None,
            flip: Some(Flip::Vertical),
        };
        assert_eq!((flip.flag(), flip.verb()), ("--flip", "flip"));
        let both = Change {
            turn: Some(Turn::Left),
            ..flip
        };
        assert_eq!((both.flag(), both.verb()), ("--rotate", "turn and flip"));
        assert!(both.is_quarter() && !flip.is_quarter());
        let report = serde_json::to_value(RotationReport::new(both, By::Pixels)).unwrap();
        assert_eq!(
            report,
            serde_json::json!({"turn": "left", "flip": "vertical", "by": "pixels"})
        );
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
