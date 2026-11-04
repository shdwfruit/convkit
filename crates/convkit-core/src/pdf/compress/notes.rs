//! What `--compress` tells the person: every sentence, worded here so the
//! engine only measures.

use crate::size::{self, UnitFamily};

/// Sizes in notes read in decimal units, as `--max-size` takes them.
pub(crate) fn bytes(n: u64) -> String {
    size::display(n, UnitFamily::Decimal)
}

fn images(n: u32) -> String {
    if n == 1 {
        "1 image".to_string()
    } else {
        format!("{n} images")
    }
}

fn dpi(d: f64) -> String {
    format!("{} dpi", d.round() as u64)
}

/// What was done to the images, or `None` when nothing was.
pub(crate) fn summary(downsampled: u32, reencoded: u32, to_dpi: f64) -> Option<String> {
    match (downsampled, reencoded) {
        (0, 0) => None,
        (d, 0) => Some(format!("Downsampled {} to {}.", images(d), dpi(to_dpi))),
        (0, r) => Some(format!(
            "Re-encoded {} as JPEG at the same size.",
            images(r)
        )),
        (d, r) => Some(format!(
            "Downsampled {} to {} and re-encoded {r} more as JPEG.",
            images(d),
            dpi(to_dpi)
        )),
    }
}

/// A result smaller with no image changed.
pub(crate) fn lossless_only() -> String {
    "Rewritten without loss; no image needed changing.".to_string()
}

/// Where a PDF's bytes are, for saying why it is already compact.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Footprint {
    pub file_bytes: u64,
    /// Every image a page lists, soft masks included.
    pub image_bytes: u64,
    pub one_bit_bytes: u64,
    pub jpx_bytes: u64,
    /// The resolution most of the eligible image bytes are drawn at.
    pub typical_dpi: Option<f64>,
}

/// Why `--compress` found little to take out, as a clause.
pub(crate) fn why_compact(f: &Footprint) -> String {
    if f.image_bytes * 2 < f.file_bytes {
        return "this PDF is mostly text and fonts, which conv keeps as they are".to_string();
    }
    let compact = f.one_bit_bytes + f.jpx_bytes;
    if compact * 2 >= f.image_bytes {
        let what = match (f.one_bit_bytes * 4 >= compact, f.jpx_bytes * 4 >= compact) {
            (true, true) => "black-and-white or JPEG 2000",
            (true, false) => "black-and-white",
            _ => "JPEG 2000",
        };
        return format!("its images are {what}, which are already compact");
    }
    match f.typical_dpi {
        Some(d) => format!("its images are about {}, which --compress keeps", dpi(d)),
        None => {
            "conv found nothing it could make smaller without changing how it looks".to_string()
        }
    }
}

/// The next step when the same look saves little: a size measured at a
/// lower resolution, and the target that reaches it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Suggestion {
    pub dpi: f64,
    pub bytes: u64,
    /// As `--max-size` takes it: `25mb`.
    pub spelling: String,
}

fn suggestion_sentence(s: &Suggestion) -> String {
    format!(
        "At {} it would be {}; try --max-size {}.",
        dpi(s.dpi),
        bytes(s.bytes),
        s.spelling
    )
}

/// A target just above `n` bytes that reads well: whole megabytes from
/// 10 MB, tenths below that, and tens of kilobytes under 1 MB. Each rounds
/// up by at most a tenth or so.
pub(crate) fn target_above(n: u64) -> String {
    const KB: u64 = 1_000;
    const MB: u64 = 1_000_000;
    if n >= 10 * MB {
        format!("{}mb", n.div_ceil(MB))
    } else if n >= MB - 10 * KB {
        let tenths = n.div_ceil(MB / 10).max(10);
        match tenths % 10 {
            0 => format!("{}mb", tenths / 10),
            t => format!("{}.{t}mb", tenths / 10),
        }
    } else {
        format!("{}kb", n.div_ceil(10 * KB) * 10)
    }
}

/// The note for a copy of the input: the result was not smaller.
pub(crate) fn already_compact(why: &str, suggestion: Option<&Suggestion>) -> String {
    match suggestion {
        None => format!("Already compact: {why}. The copy is the same as the original."),
        Some(s) => format!(
            "Already compact at the same look: {why}. The copy is the same as the original. {}",
            suggestion_sentence(s)
        ),
    }
}

/// The note for a result only a little smaller, and the next step when
/// there is one.
pub(crate) fn small_saving(percent: u64, why: &str, suggestion: Option<&Suggestion>) -> String {
    let how = match percent {
        0 => "Less than 1% smaller".to_string(),
        p => format!("Only {p}% smaller"),
    };
    match suggestion {
        Some(s) => format!("{how} at the same look: {why}. {}", suggestion_sentence(s)),
        None => format!("{how}: {why}."),
    }
}

/// What a level did to the images, for a `--max-size` result.
pub(crate) fn sized(
    level: &super::Level,
    downsampled: u32,
    reencoded: u32,
    attempts: u32,
) -> String {
    let tries = if attempts == 1 {
        "1 try".to_string()
    } else {
        format!("{attempts} tries")
    };
    if level.n == 0 || downsampled + reencoded == 0 {
        return format!("Sized without changing any image; {tries}.");
    }
    let what = match (downsampled, reencoded) {
        (d, 0) => format!("{} downsampled", images(d)),
        (0, r) => format!("{} re-encoded", images(r)),
        (d, r) => format!("{} downsampled and {r} re-encoded", images(d)),
    };
    format!(
        "Sized with images at up to {}, JPEG quality {}: {what}; {tries}.",
        dpi(level.to_dpi),
        level.quality
    )
}

/// What `--max-size` would try, for `--dry-run`.
pub(crate) fn ladder(
    levels: &[super::Level],
    start: usize,
    extreme: usize,
    target: &str,
) -> String {
    let at = |l: &super::Level| {
        if l.n == 0 {
            "qpdf's lossless rewrite".to_string()
        } else {
            format!("images at {} (quality {})", dpi(l.to_dpi), l.quality)
        }
    };
    let tries: Vec<String> = levels[start..extreme].iter().map(at).collect();
    let asks: Vec<String> = levels[extreme..].iter().map(|l| dpi(l.to_dpi)).collect();
    format!(
        "Would try {}, in that order, and keep the first under {target}; {} would need a yes. \
         Each try is measured.",
        join(&tries),
        join(&asks)
    )
}

fn join(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{}, then {last}", rest.join(", ")),
    }
}

/// A file already under its `--max-size` target.
pub(crate) fn already_under(bytes_now: u64, max: &crate::size::MaxSize) -> String {
    format!(
        "Already {}, under {}; copied as it is.",
        size::display(bytes_now, max.family),
        max.label()
    )
}

/// The question before an extreme level.
pub(crate) fn extreme(name: &str, target: &str, to_dpi: f64) -> String {
    format!(
        "getting {name} under {target} takes its images down to {} or less, where small print \
         may be hard to read",
        dpi(to_dpi)
    )
}

pub(crate) fn extreme_fix(suggested: Option<&str>) -> String {
    match suggested {
        Some(s) => format!("pass --yes to make it anyway, or try --max-size {s}"),
        None => "pass --yes to make it anyway".to_string(),
    }
}

/// After an extreme level, with consent.
pub(crate) fn extreme_done(name: &str, to_dpi: f64) -> String {
    format!(
        "The images in {name} went down to {} to fit; small print may be hard to read.",
        dpi(to_dpi)
    )
}

/// What holds the bytes no level takes out, when that is clear.
pub(crate) fn why_over(f: &Footprint) -> Option<String> {
    if f.image_bytes * 2 < f.file_bytes {
        return Some("most of it is text and fonts, which conv keeps as they are".to_string());
    }
    let compact = f.one_bit_bytes + f.jpx_bytes;
    (compact * 2 >= f.image_bytes).then(|| {
        "most of its images are black-and-white or JPEG 2000, which conv keeps as they are"
            .to_string()
    })
}

/// Still over the target after the last level.
pub(crate) fn over_target(target: &str, smallest: u64, why: Option<&str>) -> String {
    match why {
        Some(why) => format!(
            "Could not get under {target}: the smallest conv could make is {}, and {why}.",
            bytes(smallest)
        ),
        None => format!(
            "Could not get under {target}: the smallest conv could make is {}.",
            bytes(smallest)
        ),
    }
}

pub(crate) fn signed(name: &str) -> String {
    format!("{name} is digitally signed, and the smaller copy won't carry a valid signature")
}

pub(crate) fn signed_fix() -> String {
    "pass --yes to make it anyway".to_string()
}

/// The output as a warning names it: by name, unless it has the input's
/// (a batch's folder), where that would read as the input.
fn copy_name(name: &str, out: &str) -> String {
    if name == out {
        "the smaller copy".to_string()
    } else {
        out.to_string()
    }
}

pub(crate) fn signature_dropped(name: &str, out: &str) -> String {
    let copy = copy_name(name, out);
    let mut s =
        format!("{copy} is not validly signed: the signature on {name} does not carry over.");
    s[..1].make_ascii_uppercase();
    s
}

pub(crate) fn usage_rights_dropped(name: &str, out: &str) -> String {
    format!(
        "{name} carries Adobe Reader usage rights, which {} loses; Acrobat Reader may say \
         the document has changed since it was created.",
        copy_name(name, out)
    )
}

pub(crate) fn repaired(name: &str, out: &str) -> String {
    format!(
        "{name} was damaged and has been repaired; check {}.",
        copy_name(name, out)
    )
}

pub(crate) fn unreadable(name: &str) -> String {
    format!("conv could not read the images in {name}, so it only rewrote the file losslessly.")
}

/// `--dry-run`'s account of the images and what a real run would try.
pub(crate) fn dry_run(
    found: u32,
    image_bytes: u64,
    file_bytes: u64,
    above: u32,
    level: Option<&super::Level>,
) -> String {
    if found == 0 {
        return "No images: only qpdf's lossless rewrite would run.".to_string();
    }
    let share = image_bytes * 100 / file_bytes.max(1);
    let mut s = format!(
        "{} {} {} ({share}% of the file).",
        images(found),
        if found == 1 { "holds" } else { "hold" },
        bytes(image_bytes)
    );
    // `--max-size` says its ladder in a note of its own.
    let Some(l) = level else {
        return s;
    };
    let reencode = format!(
        "high-quality JPEGs and large lossless photos would be re-encoded as JPEG at quality {}",
        l.quality
    );
    if above > 0 {
        s.push_str(&format!(
            " {above} drawn above {} would be downsampled to {}; {reencode}.",
            dpi(l.above_dpi),
            dpi(l.to_dpi)
        ));
    } else {
        s.push_str(&format!(
            " None is drawn above {}; {reencode}.",
            dpi(l.above_dpi)
        ));
    }
    s.push_str(
        " Each change is kept only if it saves space, and the original is copied if the result \
         is not smaller.",
    );
    s
}

/// The skipped images, for `--dry-run`: `3 black-and-white, 1 JPEG 2000`.
pub(crate) fn skipped_note(skipped: &[(super::Skip, u32)]) -> Option<String> {
    use super::Skip;
    if skipped.is_empty() {
        return None;
    }
    let parts: Vec<String> = skipped
        .iter()
        .map(|(why, n)| {
            let what = match why {
                Skip::OneBit => "black-and-white",
                Skip::Jpeg2000 => "JPEG 2000",
                Skip::BitDepth => "not 8-bit",
                Skip::ColorSpace => "in a colour space conv keeps as it is",
                Skip::Filter => "stored in a way conv keeps as it is",
                Skip::Mask => "masked",
                Skip::DecodeArray => "with a decode array",
                Skip::NotDrawn => "never drawn",
                Skip::Unreadable => "unreadable",
            };
            format!("{n} {what}")
        })
        .collect();
    Some(format!("Left as they are: {}.", parts.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warnings_name_the_copy_apart_from_the_input() {
        assert_eq!(
            repaired("a.pdf", "a-compressed.pdf"),
            "a.pdf was damaged and has been repaired; check a-compressed.pdf."
        );
        assert_eq!(
            repaired("a.pdf", "a.pdf"),
            "a.pdf was damaged and has been repaired; check the smaller copy."
        );
        assert_eq!(
            signature_dropped("a.pdf", "a.pdf"),
            "The smaller copy is not validly signed: the signature on a.pdf does not carry over."
        );
        assert!(usage_rights_dropped("f.pdf", "f.pdf").contains("which the smaller copy loses"));
    }

    #[test]
    fn the_summary_counts_what_changed() {
        assert_eq!(summary(0, 0, 150.0), None);
        assert_eq!(
            summary(6, 0, 150.0).unwrap(),
            "Downsampled 6 images to 150 dpi."
        );
        assert_eq!(
            summary(1, 0, 150.0).unwrap(),
            "Downsampled 1 image to 150 dpi."
        );
        assert_eq!(
            summary(0, 3, 150.0).unwrap(),
            "Re-encoded 3 images as JPEG at the same size."
        );
        assert_eq!(
            summary(26, 4, 150.0).unwrap(),
            "Downsampled 26 images to 150 dpi and re-encoded 4 more as JPEG."
        );
    }

    #[test]
    fn why_compact_names_what_holds_the_bytes() {
        let text = Footprint {
            file_bytes: 3_000_000,
            image_bytes: 200_000,
            ..Default::default()
        };
        assert!(why_compact(&text).contains("mostly text and fonts"));
        let bw = Footprint {
            file_bytes: 4_000_000,
            image_bytes: 3_900_000,
            one_bit_bytes: 3_900_000,
            ..Default::default()
        };
        assert_eq!(
            why_compact(&bw),
            "its images are black-and-white, which are already compact"
        );
        let jpx = Footprint {
            jpx_bytes: 3_900_000,
            one_bit_bytes: 0,
            ..bw
        };
        assert!(why_compact(&jpx).contains("JPEG 2000"));
        let scans = Footprint {
            file_bytes: 47_000_000,
            image_bytes: 46_000_000,
            typical_dpi: Some(200.4),
            ..Default::default()
        };
        assert_eq!(
            why_compact(&scans),
            "its images are about 200 dpi, which --compress keeps"
        );
    }

    #[test]
    fn targets_round_up_to_a_size_that_reads_well() {
        assert_eq!(target_above(23_620_000), "24mb");
        assert_eq!(target_above(24_000_000), "24mb");
        assert_eq!(target_above(1_210_000), "1.3mb");
        assert_eq!(target_above(2_000_000), "2mb");
        assert_eq!(target_above(9_950_000), "10mb");
        assert_eq!(target_above(452_000), "460kb");
        assert_eq!(target_above(995_000), "1mb");
        for n in [
            1,
            999,
            450_001,
            1_000_001,
            9_999_999,
            10_000_001,
            123_456_789,
        ] {
            let t = crate::size::parse(&target_above(n)).unwrap();
            assert!(
                t.bytes >= n && t.bytes <= n + n / 9 + 10_000,
                "{n} -> {}",
                t.bytes
            );
        }
    }

    #[test]
    fn the_copy_note_says_it_is_a_copy_and_what_to_try() {
        let s = Suggestion {
            dpi: 150.0,
            bytes: 23_620_000,
            spelling: "24mb".into(),
        };
        assert_eq!(
            already_compact(
                "its images are about 200 dpi, which --compress keeps",
                Some(&s)
            ),
            "Already compact at the same look: its images are about 200 dpi, which --compress \
             keeps. The copy is the same as the original. At 150 dpi it would be 23.62 MB; try \
             --max-size 24mb."
        );
        assert_eq!(
            already_compact("x", None),
            "Already compact: x. The copy is the same as the original."
        );
        assert!(small_saving(4, "x", Some(&s))
            .starts_with("Only 4% smaller at the same look: x. At 150"));
        assert!(small_saving(0, "x", Some(&s)).starts_with("Less than 1% smaller at the same look"));
        assert_eq!(small_saving(2, "x", None), "Only 2% smaller: x.");
    }

    #[test]
    fn the_ladder_note_names_each_try_and_what_needs_a_yes() {
        let n = ladder(&super::super::LEVELS, 0, 4, "5 MB");
        assert_eq!(
            n,
            "Would try qpdf's lossless rewrite, images at 150 dpi (quality 80), images at 150 dpi \
             (quality 65), then images at 120 dpi (quality 60), in that order, and keep the first \
             under 5 MB; 96 dpi, then 72 dpi would need a yes. Each try is measured."
        );
        assert!(ladder(&super::super::LEVELS, 1, 4, "5 MB")
            .starts_with("Would try images at 150 dpi (quality 80),"));
    }

    #[test]
    fn max_size_sentences() {
        let l = super::super::LEVELS[2];
        assert_eq!(
            sized(&l, 26, 4, 3),
            "Sized with images at up to 150 dpi, JPEG quality 65: 26 images downsampled and 4 \
             re-encoded; 3 tries."
        );
        assert_eq!(
            sized(&super::super::LEVELS[0], 0, 0, 1),
            "Sized without changing any image; 1 try."
        );
        let m = crate::size::parse("5mb").unwrap();
        assert_eq!(
            already_under(3_200_000, &m),
            "Already 3.20 MB, under 5 MB; copied as it is."
        );
        assert_eq!(
            extreme("deck.pdf", "1 MB", 96.0),
            "getting deck.pdf under 1 MB takes its images down to 96 dpi or less, where small \
             print may be hard to read"
        );
        assert_eq!(
            extreme_fix(Some("4mb")),
            "pass --yes to make it anyway, or try --max-size 4mb"
        );
        assert_eq!(
            over_target(
                "1 MB",
                2_300_000,
                Some("most of it is text and fonts, which conv keeps as they are")
            ),
            "Could not get under 1 MB: the smallest conv could make is 2.30 MB, and most of it is \
             text and fonts, which conv keeps as they are."
        );
    }

    #[test]
    fn the_dry_run_note_reads_as_a_plan() {
        let l1 = super::super::LEVELS[1];
        let n = dry_run(26, 12_100_000, 17_000_000, 20, Some(&l1));
        assert!(
            n.starts_with("26 images hold 12.10 MB (71% of the file). 20 drawn above 225 dpi"),
            "{n}"
        );
        let n = dry_run(26, 12_100_000, 17_000_000, 0, Some(&l1));
        assert!(
            n.contains("(71% of the file). None is drawn above 225 dpi; high-quality"),
            "{n}"
        );
        assert_eq!(
            dry_run(1, 100, 1000, 0, None),
            "1 image holds 100 B (10% of the file)."
        );
        assert!(dry_run(0, 0, 100, 0, Some(&l1)).starts_with("No images"));
        assert_eq!(
            skipped_note(&[
                (super::super::Skip::OneBit, 3),
                (super::super::Skip::Jpeg2000, 1)
            ])
            .unwrap(),
            "Left as they are: 3 black-and-white, 1 JPEG 2000."
        );
    }
}
