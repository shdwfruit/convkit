//! The part of a source a conversion keeps: `--start`, `--end` and
//! `--duration` as typed, resolved against a probed source into the cut
//! ffmpeg makes, with the notes and file names that go with it. Pure: no
//! process, no filesystem.
//!
//! Times are whole milliseconds, parsed by digit string rather than through
//! a float, for the same reason `probe::parse_duration_ms` is: a float would
//! turn `0.3` into something that does not print back as `0.3`.

use std::path::Path;

use serde::Serialize;

use crate::probe::MediaProbe;
use crate::{registry, ConvError, ErrorCode, Format, Kind};

/// A time as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Time {
    /// Milliseconds from the start of the file, or, when `from_end`, back
    /// from its end.
    pub ms: u64,
    /// Typed with a leading `-`.
    pub from_end: bool,
    /// As typed, for notes, refusals and `--json`.
    pub text: String,
}

/// Where a range stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    /// `--end`.
    At(Time),
    /// `--duration`: a length counted from the start.
    After(Time),
}

/// The part of a source to keep, as typed. `Range::new` returns `None`
/// rather than an empty one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    pub start: Option<Time>,
    pub end: Option<End>,
}

const FORMS: &str = "expected seconds (90, 90.5, 30s), m:ss (1:02.5) or h:mm:ss \
                     (1:02:03), with a leading - to count back from the end";

/// Parses `90`, `90.5`, `30s`, `1:02`, `1:02.5` or `1:02:03.250`, each
/// optionally after a `-` that counts back from the end. The first field is
/// unbounded (`90:00`); the fields after it are under 60. Fraction digits
/// past three round to the nearest millisecond: no frame is that short, and
/// a time pasted from ffprobe's six-digit output should not be refused.
pub fn parse_time(text: &str) -> Result<Time, String> {
    let bad = || format!("invalid time '{text}'; {FORMS}");
    let (from_end, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let fields: Vec<&str> = body.split(':').collect();
    if fields.len() > 3 {
        return Err(bad());
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let (leading, last) = fields.split_at(fields.len() - 1);
    // `30s` is only the plain-seconds form; `1:02s` is a typo, not a unit.
    let last = match last[0].strip_suffix('s') {
        Some(secs) if fields.len() == 1 => secs,
        _ => last[0],
    };
    let (whole, frac) = match last.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (last, None),
    };
    if !digits(whole) || frac.is_some_and(|f| !digits(f)) || !leading.iter().all(|f| digits(f)) {
        return Err(bad());
    }
    let too_large = || format!("invalid time '{text}'; it is too large");
    let value = |s: &str| s.parse::<u64>().map_err(|_| too_large());
    let seconds = value(whole)?;
    if fields.len() > 1 && seconds >= 60 {
        return Err(format!(
            "invalid time '{text}'; seconds after a colon run 0-59"
        ));
    }
    let (hours, minutes) = match leading {
        [] => (0, 0),
        [m] => (0, value(m)?),
        [h, m] => {
            let m = value(m)?;
            if m >= 60 {
                return Err(format!(
                    "invalid time '{text}'; minutes after a colon run 0-59"
                ));
            }
            (value(h)?, m)
        }
        _ => unreachable!("at most three fields"),
    };
    // Tenths of a millisecond from the first four fraction digits, then
    // rounded half up; a carry into the next second falls out of the sum.
    let tenths: u64 = frac.map_or(0, |f| {
        let four: String = f.chars().chain("0000".chars()).take(4).collect();
        four.parse().expect("four ascii digits")
    });
    let ms = hours
        .checked_mul(3_600_000)
        .and_then(|h| minutes.checked_mul(60_000).and_then(|m| h.checked_add(m)))
        .and_then(|hm| seconds.checked_mul(1_000).and_then(|s| hm.checked_add(s)))
        .and_then(|t| t.checked_add((tenths + 5) / 10))
        .ok_or_else(too_large)?;
    Ok(Time {
        ms,
        from_end,
        text: text.to_string(),
    })
}

/// `--duration`: a time that is a length, so neither negative nor zero.
pub fn parse_duration(text: &str) -> Result<Time, String> {
    let t = parse_time(text)?;
    if t.from_end {
        return Err(format!(
            "invalid duration '{text}'; a duration can't be negative \
             (use --end -T to stop T before the end)"
        ));
    }
    if t.ms == 0 {
        return Err(format!(
            "invalid duration '{text}'; it must be longer than zero"
        ));
    }
    Ok(t)
}

/// The fraction of a second, without trailing zeros: `.5`, `.25`, `.001`,
/// or nothing.
fn fraction(ms: u64) -> String {
    match ms % 1000 {
        0 => String::new(),
        f => format!(".{f:03}").trim_end_matches('0').to_string(),
    }
}

/// A time as conv prints it, in the syntax `--start` reads back:
/// `0:05`, `1:02.5`, `1:02:03`.
pub fn format_time(ms: u64) -> String {
    let (h, m, s) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60);
    let frac = fraction(ms);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}{frac}")
    } else {
        format!("{m}:{s:02}{frac}")
    }
}

/// A time as a file name holds it: no colon, which Windows forbids in one.
/// `5s`, `1m02s`, `1m02.5s`, `1h02m03s`.
fn name_time(ms: u64) -> String {
    let (h, m, s) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60);
    let frac = fraction(ms);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}{frac}s")
    } else if m > 0 {
        format!("{m}m{s:02}{frac}s")
    } else {
        format!("{s}{frac}s")
    }
}

impl Range {
    /// The range the three flags ask for, refusing what is wrong before
    /// anything is probed. `None` when no flag was given. Two times with
    /// different signs can only be ordered against the file's length, so
    /// that check waits for `resolve`.
    pub fn new(
        start: Option<Time>,
        end: Option<Time>,
        duration: Option<Time>,
    ) -> Result<Option<Range>, String> {
        let end = match (end, duration) {
            (Some(_), Some(_)) => {
                return Err("--end and --duration both say where the cut stops; give one".into())
            }
            (Some(e), None) => Some(End::At(e)),
            (None, Some(d)) => Some(End::After(d)),
            (None, None) => None,
        };
        if start.is_none() && end.is_none() {
            return Ok(None);
        }
        if let Some(End::At(e)) = &end {
            if !e.from_end && e.ms == 0 {
                return Err("--end 0 leaves nothing to keep".into());
            }
            if let Some(s) = &start {
                let ordered = match (s.from_end, e.from_end) {
                    (false, false) => s.ms < e.ms,
                    (true, true) => s.ms > e.ms,
                    _ => true,
                };
                if !ordered {
                    return Err(format!("--start {} is not before --end {}", s.text, e.text));
                }
            }
        }
        Ok(Some(Range { start, end }))
    }

    /// The first flag given, for a refusal that names one.
    pub fn flag(&self) -> &'static str {
        match (&self.start, &self.end) {
            (Some(_), _) => "--start",
            (None, Some(End::At(_))) => "--end",
            (None, Some(End::After(_))) => "--duration",
            (None, None) => unreachable!("Range::new never builds an empty range"),
        }
    }

    /// Every flag given, as typed: `--start 1:00 --end -30`.
    pub fn words(&self) -> String {
        let mut w = Vec::new();
        if let Some(s) = &self.start {
            w.push(format!("--start {}", s.text));
        }
        match &self.end {
            Some(End::At(e)) => w.push(format!("--end {}", e.text)),
            Some(End::After(d)) => w.push(format!("--duration {}", d.text)),
            None => {}
        }
        w.join(" ")
    }
}

/// What a same-format output adds to its name (`talk-1m02s-1m10s.mp4`),
/// built from the times as typed, so it needs no probe: an absent start is
/// `0s`, an absent end is `end`, a time from the end is `end-30s`, and a
/// start from the end with nothing after it is `last-30s`.
pub fn name_suffix(range: &Range) -> String {
    let label = |t: &Time| match t.from_end {
        true => format!("end-{}", name_time(t.ms)),
        false => name_time(t.ms),
    };
    if let (Some(s), None) = (&range.start, &range.end) {
        if s.from_end {
            return format!("last-{}", name_time(s.ms));
        }
    }
    let start = range.start.as_ref().map_or_else(|| "0s".to_string(), label);
    let end = match (&range.end, &range.start) {
        (None, _) => "end".to_string(),
        (Some(End::At(e)), _) => label(e),
        (Some(End::After(d)), Some(s)) if s.from_end => format!("{}-long", name_time(d.ms)),
        (Some(End::After(d)), Some(s)) => name_time(s.ms + d.ms),
        (Some(End::After(d)), None) => name_time(d.ms),
    };
    format!("{start}-{end}")
}

/// A range resolved against one source: what ffmpeg actually cuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cut {
    pub start_ms: u64,
    /// Where the cut stops, or `None` when it runs to the end of the file,
    /// which needs no `-t` and no length.
    pub end_ms: Option<u64>,
}

/// Whole milliseconds as ffmpeg's seconds: `62`, `62.5`, `7.75`.
fn seconds(ms: u64) -> String {
    format!("{}{}", ms / 1000, fraction(ms))
}

impl Cut {
    /// The clip's length, when it stops before the end of the file.
    pub fn len_ms(&self) -> Option<u64> {
        self.end_ms.map(|end| end - self.start_ms)
    }

    /// The cut as ffmpeg input options, to go before `-i`: `-ss` when it
    /// starts after 0, `-t` when it stops before the end.
    ///
    /// Input options rather than output ones, so that every read of the
    /// input sees the same range: both passes of a two-pass encode, and the
    /// palette pass of a GIF. An input `-ss` also seeks rather than decoding
    /// everything before the start, and with a re-encode it is still exact:
    /// ffmpeg decodes from the keyframe before and drops the frames up to
    /// the start (measured: 90 frames for `-ss 7 -t 3` at 30 fps, on 6.1
    /// and 9.0 alike).
    pub fn input_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self.start_ms > 0 {
            args.extend(["-ss".to_string(), seconds(self.start_ms)]);
        }
        if let Some(len) = self.len_ms() {
            args.extend(["-t".to_string(), seconds(len)]);
        }
        args
    }

    /// The cut as ffmpeg output options, to go before the output path.
    ///
    /// `-map_chapters -1`: ffmpeg moves the source's chapters back by the
    /// start but never ends them at the clip's end, so an 8 s clip of a long
    /// file listed every chapter after it as well. Only an output `-t` makes
    /// ffmpeg end them there, and on 6.1 that drops an mkv clip's last
    /// frame, so a clip leaves the chapters out.
    ///
    /// `-copypriorss:s 0` when it starts after 0: a copied subtitle that
    /// starts before the cut otherwise keeps its place before it, and
    /// matroska then delays every other stream by as much so that no
    /// timestamp is negative. A cue from 1 s to 3 s, cut at 3 s, started the
    /// clip's picture at 2 s (6.1 and 9.0 alike). Re-encoded subtitles are
    /// trimmed by their decoder and need nothing.
    pub fn output_args(&self) -> Vec<String> {
        let mut args = vec!["-map_chapters".to_string(), "-1".to_string()];
        if self.start_ms > 0 {
            args.extend(["-copypriorss:s".to_string(), "0".to_string()]);
        }
        args
    }
}

/// The times as typed, for `--json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Requested {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
}

/// What `--json` says about a range: the cut made, after any clamping, and
/// the times as typed, so a script can see where the two differ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RangeReport {
    pub start_ms: u64,
    /// `null` only for a cut to the end of a file whose length could not be
    /// read.
    pub end_ms: Option<u64>,
    pub requested: Requested,
}

/// A range resolved for one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    /// `None` when the range covers the whole source: nothing is cut.
    pub cut: Option<Cut>,
    /// The probe as everything after the cut should see it: the clip's
    /// length, and the source's size scaled to it. The `--max-size` budget,
    /// the GIF memory note and the `--upscale` estimate all read these.
    pub probe: MediaProbe,
    /// Notes for this source only: a clamped end or start, a range that
    /// covers everything.
    pub notes: Vec<String>,
    pub report: RangeReport,
}

/// Refuses a range on a pair with no timeline, then resolves it. The one
/// entry point both `plan::build_tuned` and `sized::prepare` use, so a cut
/// conversion and a cut-and-sized one agree on every refusal and note.
pub fn clip(
    from: Format,
    to: Format,
    range: &Range,
    probe: Option<&MediaProbe>,
    input: &Path,
) -> crate::Result<Clip> {
    let flag = range.flag();
    if !registry::takes_range(from, to) {
        let still = |f: Format| f.kind() == Kind::Image && f != Format::Gif;
        let why = if still(to) {
            format!("{} is a still image", to.ext())
        } else if still(from) {
            format!("{} is a still image", from.ext())
        } else {
            format!("{} -> {} has no timeline to cut", from.ext(), to.ext())
        };
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "{flag} does not apply to {} -> {}: {why}; {flag} cuts video and audio",
                from.ext(),
                to.ext()
            ),
        ));
    }
    let probe = probe.ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!(
                "{flag} needs ffprobe to read {}; install ffprobe or check that it runs",
                input.display()
            ),
        )
    })?;
    let keeps_video = matches!(
        to,
        Format::Mp4 | Format::Mov | Format::Mkv | Format::Webm | Format::Gif
    );
    resolve(range, probe, input, keeps_video)
}

/// Resolves `range` against the probed source. `keeps_video` is whether
/// the target has frames, so a cut shorter than one is refused there and
/// nowhere else.
pub fn resolve(
    range: &Range,
    probe: &MediaProbe,
    input: &Path,
    keeps_video: bool,
) -> crate::Result<Clip> {
    let Some(src) = probe.duration_ms.filter(|&d| d > 0) else {
        return resolve_without_length(range, probe, input, keeps_video);
    };
    let name = input.display();
    let length = format_time(src);
    let mut notes = Vec::new();
    let start = match &range.start {
        None => 0,
        Some(t) if t.from_end && t.ms > src => {
            notes.push(format!(
                "Source is {length} long, so the cut starts at its beginning \
                 rather than at --start {}.",
                t.text
            ));
            0
        }
        Some(t) if t.from_end => src - t.ms,
        Some(t) if t.ms >= src => {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!(
                    "--start {} is past the end of {name}, which is {length} long",
                    t.text
                ),
            ))
        }
        Some(t) => t.ms,
    };
    let (asked_end, end_words) = match &range.end {
        None => (src, None),
        Some(End::At(t)) if t.from_end => (src.saturating_sub(t.ms), None),
        Some(End::At(t)) => (t.ms, Some(format!("--end {}", t.text))),
        Some(End::After(d)) => (
            start.saturating_add(d.ms),
            Some(format!("--duration {}", d.text)),
        ),
    };
    if asked_end <= start {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "{} leaves nothing of {name}, which is {length} long",
                range.words()
            ),
        ));
    }
    let end = asked_end.min(src);
    if let (true, Some(words)) = (asked_end > src, end_words) {
        notes.push(format!(
            "Source is {length} long, so the cut stops at its end rather than at {words}."
        ));
    }
    let report = RangeReport {
        start_ms: start,
        end_ms: Some(end),
        requested: requested(range),
    };
    if start == 0 && end == src {
        return Ok(Clip {
            cut: None,
            probe: probe.clone(),
            notes: vec![format!(
                "The range covers all of the {length} source, so nothing was cut."
            )],
            report,
        });
    }
    let cut = Cut {
        start_ms: start,
        end_ms: (end < src).then_some(end),
    };
    refuse_less_than_a_frame(start, end, probe, input, keeps_video)?;
    let len = end - start;
    let mut narrowed = probe.clone();
    narrowed.duration_ms = Some(len);
    narrowed.size_bytes = probe.size_bytes.map(|b| {
        u64::try_from(u128::from(b) * u128::from(len) / u128::from(src)).unwrap_or(u64::MAX)
    });
    Ok(Clip {
        cut: Some(cut),
        probe: narrowed,
        notes,
        report,
    })
}

/// The times as typed.
fn requested(range: &Range) -> Requested {
    Requested {
        start: range.start.as_ref().map(|t| t.text.clone()),
        end: match &range.end {
            Some(End::At(t)) => Some(t.text.clone()),
            _ => None,
        },
        duration: match &range.end {
            Some(End::After(t)) => Some(t.text.clone()),
            _ => None,
        },
    }
}

/// Refuses a cut too short to hold a frame, where the target has frames.
/// One frame's length is rounded down: a cut of 33 ms at 30 fps still holds
/// the frame it lands on.
fn refuse_less_than_a_frame(
    start: u64,
    end: u64,
    probe: &MediaProbe,
    input: &Path,
    keeps_video: bool,
) -> crate::Result<()> {
    if let (true, true, Some((n, d))) = (keeps_video, probe.video_streams > 0, probe.frame_rate) {
        let frame_ms = 1000 * u64::from(d) / u64::from(n);
        if end - start < frame_ms {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!(
                    "the cut {}-{} is shorter than one frame of {} ({frame_ms} ms at {} fps)",
                    format_time(start),
                    format_time(end),
                    input.display(),
                    crate::video::show_rate((n, d)),
                ),
            ));
        }
    }
    Ok(())
}

/// A source whose length cannot be read: browser recordings often carry
/// none. `-ss` and `-t` need no length, so a range counted from the start
/// is cut as given, with a note that it was not checked against the end.
/// A time counted back from the end has nothing to count from.
fn resolve_without_length(
    range: &Range,
    probe: &MediaProbe,
    input: &Path,
    keeps_video: bool,
) -> crate::Result<Clip> {
    let from_end = match (&range.start, &range.end) {
        (Some(t), _) if t.from_end => Some(format!("--start {}", t.text)),
        (_, Some(End::At(t))) if t.from_end => Some(format!("--end {}", t.text)),
        _ => None,
    };
    if let Some(words) = from_end {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "cannot read the length of {}, so {words} cannot count back from its end; \
                 give times from the start instead",
                input.display()
            ),
        ));
    }
    let start = range.start.as_ref().map_or(0, |t| t.ms);
    // `Range::new` has already refused an end at or before the start.
    let end = match &range.end {
        None => None,
        Some(End::At(t)) => Some(t.ms),
        Some(End::After(d)) => Some(start.saturating_add(d.ms)),
    };
    let report = RangeReport {
        start_ms: start,
        end_ms: end,
        requested: requested(range),
    };
    if start == 0 && end.is_none() {
        return Ok(Clip {
            cut: None,
            probe: probe.clone(),
            notes: vec!["The range covers the whole source, so nothing was cut.".to_string()],
            report,
        });
    }
    if let Some(end) = end {
        refuse_less_than_a_frame(start, end, probe, input, keeps_video)?;
    }
    let cut = Cut {
        start_ms: start,
        end_ms: end,
    };
    let mut narrowed = probe.clone();
    narrowed.duration_ms = cut.len_ms();
    // The clip's share of a file of unknown length is unknown too.
    narrowed.size_bytes = None;
    Ok(Clip {
        cut: Some(cut),
        probe: narrowed,
        notes: vec![
            "Source length could not be determined; the cut was made as given.".to_string(),
        ],
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(s: &str) -> u64 {
        parse_time(s).unwrap_or_else(|e| panic!("{s}: {e}")).ms
    }

    #[test]
    fn every_form_parses_to_whole_milliseconds() {
        assert_eq!(ms("90"), 90_000);
        assert_eq!(ms("90.5"), 90_500);
        assert_eq!(ms("30s"), 30_000);
        assert_eq!(ms("0"), 0);
        assert_eq!(ms("1:02"), 62_000);
        assert_eq!(ms("1:02.5"), 62_500);
        assert_eq!(ms("1:02:03.250"), 3_723_250);
        assert_eq!(ms("90:00"), 5_400_000, "the leading field is unbounded");
        assert_eq!(ms("0:00.001"), 1);
    }

    #[test]
    fn extra_fraction_digits_round_to_the_millisecond() {
        assert_eq!(ms("1.2344"), 1_234);
        assert_eq!(ms("1.2345"), 1_235);
        assert_eq!(ms("1.99951"), 2_000, "rounding carries into the second");
        assert_eq!(ms("62.123456"), 62_123);
    }

    #[test]
    fn a_leading_minus_counts_back_from_the_end() {
        let t = parse_time("-0:30").unwrap();
        assert_eq!((t.ms, t.from_end, t.text.as_str()), (30_000, true, "-0:30"));
        assert!(!parse_time("30").unwrap().from_end);
    }

    #[test]
    fn malformed_times_are_refused_with_the_forms_that_work() {
        for bad in [
            "",
            "-",
            "abc",
            "1:2:3:4",
            "1.",
            ".5",
            "1:60",
            "1:60:00",
            "1::2",
            "1:-2",
            "1 :02",
            "1e3",
            "30ms",
            "99999999999999999999",
            "1:02s",
        ] {
            let e = parse_time(bad).unwrap_err();
            assert!(e.starts_with("invalid time"), "{bad:?}: {e}");
        }
        assert!(parse_time("1:60").unwrap_err().contains("0-59"));
    }

    #[test]
    fn a_duration_is_positive_and_from_the_start() {
        assert_eq!(parse_duration("8").unwrap().ms, 8_000);
        assert!(parse_duration("-8")
            .unwrap_err()
            .contains("can't be negative"));
        assert!(parse_duration("0")
            .unwrap_err()
            .contains("longer than zero"));
    }

    #[test]
    fn times_print_the_way_they_are_typed() {
        assert_eq!(format_time(5_000), "0:05");
        assert_eq!(format_time(62_500), "1:02.5");
        assert_eq!(format_time(62_250), "1:02.25");
        assert_eq!(format_time(3_723_000), "1:02:03");
        assert_eq!(format_time(3_723_001), "1:02:03.001");
        // What is printed parses back to the same time.
        for t in [0, 1, 999, 59_999, 62_500, 3_599_999, 3_723_250] {
            assert_eq!(ms(&format_time(t)), t, "{t}");
        }
    }

    fn range(start: Option<&str>, end: Option<&str>, duration: Option<&str>) -> Range {
        try_range(start, end, duration).unwrap().unwrap()
    }

    fn try_range(
        start: Option<&str>,
        end: Option<&str>,
        duration: Option<&str>,
    ) -> Result<Option<Range>, String> {
        Range::new(
            start.map(|s| parse_time(s).unwrap()),
            end.map(|s| parse_time(s).unwrap()),
            duration.map(|s| parse_duration(s).unwrap()),
        )
    }

    #[test]
    fn no_flag_is_no_range() {
        assert_eq!(try_range(None, None, None), Ok(None));
    }

    #[test]
    fn an_order_that_cannot_work_is_refused_before_anything_is_probed() {
        let e = try_range(Some("1:10"), Some("1:02"), None).unwrap_err();
        assert_eq!(e, "--start 1:10 is not before --end 1:02");
        assert!(try_range(Some("5"), Some("5"), None).is_err());
        assert!(try_range(Some("-5"), Some("-30"), None).is_err());
        assert!(try_range(None, Some("0"), None).is_err());
        // Mixed signs are only decidable against the length.
        assert!(try_range(Some("1:00"), Some("-30"), None).is_ok());
        assert!(try_range(Some("-30"), Some("-5"), None).is_ok());
    }

    #[test]
    fn end_and_duration_together_are_refused() {
        let e = try_range(None, Some("10"), Some("5")).unwrap_err();
        assert!(e.contains("give one"), "{e}");
    }

    #[test]
    fn the_flag_named_in_refusals_is_the_first_one_given() {
        assert_eq!(range(Some("1"), Some("2"), None).flag(), "--start");
        assert_eq!(range(None, Some("2"), None).flag(), "--end");
        assert_eq!(range(None, None, Some("2")).flag(), "--duration");
        assert_eq!(
            range(Some("1"), None, Some("2")).words(),
            "--start 1 --duration 2"
        );
    }

    #[test]
    fn a_name_suffix_never_holds_a_colon() {
        let cases = [
            (range(Some("1:02"), Some("1:10"), None), "1m02s-1m10s"),
            (range(Some("1:02"), None, None), "1m02s-end"),
            (range(None, Some("30"), None), "0s-30s"),
            (range(Some("-30"), None, None), "last-30s"),
            (range(Some("1:02"), None, Some("8")), "1m02s-1m10s"),
            (range(None, None, Some("8")), "0s-8s"),
            (
                range(Some("1:02.5"), Some("1:02:03"), None),
                "1m02.5s-1h02m03s",
            ),
            (range(None, Some("-5"), None), "0s-end-5s"),
            (range(Some("-30"), None, Some("10")), "end-30s-10s-long"),
        ];
        for (r, want) in cases {
            let got = name_suffix(&r);
            assert_eq!(got, want, "{}", r.words());
            assert!(!got.contains(':'), "{got}");
        }
    }

    fn source(secs: u64) -> MediaProbe {
        MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            frame_rate: Some((30, 1)),
            duration_ms: Some(secs * 1000),
            size_bytes: Some(secs * 100_000),
            ..MediaProbe::default()
        }
    }

    fn resolved(r: &Range, secs: u64) -> crate::Result<Clip> {
        resolve(r, &source(secs), Path::new("talk.mp4"), true)
    }

    #[test]
    fn a_range_inside_the_file_is_cut_where_asked() {
        let c = resolved(&range(Some("1:02"), Some("1:10"), None), 600).unwrap();
        assert_eq!(
            c.cut,
            Some(Cut {
                start_ms: 62_000,
                end_ms: Some(70_000),
            })
        );
        assert!(c.notes.is_empty(), "{:?}", c.notes);
        assert_eq!(c.probe.duration_ms, Some(8_000), "planning sees the clip");
        assert_eq!(c.probe.size_bytes, Some(800_000), "scaled with it");
        assert_eq!((c.report.start_ms, c.report.end_ms), (62_000, Some(70_000)));
        assert_eq!(c.report.requested.start.as_deref(), Some("1:02"));
    }

    #[test]
    fn times_from_the_end_count_back_from_this_files_own_length() {
        let c = resolved(&range(Some("-30"), None, None), 100)
            .unwrap()
            .cut
            .unwrap();
        assert_eq!((c.start_ms, c.end_ms), (70_000, None), "to the end");
        let c = resolved(&range(None, Some("-5"), None), 100)
            .unwrap()
            .cut
            .unwrap();
        assert_eq!((c.start_ms, c.end_ms), (0, Some(95_000)));
        let c = resolved(&range(Some("1:02"), None, Some("8")), 600)
            .unwrap()
            .cut
            .unwrap();
        assert_eq!((c.start_ms, c.end_ms), (62_000, Some(70_000)));
    }

    #[test]
    fn an_end_past_the_file_stops_at_its_end_with_a_note() {
        let c = resolved(&range(Some("1:00"), Some("1:10"), None), 65).unwrap();
        assert_eq!(c.cut.unwrap().end_ms, None, "it runs to the end");
        assert_eq!(
            c.notes,
            ["Source is 1:05 long, so the cut stops at its end rather than at --end 1:10."]
        );
        let c = resolved(&range(Some("1:00"), None, Some("30")), 65).unwrap();
        assert!(
            c.notes[0].ends_with("rather than at --duration 30."),
            "{:?}",
            c.notes
        );
    }

    #[test]
    fn a_start_from_the_end_before_the_beginning_starts_at_it() {
        let c = resolved(&range(Some("-30"), Some("15"), None), 20).unwrap();
        assert_eq!(c.cut.unwrap().start_ms, 0);
        assert_eq!(
            c.notes,
            ["Source is 0:20 long, so the cut starts at its beginning rather than at --start -30."]
        );
    }

    #[test]
    fn a_range_covering_the_whole_file_cuts_nothing_and_says_so() {
        let c = resolved(&range(None, Some("2:00"), None), 65).unwrap();
        assert_eq!(c.cut, None);
        assert_eq!(
            c.notes,
            ["The range covers all of the 1:05 source, so nothing was cut."]
        );
        assert_eq!(c.probe, source(65), "the probe is left whole");
        assert_eq!((c.report.start_ms, c.report.end_ms), (0, Some(65_000)));
    }

    #[test]
    fn a_start_past_the_end_is_refused_naming_the_length() {
        let e = resolved(&range(Some("2:00"), None, None), 65).unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "--start 2:00 is past the end of talk.mp4, which is 1:05 long"
        );
    }

    #[test]
    fn mixed_signs_that_leave_nothing_are_refused() {
        let e = resolved(&range(Some("1:00"), Some("-30"), None), 80).unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "--start 1:00 --end -30 leaves nothing of talk.mp4, which is 1:20 long"
        );
    }

    #[test]
    fn a_cut_shorter_than_a_frame_is_refused_only_where_there_are_frames() {
        let r = range(Some("1"), Some("1.010"), None);
        let e = resolved(&r, 10).unwrap_err();
        assert!(
            e.message.contains("shorter than one frame"),
            "{}",
            e.message
        );
        assert!(e.message.contains("33 ms at 30 fps"), "{}", e.message);
        // The same 10 ms of audio is a real, if short, clip.
        assert!(resolve(&r, &source(10), Path::new("a.mp4"), false).is_ok());
    }

    fn no_length() -> MediaProbe {
        MediaProbe {
            duration_ms: None,
            ..source(10)
        }
    }

    /// Browser recordings often carry no duration. `-ss` and `-t` need none,
    /// so a range counted from the start is cut as given and says so.
    #[test]
    fn a_source_without_a_length_is_still_cut_from_its_start() {
        let c = resolve(
            &range(Some("5"), Some("10"), None),
            &no_length(),
            Path::new("live.webm"),
            true,
        )
        .unwrap();
        assert_eq!(c.cut.unwrap().input_args(), ["-ss", "5", "-t", "5"]);
        assert_eq!(
            c.notes,
            ["Source length could not be determined; the cut was made as given."]
        );
        assert_eq!(c.report.end_ms, Some(10_000));
        assert_eq!(
            c.probe.duration_ms,
            Some(5_000),
            "the clip's length is known"
        );
        assert_eq!(c.probe.size_bytes, None, "its share of the file is not");

        let c = resolve(
            &range(Some("5"), None, None),
            &no_length(),
            Path::new("live.webm"),
            true,
        )
        .unwrap();
        assert_eq!(c.cut.unwrap().input_args(), ["-ss", "5"]);
        assert_eq!(c.report.end_ms, None, "the end is wherever the file ends");
        assert_eq!(c.probe.duration_ms, None);
    }

    #[test]
    fn a_time_from_the_end_of_a_source_without_a_length_says_what_to_give_instead() {
        let e = resolve(
            &range(Some("-30"), None, None),
            &no_length(),
            Path::new("live.webm"),
            true,
        )
        .unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "cannot read the length of live.webm, so --start -30 cannot count back from \
             its end; give times from the start instead"
        );
    }

    #[test]
    fn a_cut_becomes_input_options_only_where_it_cuts() {
        let c = |s, e| {
            Cut {
                start_ms: s,
                end_ms: e,
            }
            .input_args()
        };
        assert_eq!(c(62_000, Some(70_000)), ["-ss", "62", "-t", "8"]);
        assert_eq!(c(62_500, Some(70_250)), ["-ss", "62.5", "-t", "7.75"]);
        assert_eq!(c(0, Some(30_000)), ["-t", "30"], "from 0: no seek");
        assert_eq!(c(62_000, None), ["-ss", "62"], "to the end: no length");
    }

    #[test]
    fn a_cut_leaves_out_the_chapters_and_any_subtitle_from_before_it() {
        let c = |s, e| {
            Cut {
                start_ms: s,
                end_ms: e,
            }
            .output_args()
        };
        assert_eq!(
            c(62_000, Some(70_000)),
            ["-map_chapters", "-1", "-copypriorss:s", "0"]
        );
        assert_eq!(
            c(62_000, None),
            ["-map_chapters", "-1", "-copypriorss:s", "0"]
        );
        assert_eq!(
            c(0, Some(30_000)),
            ["-map_chapters", "-1"],
            "from 0: nothing comes before it"
        );
    }

    #[test]
    fn the_report_serialises_with_only_the_flags_given() {
        let c = resolved(&range(Some("1:02"), None, Some("8")), 600).unwrap();
        assert_eq!(
            serde_json::to_value(&c.report).unwrap(),
            serde_json::json!({"start_ms": 62000, "end_ms": 70000,
                               "requested": {"start": "1:02", "duration": "8"}})
        );
    }
}
