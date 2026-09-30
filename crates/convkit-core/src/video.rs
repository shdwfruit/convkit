//! Resolving the video knobs against a source.
//!
//! This is pure arithmetic over a `Tuning` and a `MediaProbe`: no argv, no
//! process, no registry table. It lives apart from `media.rs` and
//! `registry.rs` so the cap rules -- the part a user will argue with --
//! can be tested without a backend installed.
//!
//! Both geometry knobs are caps. `--fps 30` on a 24 fps source leaves it at
//! 24; `--resize 1920x1080` on a 640x480 source leaves it at 640x480. This
//! diverges from what `--resize` does to an image, where ImageMagick will
//! happily scale a 320x240 up to 1200x900, and the divergence is deliberate:
//! upscaling video invents no detail and pays for the invention in every
//! frame. `--crf` is not resolved here at all -- it is an anchor, not a
//! bound, and nothing in the source constrains it.

use crate::probe::MediaProbe;
use crate::recipe::Tuning;

/// The video knobs resolved against a source, as the exact strings that go
/// into a filter chain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedVideo {
    /// The `fps=` value, set only when the cap actually binds. `None` means
    /// "leave the recipe's own default alone", which is why a cap that does
    /// not bind is `None` plus a note rather than the source's own rate.
    pub fps: Option<String>,
    /// The whole `scale=...` filter, set whenever `--resize` was given. The
    /// clamp is inside the filter expression rather than decided here,
    /// because ffmpeg knows the true post-autorotation size and convkit
    /// only knows what ffprobe reported.
    pub scale: Option<String>,
    /// Lines for `Outcome.warnings`, which render.rs prints as `note  {w}`.
    /// Capitalised sentences with a terminal period, per that register.
    pub notes: Vec<String>,
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
/// Every form clamps against the source with `min(...)`: video is capped,
/// never upscaled. The percentage form needs the clamp most -- it is the
/// only form with no fixed pixel number, so `--resize 200%` would otherwise
/// double the frame.
///
/// The comma inside `min()` is escaped because ffmpeg's filter parser splits
/// an unescaped comma into two filters; `min(640,iw)` fails outright with
/// "No option name near '-2'". No shell is involved, so the backslash is
/// literal.
fn scale_filter(geometry: &str) -> String {
    if let Some(pct) = geometry.strip_suffix('%') {
        return format!(r"scale=w=min(iw*{pct}/100\,iw):h=min(ih*{pct}/100\,ih)");
    }
    match geometry.split_once('x') {
        Some((w, "")) => format!(r"scale=w=min({w}\,iw):h=-2"),
        Some(("", h)) => format!(r"scale=w=-2:h=min({h}\,ih)"),
        Some((w, h)) => {
            format!(r"scale=w=min({w}\,iw):h=min({h}\,ih):force_original_aspect_ratio=decrease")
        }
        None => format!(r"scale=w=min({geometry}\,iw):h=-2"),
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

/// Parses a `--fps` value (`24`, `29.97`, `30000/1001`) into an exact
/// rational: `29.97` is `2997/100`, not a float.
pub(crate) fn parse_rate(s: &str) -> Option<(u32, u32)> {
    let (n, d) = if let Some((n, d)) = s.split_once('/') {
        (n.parse::<u32>().ok()?, d.parse::<u32>().ok()?)
    } else if let Some((whole, frac)) = s.split_once('.') {
        if frac.is_empty() || frac.len() > 6 {
            return None;
        }
        (
            format!("{whole}{frac}").parse::<u32>().ok()?,
            10u32.pow(frac.len() as u32),
        )
    } else {
        (s.parse::<u32>().ok()?, 1)
    };
    (n != 0 && d != 0).then_some((n, d))
}

/// Resolves the video knobs against a source.
pub fn resolve(tuning: &Tuning, probe: Option<&MediaProbe>) -> ResolvedVideo {
    let mut r = ResolvedVideo::default();

    if let Some(want) = &tuning.fps {
        match probe.and_then(|p| p.frame_rate) {
            Some(source) => {
                let asked = rate_value(want);
                let have = f64::from(source.0) / f64::from(source.1);
                match asked {
                    Some(a) if a < have => r.fps = Some(want.clone()),
                    _ => r.notes.push(format!(
                        "Source is {} fps; --fps {want} left it unchanged.",
                        show_rate(source)
                    )),
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
        r.scale = Some(scale_filter(geometry));
        if let Some(dims) = probe.and_then(|p| p.display_dimensions()) {
            if !geometry_binds(geometry, dims) {
                r.notes.push(format!(
                    "Source is {}x{}; --resize {geometry} left it unchanged.",
                    dims.0, dims.1
                ));
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
        let r = resolve(&tuning_fps("24"), Some(&probe_at(1920, 1080, (30, 1))));
        assert_eq!(r.fps.as_deref(), Some("24"));
        assert!(r.notes.is_empty());
    }

    #[test]
    fn a_cap_above_the_source_does_nothing_and_says_so() {
        let r = resolve(&tuning_fps("30"), Some(&probe_at(1920, 1080, (24, 1))));
        assert_eq!(r.fps, None, "a cap must never raise a rate");
        assert_eq!(
            r.notes,
            vec!["Source is 24 fps; --fps 30 left it unchanged.".to_string()]
        );
    }

    #[test]
    fn a_cap_equal_to_the_source_does_nothing() {
        let r = resolve(&tuning_fps("30"), Some(&probe_at(640, 480, (30, 1))));
        assert_eq!(r.fps, None);
        assert_eq!(r.notes.len(), 1);
    }

    #[test]
    fn a_fractional_source_rate_is_compared_honestly() {
        // 30000/1001 is 29.97, so --fps 30 does not bind.
        let r = resolve(
            &tuning_fps("30"),
            Some(&probe_at(1920, 1080, (30000, 1001))),
        );
        assert_eq!(r.fps, None);
        assert_eq!(
            r.notes,
            vec!["Source is 29.97 fps; --fps 30 left it unchanged.".to_string()]
        );
    }

    #[test]
    fn an_unknown_source_rate_applies_the_value_and_admits_it() {
        let r = resolve(&tuning_fps("24"), Some(&MediaProbe::default()));
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
        let r = resolve(&tuning_fps("24"), None);
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
            );
            assert_eq!(r.scale.as_deref(), Some(expected), "geometry {geometry}");
        }
    }

    #[test]
    fn the_percentage_form_clamps_too() {
        // parse_resize_geometry accepts any digit string before '%', so
        // --resize 200% reaches here. Without the min() this is the one
        // form in five that upscales -- the operation the design refuses.
        let r = resolve(&tuning_resize("200%"), Some(&probe_at(640, 480, (30, 1))));
        assert_eq!(
            r.scale.as_deref(),
            Some(r"scale=w=min(iw*200/100\,iw):h=min(ih*200/100\,ih)")
        );
    }

    #[test]
    fn a_resize_that_cannot_bind_says_so() {
        let r = resolve(
            &tuning_resize("1920x1080"),
            Some(&probe_at(640, 480, (30, 1))),
        );
        assert_eq!(
            r.notes,
            vec!["Source is 640x480; --resize 1920x1080 left it unchanged.".to_string()]
        );
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
        );
        assert!(
            wide.notes.is_empty(),
            "500 < 1080 should bind: {:?}",
            wide.notes
        );

        let tall = resolve(
            &tuning_resize("500x3000"),
            Some(&probe_at(1920, 1080, (30, 1))),
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
        );
        assert_eq!(
            r.notes,
            vec!["Source is 640x480; --resize 99999999999 left it unchanged.".to_string()]
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
        let r = resolve(&tuning_resize("1000"), Some(&p));
        // Displayed width is 720, so a 1000-wide cap does not bind.
        assert_eq!(r.notes.len(), 1, "should report the cap did nothing");
    }

    #[test]
    fn an_empty_tuning_resolves_to_nothing_at_all() {
        let r = resolve(&Tuning::default(), Some(&probe_at(1920, 1080, (30, 1))));
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
    fn parse_rate_keeps_rates_exact() {
        assert_eq!(parse_rate("24"), Some((24, 1)));
        assert_eq!(parse_rate("29.97"), Some((2997, 100)));
        assert_eq!(parse_rate("30000/1001"), Some((30000, 1001)));
        assert_eq!(parse_rate("0"), None);
        assert_eq!(parse_rate("1/0"), None);
        assert_eq!(parse_rate("abc"), None);
    }
}
