//! Resolving the video knobs against a source.
//!
//! This is pure arithmetic over a `Tuning` and a `MediaProbe`: no argv, no
//! process, no registry table. It lives apart from `media.rs` and
//! `registry.rs` so the cap rules -- the part a user will argue with --
//! can be tested without a backend installed.
//!
//! Both geometry knobs are caps. `--fps 30` on a 24 fps source leaves it at
//! 24; `--resize 1920x1080` on a 640x480 source leaves it at 640x480, as it
//! does on every target `--resize` applies to. Only `--upscale` lets
//! `--resize` enlarge, and the plan warns when it does: enlarging invents no
//! detail, and on video it pays for the invention in every frame. `--crf`
//! is not resolved here at all -- it is an anchor, not a bound, and nothing
//! in the source constrains it.

use crate::probe::MediaProbe;
use crate::recipe::Tuning;

/// The video knobs resolved against a source, as the exact strings that go
/// into a filter chain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedVideo {
    /// The `fps=` value, set only when the cap actually binds. `None` means
    /// "leave the recipe's own default alone", which is why a cap that does
    /// not bind is `None` plus a note rather than the source's own rate: a
    /// filter set to the source's rate would give up a stream copy to
    /// change nothing.
    pub fps: Option<String>,
    /// Set when `--fps` was given and the source is already at or under
    /// it, so `fps` is `None`. A recipe's own authored rate (GIF's 15) must
    /// not apply either: `--fps` replaces that default whether or not it
    /// binds, and the output keeps the source's rate, as the note says.
    pub keep_source_rate: bool,
    /// The whole `scale=...` filter, set when `--resize` was given and the
    /// source might not already fit within it. The clamp is inside the
    /// filter expression rather than decided here, because ffmpeg knows the
    /// true post-autorotation size and convkit only knows what ffprobe
    /// reported. A source ffprobe shows already fits gets no filter: one
    /// would give up a stream copy to change nothing.
    pub scale: Option<String>,
    /// Set when `--resize` was given and the source already fits within
    /// it, so `scale` is `None`. A recipe's own authored width (GIF's 640)
    /// must not apply either: `--resize` replaces that default whether or
    /// not it binds, and the output keeps the source's size.
    pub keep_source_size: bool,
    /// Lines for `Outcome.warnings`, which render.rs prints as `note  {w}`.
    /// Capitalised sentences with a terminal period, per that register.
    pub notes: Vec<String>,
    /// Set when `--upscale` enlarges the picture, or might because the
    /// source size is unknown: the warning `ConversionPlan.enlarged` carries.
    pub enlarged: Option<String>,
}

/// Parses a user frame rate (`24`, `29.97`, `30000/1001`) into a float for
/// comparison only. The string itself, not this number, is what reaches
/// ffmpeg -- so `30000/1001` stays exact in the argv.
fn rate_value(s: &str) -> Option<f64> {
    if let Some((n, d)) = s.split_once('/') {
        let (n, d) = (n.parse::<f64>().ok()?, d.parse::<f64>().ok()?);
        return (d != 0.0).then_some(n / d);
    }
    s.parse::<f64>().ok()
}

/// Formats a probed rational for a note: `24`, `29.97`.
fn show_rate((n, d): (u32, u32)) -> String {
    let v = f64::from(n) / f64::from(d);
    if (v - v.round()).abs() < 1e-6 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v:.2}")
    }
}

/// Translates a `--resize` geometry into one ffmpeg `scale` filter.
///
/// Without `--upscale`, every form clamps against the source with
/// `min(...)`, so the picture is capped, never enlarged. The percentage form
/// needs the clamp most -- it is the only form with no fixed pixel number,
/// so `--resize 200%` would otherwise double the frame. With `--upscale` the
/// clamp is left out and the geometry applies as given.
///
/// The comma inside `min()` is escaped because ffmpeg's filter parser splits
/// an unescaped comma into two filters; `min(640,iw)` fails outright with
/// "No option name near '-2'". No shell is involved, so the backslash is
/// literal.
fn scale_filter(geometry: &str, upscale: bool) -> String {
    let side = |v: &str, source: &str| {
        if upscale {
            v.to_string()
        } else {
            format!(r"min({v}\,{source})")
        }
    };
    if let Some(pct) = geometry.strip_suffix('%') {
        let (w, h) = (format!("iw*{pct}/100"), format!("ih*{pct}/100"));
        return format!("scale=w={}:h={}", side(&w, "iw"), side(&h, "ih"));
    }
    match geometry.split_once('x') {
        Some((w, "")) => format!("scale=w={}:h=-2", side(w, "iw")),
        Some(("", h)) => format!("scale=w=-2:h={}", side(h, "ih")),
        Some((w, h)) => format!(
            "scale=w={}:h={}:force_original_aspect_ratio=decrease",
            side(w, "iw"),
            side(h, "ih")
        ),
        None => format!("scale=w={}:h=-2", side(geometry, "iw")),
    }
}

/// How many times the source's pixels a geometry asks for, aspect kept:
/// for `WxH` the tighter side decides, as `force_original_aspect_ratio=decrease`
/// does. Above 1 means the geometry enlarges. Only compared, and rounded
/// for a warning, so a float is exact enough.
fn pixel_ratio(geometry: &str, (w, h): (u32, u32)) -> f64 {
    let num = |v: &str| v.parse::<f64>().unwrap_or(f64::MAX);
    let (w, h) = (f64::from(w), f64::from(h));
    let side = if let Some(pct) = geometry.strip_suffix('%') {
        num(pct) / 100.0
    } else {
        match geometry.split_once('x') {
            Some((a, "")) => num(a) / w,
            Some(("", b)) => num(b) / h,
            Some((a, b)) => (num(a) / w).min(num(b) / h),
            None => num(geometry) / w,
        }
    };
    side * side
}

/// A pixel ratio for a warning: `9.8` below ten, a whole number above it.
fn show_ratio(r: f64) -> String {
    if r >= 10.0 {
        format!("{}", r.round() as u64)
    } else {
        let s = format!("{r:.1}");
        s.strip_suffix(".0").map(str::to_owned).unwrap_or(s)
    }
}

/// Whether a geometry would leave a source of these displayed dimensions
/// untouched, so the caller can say so rather than leaving the user to
/// wonder why the file looks the same.
///
/// `parse_resize_geometry` (in `conv`'s CLI layer) validates only that the
/// digits are non-empty ASCII, with no length cap, so a value here can
/// overflow `u32::parse` -- `--resize 99999999999` reaches this function
/// still a string. An overflowing value names a number past 4.29 billion,
/// which is larger than any real dimension or any sane percentage, so it
/// can never be the smaller side of a `min()` clamp: treating it as
/// `u32::MAX` (rather than defaulting the comparison's own outcome) keeps
/// the "does it fit" question answered in one place and gets the only
/// correct answer -- it does not bind.
fn geometry_binds(geometry: &str, (w, h): (u32, u32)) -> bool {
    if let Some(pct) = geometry.strip_suffix('%') {
        return pct.parse::<u32>().unwrap_or(u32::MAX) < 100;
    }
    let fits = |v: &str, against: u32| v.parse::<u32>().unwrap_or(u32::MAX) < against;
    match geometry.split_once('x') {
        Some((v, "")) => fits(v, w),
        Some(("", v)) => fits(v, h),
        Some((a, b)) => fits(a, w) || fits(b, h),
        None => fits(geometry, w),
    }
}

/// The largest dimensions a `--resize` geometry allows for a source of
/// these displayed dimensions: aspect preserved, never larger than the
/// source. `--max-size` treats this as a ceiling it may go under. Digit
/// strings too long for a number are unbounded, as in `geometry_binds`.
pub(crate) fn fit_within(geometry: &str, (w, h): (u32, u32)) -> (u32, u32) {
    let num = |v: &str| v.parse::<u128>().unwrap_or(u128::MAX);
    let (w128, h128) = (u128::from(w), u128::from(h));
    let (bw, bh) = if let Some(pct) = geometry.strip_suffix('%') {
        let p = num(pct).min(100);
        ((w128 * p / 100).max(1), (h128 * p / 100).max(1))
    } else {
        match geometry.split_once('x') {
            Some((a, "")) => (num(a), u128::MAX),
            Some(("", b)) => (u128::MAX, num(b)),
            Some((a, b)) => (num(a), num(b)),
            None => (num(geometry), u128::MAX),
        }
    };
    if bw >= w128 && bh >= h128 {
        return (w, h);
    }
    let (nw, nh) = if bw.saturating_mul(h128) <= bh.saturating_mul(w128) {
        (bw, h128 * bw / w128)
    } else {
        (w128 * bh / h128, bh)
    };
    (nw.max(2) as u32, nh.max(2) as u32)
}

/// Parses a `--fps` value (`24`, `29.97`, `30000/1001`) into an exact,
/// reduced rational: `29.97` is `2997/100`, not a float, and `24.0` is
/// `24/1`. Any number of fraction digits is read exactly. `None` when the
/// value is not a positive number or does not fit in a `u32` ratio once
/// reduced, so a caller can refuse it rather than drop the cap.
pub(crate) fn parse_rate(s: &str) -> Option<(u32, u32)> {
    let digits = |t: &str| {
        let all = !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
        all.then(|| t.parse::<u128>().ok()).flatten()
    };
    let (n, d) = if let Some((n, d)) = s.split_once('/') {
        (digits(n)?, digits(d)?)
    } else if let Some((whole, frac)) = s.split_once('.') {
        if frac.is_empty() {
            return None;
        }
        // Trailing zeros add no precision; without them a long run of zeros
        // cannot overflow the scale.
        let frac = frac.trim_end_matches('0');
        let scale = 10u128.checked_pow(u32::try_from(frac.len()).ok()?)?;
        let whole = if whole.is_empty() { 0 } else { digits(whole)? };
        let frac = if frac.is_empty() { 0 } else { digits(frac)? };
        (whole.checked_mul(scale)?.checked_add(frac)?, scale)
    } else {
        (digits(s)?, 1)
    };
    if n == 0 || d == 0 {
        return None;
    }
    let (mut a, mut b) = (n, d);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    Some((u32::try_from(n / a).ok()?, u32::try_from(d / a).ok()?))
}

/// Resolves the video knobs against a source. `gif_width` is the width a GIF
/// target's recipe caps at when no `--resize` is given, so a note about a
/// `--resize` that kept a wider source can say it is wider than that.
pub fn resolve(
    tuning: &Tuning,
    probe: Option<&MediaProbe>,
    gif_width: Option<u32>,
) -> ResolvedVideo {
    let mut r = ResolvedVideo::default();

    if let Some(want) = &tuning.fps {
        match probe.and_then(|p| p.frame_rate) {
            Some(source) => {
                let asked = rate_value(want);
                let have = f64::from(source.0) / f64::from(source.1);
                match asked {
                    Some(a) if a < have => r.fps = Some(want.clone()),
                    _ => {
                        r.keep_source_rate = true;
                        r.notes.push(format!(
                            "Source is {} fps; --fps {want} left it unchanged.",
                            show_rate(source)
                        ));
                    }
                }
            }
            None => {
                // Applied as given: convkit will not silently guess a rate,
                // and it will not silently refuse either.
                r.fps = Some(want.clone());
                r.notes.push(format!(
                    "Source frame rate could not be determined; --fps {want} was applied as given."
                ));
            }
        }
    }

    if let Some(geometry) = &tuning.resize {
        let cost = "enlarging adds no detail, so expect a soft picture and a much larger file.";
        match probe.and_then(|p| p.display_dimensions()) {
            Some((w, h)) if tuning.upscale && pixel_ratio(geometry, (w, h)) > 1.0 => {
                r.scale = Some(scale_filter(geometry, true));
                r.enlarged = Some(format!(
                    "--resize {geometry} --upscale enlarges the {w}x{h} source to about {} \
                     times its pixels: {cost}",
                    show_ratio(pixel_ratio(geometry, (w, h)))
                ));
            }
            Some((w, h)) if !geometry_binds(geometry, (w, h)) => {
                r.keep_source_size = true;
                let wider = gif_width
                    .filter(|&default| w > default)
                    .map(|default| format!(", wider than the GIF default of {default}"))
                    .unwrap_or_default();
                let hint = if tuning.upscale {
                    ""
                } else {
                    " (add --upscale to enlarge)"
                };
                r.notes.push(format!(
                    "Source is {w}x{h}; --resize {geometry} left it unchanged{wider}{hint}."
                ));
            }
            Some(_) => r.scale = Some(scale_filter(geometry, tuning.upscale)),
            None => {
                r.scale = Some(scale_filter(geometry, tuning.upscale));
                if tuning.upscale {
                    r.enlarged = Some(format!(
                        "--resize {geometry} --upscale enlarges any source smaller than that, \
                         and this one's size is not known: {cost}"
                    ));
                }
            }
        }
    }

    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::MediaProbe;

    fn probe_at(w: u32, h: u32, rate: (u32, u32)) -> MediaProbe {
        MediaProbe {
            width: Some(w),
            height: Some(h),
            frame_rate: Some(rate),
            ..MediaProbe::default()
        }
    }

    fn tuning_fps(v: &str) -> Tuning {
        Tuning {
            fps: Some(v.into()),
            ..Default::default()
        }
    }

    fn tuning_resize(v: &str) -> Tuning {
        Tuning {
            resize: Some(v.into()),
            ..Default::default()
        }
    }

    #[test]
    fn a_cap_below_the_source_binds() {
        let r = resolve(
            &tuning_fps("24"),
            Some(&probe_at(1920, 1080, (30, 1))),
            None,
        );
        assert_eq!(r.fps.as_deref(), Some("24"));
        assert!(r.notes.is_empty());
    }

    #[test]
    fn a_cap_above_the_source_does_nothing_and_says_so() {
        let r = resolve(
            &tuning_fps("30"),
            Some(&probe_at(1920, 1080, (24, 1))),
            None,
        );
        assert_eq!(r.fps, None, "a cap must never raise a rate");
        assert_eq!(
            r.notes,
            vec!["Source is 24 fps; --fps 30 left it unchanged.".to_string()]
        );
    }

    #[test]
    fn a_cap_equal_to_the_source_does_nothing() {
        let r = resolve(&tuning_fps("30"), Some(&probe_at(640, 480, (30, 1))), None);
        assert_eq!(r.fps, None);
        assert_eq!(r.notes.len(), 1);
    }

    #[test]
    fn a_fractional_source_rate_is_compared_honestly() {
        // 30000/1001 is 29.97, so --fps 30 does not bind.
        let r = resolve(
            &tuning_fps("30"),
            Some(&probe_at(1920, 1080, (30000, 1001))),
            None,
        );
        assert_eq!(r.fps, None);
        assert_eq!(
            r.notes,
            vec!["Source is 29.97 fps; --fps 30 left it unchanged.".to_string()]
        );
    }

    #[test]
    fn an_unknown_source_rate_applies_the_value_and_admits_it() {
        let r = resolve(&tuning_fps("24"), Some(&MediaProbe::default()), None);
        assert_eq!(r.fps.as_deref(), Some("24"));
        assert_eq!(
            r.notes,
            vec![
                "Source frame rate could not be determined; --fps 24 was applied as given."
                    .to_string()
            ]
        );
    }

    #[test]
    fn no_probe_at_all_behaves_like_an_unknown_rate() {
        let r = resolve(&tuning_fps("24"), None, None);
        assert_eq!(r.fps.as_deref(), Some("24"));
        assert_eq!(r.notes.len(), 1);
    }

    #[test]
    fn every_geometry_form_translates_and_every_one_clamps() {
        let cases = [
            ("640", r"scale=w=min(640\,iw):h=-2"),
            ("640x", r"scale=w=min(640\,iw):h=-2"),
            ("x480", r"scale=w=-2:h=min(480\,ih)"),
            (
                "640x480",
                r"scale=w=min(640\,iw):h=min(480\,ih):force_original_aspect_ratio=decrease",
            ),
            ("50%", r"scale=w=min(iw*50/100\,iw):h=min(ih*50/100\,ih)"),
        ];
        for (geometry, expected) in cases {
            let r = resolve(
                &tuning_resize(geometry),
                Some(&probe_at(1920, 1080, (30, 1))),
                None,
            );
            assert_eq!(r.scale.as_deref(), Some(expected), "geometry {geometry}");
        }
    }

    #[test]
    fn the_percentage_form_clamps_too() {
        // parse_resize_geometry accepts any digit string before '%', so
        // --resize 200% reaches here. With no source size to decide
        // against, the min() is all that stops this one form in five from
        // upscaling.
        let r = resolve(&tuning_resize("200%"), None, None);
        assert_eq!(
            r.scale.as_deref(),
            Some(r"scale=w=min(iw*200/100\,iw):h=min(ih*200/100\,ih)")
        );
    }

    #[test]
    fn a_resize_that_cannot_bind_adds_no_filter_and_says_so() {
        // A filter that changes nothing would still give up the stream
        // copy, which `--fps` already avoids.
        let r = resolve(
            &tuning_resize("1920x1080"),
            Some(&probe_at(640, 480, (30, 1))),
            None,
        );
        assert_eq!(r.scale, None, "a cap that cannot bind must not filter");
        assert!(r.keep_source_size);
        assert_eq!(
            r.notes,
            vec!["Source is 640x480; --resize 1920x1080 left it unchanged (add --upscale to enlarge).".to_string()]
        );
    }

    fn tuning_upscale(v: &str) -> Tuning {
        Tuning {
            resize: Some(v.into()),
            upscale: true,
            ..Default::default()
        }
    }

    const ENLARGE_TAIL: &str =
        "enlarging adds no detail, so expect a soft picture and a much larger file.";

    #[test]
    fn upscale_lets_every_form_enlarge_and_warns_with_the_pixel_ratio() {
        let cases = [
            ("4000x", "scale=w=4000:h=-2", "9.8"),
            ("4000", "scale=w=4000:h=-2", "9.8"),
            ("x900", "scale=w=-2:h=900", "1.6"),
            (
                "4000x3000",
                "scale=w=4000:h=3000:force_original_aspect_ratio=decrease",
                "9.8",
            ),
            ("400%", "scale=w=iw*400/100:h=ih*400/100", "16"),
        ];
        for (geometry, filter, ratio) in cases {
            let r = resolve(
                &tuning_upscale(geometry),
                Some(&probe_at(1280, 720, (30, 1))),
                None,
            );
            assert_eq!(r.scale.as_deref(), Some(filter), "geometry {geometry}");
            assert!(!r.keep_source_size);
            assert_eq!(
                r.enlarged,
                Some(format!(
                    "--resize {geometry} --upscale enlarges the 1280x720 source to about \
                     {ratio} times its pixels: {ENLARGE_TAIL}"
                )),
                "geometry {geometry}"
            );
            assert!(r.notes.is_empty(), "{:?}", r.notes);
        }
    }

    #[test]
    fn upscale_on_a_shrinking_resize_changes_nothing_and_warns_nothing() {
        let r = resolve(
            &tuning_upscale("640x"),
            Some(&probe_at(1920, 1080, (30, 1))),
            None,
        );
        assert_eq!(r.scale.as_deref(), Some("scale=w=640:h=-2"));
        assert_eq!(r.enlarged, None);
        assert!(r.notes.is_empty(), "{:?}", r.notes);
    }

    #[test]
    fn upscale_to_the_source_size_keeps_it_with_no_hint() {
        let r = resolve(
            &tuning_upscale("1280x"),
            Some(&probe_at(1280, 720, (30, 1))),
            None,
        );
        assert_eq!(r.scale, None);
        assert!(r.keep_source_size);
        assert_eq!(r.enlarged, None);
        assert_eq!(
            r.notes,
            vec!["Source is 1280x720; --resize 1280x left it unchanged.".to_string()]
        );
    }

    #[test]
    fn upscale_with_no_source_size_applies_as_given_and_warns_generically() {
        let r = resolve(&tuning_upscale("4000x"), None, None);
        assert_eq!(r.scale.as_deref(), Some("scale=w=4000:h=-2"));
        assert_eq!(
            r.enlarged,
            Some(format!(
                "--resize 4000x --upscale enlarges any source smaller than that, and this \
                 one's size is not known: {ENLARGE_TAIL}"
            ))
        );
    }

    #[test]
    fn a_gif_resize_past_a_source_wider_than_the_default_says_so() {
        let r = resolve(
            &tuning_resize("4000x"),
            Some(&probe_at(1280, 720, (30, 1))),
            Some(640),
        );
        assert_eq!(r.scale, None);
        assert!(r.keep_source_size);
        assert_eq!(
            r.notes,
            vec![
                "Source is 1280x720; --resize 4000x left it unchanged, wider than the GIF \
                 default of 640 (add --upscale to enlarge)."
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_gif_resize_past_a_source_narrower_than_the_default_needs_no_mention_of_it() {
        let r = resolve(
            &tuning_resize("4000x"),
            Some(&probe_at(320, 180, (30, 1))),
            Some(640),
        );
        assert_eq!(
            r.notes,
            vec![
                "Source is 320x180; --resize 4000x left it unchanged (add --upscale to enlarge)."
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_resize_that_binds_does_not_keep_the_source_size() {
        let r = resolve(
            &tuning_resize("640x"),
            Some(&probe_at(1920, 1080, (30, 1))),
            None,
        );
        assert_eq!(r.scale.as_deref(), Some(r"scale=w=min(640\,iw):h=-2"));
        assert!(!r.keep_source_size);
    }

    #[test]
    fn an_asymmetric_wxh_binds_when_either_axis_would_shrink() {
        // force_original_aspect_ratio=decrease is a no-op iff BOTH W>=iw
        // AND H>=ih; geometry_binds negates that with fits(a,w)||fits(b,h),
        // so either axis alone shrinking is enough to bind. Cover both
        // directions against a 1920x1080 source.
        let wide = resolve(
            &tuning_resize("3000x500"),
            Some(&probe_at(1920, 1080, (30, 1))),
            None,
        );
        assert!(
            wide.notes.is_empty(),
            "500 < 1080 should bind: {:?}",
            wide.notes
        );

        let tall = resolve(
            &tuning_resize("500x3000"),
            Some(&probe_at(1920, 1080, (30, 1))),
            None,
        );
        assert!(
            tall.notes.is_empty(),
            "500 < 1920 should bind: {:?}",
            tall.notes
        );
    }

    #[test]
    fn an_overflowing_dimension_cannot_bind() {
        // parse_resize_geometry only checks "non-empty ASCII digits", with
        // no length cap, so a value that overflows u32 reaches
        // geometry_binds as a string. It names a number past 4.29 billion,
        // larger than any real source, so it can never bind -- the note
        // must still fire.
        let r = resolve(
            &tuning_resize("99999999999"),
            Some(&probe_at(640, 480, (30, 1))),
            None,
        );
        assert_eq!(
            r.notes,
            vec!["Source is 640x480; --resize 99999999999 left it unchanged (add --upscale to enlarge).".to_string()]
        );
    }

    #[test]
    fn the_cap_is_decided_against_displayed_dimensions_not_stored_ones() {
        // A portrait clip stored 1280x720 with a 90-degree display matrix
        // arrives at `scale` as 720x1280.
        let p = MediaProbe {
            width: Some(1280),
            height: Some(720),
            rotation: Some(90),
            frame_rate: Some((30, 1)),
            ..MediaProbe::default()
        };
        let r = resolve(&tuning_resize("1000"), Some(&p), None);
        // Displayed width is 720, so a 1000-wide cap does not bind.
        assert_eq!(r.notes.len(), 1, "should report the cap did nothing");
    }

    #[test]
    fn an_empty_tuning_resolves_to_nothing_at_all() {
        let r = resolve(
            &Tuning::default(),
            Some(&probe_at(1920, 1080, (30, 1))),
            None,
        );
        assert_eq!(r, ResolvedVideo::default());
    }

    #[test]
    fn fit_within_preserves_aspect_and_never_grows() {
        assert_eq!(fit_within("1280x720", (1920, 1080)), (1280, 720));
        assert_eq!(fit_within("640x", (1920, 1080)), (640, 360));
        assert_eq!(fit_within("x480", (1920, 1080)), (853, 480));
        assert_eq!(fit_within("50%", (1920, 1080)), (960, 540));
        assert_eq!(fit_within("4000x3000", (1920, 1080)), (1920, 1080));
        assert_eq!(fit_within("1280x720", (1080, 1920)), (405, 720));
        assert_eq!(
            fit_within("99999999999999999999x", (1920, 1080)),
            (1920, 1080)
        );
    }

    #[test]
    fn parse_rate_reduces_and_refuses_what_it_cannot_hold() {
        assert_eq!(parse_rate("24.0"), Some((24, 1)));
        assert_eq!(parse_rate("29.970"), Some((2997, 100)));
        assert_eq!(parse_rate("60/2"), Some((30, 1)));
        assert_eq!(parse_rate("60000/2002"), Some((30000, 1001)));
        assert_eq!(parse_rate(".5"), Some((1, 2)));
        assert_eq!(parse_rate("0.5"), Some((1, 2)));
        // Any number of fraction digits, exactly: 23.976023976 = 2997002997/125000000.
        assert_eq!(
            parse_rate("23.9760239760"),
            Some((2_997_002_997, 125_000_000))
        );
        assert_eq!(
            parse_rate("24.0000000000000000000000000000000000000000000"),
            Some((24, 1))
        );
        assert_eq!(parse_rate("4294967295"), Some((u32::MAX, 1)));
        for refused in [
            "4294967296",
            "99999999999/1",
            "1/99999999999",
            "5.",
            "0.0",
            "0/5",
            "+24",
            "1e3",
            "1.2.3",
            "",
            "/5",
            "24/",
        ] {
            assert_eq!(parse_rate(refused), None, "{refused:?}");
        }
    }

    #[test]
    fn parse_rate_keeps_rates_exact() {
        assert_eq!(parse_rate("24"), Some((24, 1)));
        assert_eq!(parse_rate("29.97"), Some((2997, 100)));
        assert_eq!(parse_rate("30000/1001"), Some((30000, 1001)));
        assert_eq!(parse_rate("0"), None);
        assert_eq!(parse_rate("1/0"), None);
        assert_eq!(parse_rate("abc"), None);
    }
}
