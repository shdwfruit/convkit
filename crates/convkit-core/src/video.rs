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

use serde::Serialize;

use crate::probe::MediaProbe;
use crate::recipe::Tuning;

/// What kind of picture the target makes: the notes and the size estimate a
/// `--upscale` warning gives depend on it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    /// A still image. `bytes_per_pixel` is the range an enlarged picture
    /// came out at in the target format, measured; `None` when there is no
    /// figure for it.
    Image { bytes_per_pixel: Option<(f64, f64)> },
    /// A libx264 or VP9 encode, at an even size.
    Video,
    /// A GIF, which caps its width and frame rate unless told otherwise.
    Gif {
        default_width: u32,
        default_fps: u32,
    },
}

/// What a `--resize --upscale` that enlarges the picture costs: the warning
/// printed, the facts behind it for `--json`, and whether the run asks
/// first.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Enlargement {
    /// The warning, as printed.
    pub warning: String,
    /// The output's pixels over the source's. Absent when neither size is
    /// known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pixel_ratio: Option<f64>,
    /// The source's displayed size, `[width, height]`, when it was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<[u32; 2]>,
    /// The enlarged size, `[width, height]`, when it can be worked out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<[u32; 2]>,
    /// A rough output size, `[low, high]` bytes, from a rule of thumb.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_bytes: Option<[u64; 2]>,
    /// More than four times the source's pixels: the run asks first.
    pub needs_confirmation: bool,
}

// `pixel_ratio` is never NaN, so equality is reflexive.
impl Eq for Enlargement {}

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
    /// source size is unknown.
    pub enlarged: Option<Enlargement>,
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

/// The size a geometry gives a source of these displayed dimensions when
/// it may enlarge, rounded as the target's scaler rounds: a side ffmpeg
/// derives with `-2` to the nearest even number, and a video encode's whole
/// frame down to even, as the guard every libx264 transcode ends with does.
/// For `WxH` the tighter side decides, as `force_original_aspect_ratio=decrease`
/// and ImageMagick's fit-within both do.
fn scaled_size(geometry: &str, (w, h): (u32, u32), target: Target) -> (u32, u32) {
    let num = |v: &str| v.parse::<f64>().unwrap_or(f64::MAX);
    let (wf, hf) = (f64::from(w), f64::from(h));
    let derived = |v: f64| match target {
        Target::Image { .. } => v.round(),
        _ => (v / 2.0).round() * 2.0,
    };
    let (ow, oh) = if let Some(pct) = geometry.strip_suffix('%') {
        let p = num(pct) / 100.0;
        ((wf * p).round(), (hf * p).round())
    } else {
        match geometry.split_once('x') {
            Some((a, "")) => (num(a), derived(hf * num(a) / wf)),
            Some(("", b)) => (derived(wf * num(b) / hf), num(b)),
            Some((a, b)) => {
                let (bw, bh) = (num(a), num(b));
                if bw * hf <= bh * wf {
                    (bw, (hf * bw / wf).round())
                } else {
                    ((wf * bh / hf).round(), bh)
                }
            }
            None => (num(geometry), derived(hf * num(geometry) / wf)),
        }
    };
    let side = |v: f64| v.clamp(1.0, f64::from(u32::MAX)) as u32;
    let (ow, oh) = (side(ow), side(oh));
    match target {
        Target::Video => (ow & !1, oh & !1),
        _ => (ow, oh),
    }
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

/// Resolves the video knobs against a source, for a target of `target`'s
/// kind.
pub fn resolve(tuning: &Tuning, probe: Option<&MediaProbe>, target: Target) -> ResolvedVideo {
    let gif_width = match target {
        Target::Gif { default_width, .. } => Some(default_width),
        _ => None,
    };
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
        let dims = probe.and_then(|p| p.display_dimensions());
        let grown = dims
            .filter(|_| tuning.upscale)
            .map(|d| (d, scaled_size(geometry, d, target)))
            .filter(|&((w, h), (ow, oh))| {
                u64::from(ow) * u64::from(oh) > u64::from(w) * u64::from(h)
            });
        match (dims, grown) {
            (_, Some((source, output))) => {
                r.scale = Some(scale_filter(geometry, true));
                let estimate = estimate(target, output, probe, &r);
                r.enlarged = Some(enlargement(geometry, source, output, estimate));
            }
            (Some((w, h)), None) if !geometry_binds(geometry, (w, h)) => {
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
            (Some(_), None) => r.scale = Some(scale_filter(geometry, tuning.upscale)),
            (None, None) => {
                r.scale = Some(scale_filter(geometry, tuning.upscale));
                if tuning.upscale {
                    r.enlarged = unsized_enlargement(geometry);
                }
            }
        }
    }

    r
}

/// The cost of every enlargement, as the warnings end it.
const COST: &str = "enlarging adds no detail, so expect a soft picture and a much larger file";

/// An enlargement of a source whose size was read.
fn enlargement(
    geometry: &str,
    (w, h): (u32, u32),
    (ow, oh): (u32, u32),
    estimate: Option<[u64; 2]>,
) -> Enlargement {
    let (pixels, source_pixels) = (u64::from(ow) * u64::from(oh), u64::from(w) * u64::from(h));
    let ratio = pixels as f64 / source_pixels as f64;
    let size = estimate
        .map(|[lo, hi]| format!(", very roughly {} to {}", rough_size(lo), rough_size(hi)))
        .unwrap_or_default();
    Enlargement {
        warning: format!(
            "--resize {geometry} --upscale enlarges the {w}x{h} source to {ow}x{oh}, about {} \
             times its pixels: {COST}{size}.",
            show_ratio(ratio)
        ),
        pixel_ratio: Some(ratio),
        source: Some([w, h]),
        output: Some([ow, oh]),
        estimated_bytes: estimate,
        needs_confirmation: pixels > 4 * source_pixels,
    }
}

/// An enlargement of a source whose size is unknown. A percentage still
/// knows its ratio, so it warns only when it enlarges, and asks first past
/// four times the pixels, as a sized one does; any other geometry might
/// enlarge or not, so it warns without asking.
fn unsized_enlargement(geometry: &str) -> Option<Enlargement> {
    let (warning, pixel_ratio, needs_confirmation) = match geometry.strip_suffix('%') {
        Some(pct) => {
            let p = pct.parse::<u64>().unwrap_or(u64::MAX);
            if p <= 100 {
                return None;
            }
            let ratio = (p as f64 / 100.0).powi(2);
            let warning = format!(
                "--resize {geometry} --upscale enlarges the source to about {} times its \
                 pixels: {COST}.",
                show_ratio(ratio)
            );
            (warning, Some(ratio), p > 200)
        }
        None => (
            format!(
                "--resize {geometry} --upscale enlarges any source smaller than that, and this \
                 one's size is not known: {COST}."
            ),
            None,
            false,
        ),
    };
    Some(Enlargement {
        warning,
        pixel_ratio,
        source: None,
        output: None,
        estimated_bytes: None,
        needs_confirmation,
    })
}

/// A rough output size, `[low, high]` bytes, by a rule of thumb measured on
/// enlarged pictures: bits per pixel per frame for a video encode (0.01 to
/// 0.1 at the default quality; enlarged pictures sit low, since enlarging
/// adds no detail to spend bits on), bytes per pixel per frame for a GIF
/// (0.05 to 0.5), and the target format's bytes per pixel for an image.
/// `None` when a figure it needs is unknown.
fn estimate(
    target: Target,
    (ow, oh): (u32, u32),
    probe: Option<&MediaProbe>,
    r: &ResolvedVideo,
) -> Option<[u64; 2]> {
    let pixels = f64::from(ow) * f64::from(oh);
    let frames = |rate: f64| {
        let secs = probe?.duration_ms? as f64 / 1000.0;
        Some(pixels * secs * rate)
    };
    let source_rate = probe
        .and_then(|p| p.frame_rate)
        .map(|(n, d)| f64::from(n) / f64::from(d));
    let asked_rate = r.fps.as_deref().and_then(rate_value);
    let (amount, (lo, hi)) = match target {
        Target::Image { bytes_per_pixel } => (pixels, bytes_per_pixel?),
        Target::Video => (
            frames(asked_rate.or(source_rate)?)?,
            (0.01 / 8.0, 0.1 / 8.0),
        ),
        Target::Gif { default_fps, .. } => {
            let rate = match (asked_rate, source_rate) {
                (Some(asked), _) => asked,
                (None, Some(source)) if r.keep_source_rate => source,
                (None, Some(source)) => source.min(f64::from(default_fps)),
                (None, None) => return None,
            };
            (frames(rate)?, (0.05, 0.5))
        }
    };
    Some([(amount * lo).round() as u64, (amount * hi).round() as u64])
}

/// A byte count to two significant figures, in decimal units: `3.4 MB`,
/// `680 MB`. Only for estimates, which claim no more precision than that.
fn rough_size(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".to_string();
    }
    let b = bytes as f64;
    let (v, unit) = [(1e9, "GB"), (1e6, "MB"), (1e3, "KB")]
        .into_iter()
        .find(|&(scale, _)| b >= scale)
        .map_or((b, "B"), |(scale, unit)| (b / scale, unit));
    let step = 10f64.powi(v.log10().floor() as i32 - 1);
    let r = (v / step).round() * step;
    if r >= 10.0 {
        format!("{r:.0} {unit}")
    } else {
        format!("{r:.1} {unit}")
    }
}

/// The refusal for a large upscale run without consent.
pub fn confirmation_error(input: &std::path::Path) -> crate::ConvError {
    crate::ConvError::new(
        crate::ErrorCode::ConfirmationRequired,
        format!(
            "large upscale not confirmed for {}; pass --yes to convert anyway",
            input.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIDEO: Target = Target::Video;
    const GIF: Target = Target::Gif {
        default_width: 640,
        default_fps: 15,
    };
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
            VIDEO,
        );
        assert_eq!(r.fps.as_deref(), Some("24"));
        assert!(r.notes.is_empty());
    }

    #[test]
    fn a_cap_above_the_source_does_nothing_and_says_so() {
        let r = resolve(
            &tuning_fps("30"),
            Some(&probe_at(1920, 1080, (24, 1))),
            VIDEO,
        );
        assert_eq!(r.fps, None, "a cap must never raise a rate");
        assert_eq!(
            r.notes,
            vec!["Source is 24 fps; --fps 30 left it unchanged.".to_string()]
        );
    }

    #[test]
    fn a_cap_equal_to_the_source_does_nothing() {
        let r = resolve(&tuning_fps("30"), Some(&probe_at(640, 480, (30, 1))), VIDEO);
        assert_eq!(r.fps, None);
        assert_eq!(r.notes.len(), 1);
    }

    #[test]
    fn a_fractional_source_rate_is_compared_honestly() {
        // 30000/1001 is 29.97, so --fps 30 does not bind.
        let r = resolve(
            &tuning_fps("30"),
            Some(&probe_at(1920, 1080, (30000, 1001))),
            VIDEO,
        );
        assert_eq!(r.fps, None);
        assert_eq!(
            r.notes,
            vec!["Source is 29.97 fps; --fps 30 left it unchanged.".to_string()]
        );
    }

    #[test]
    fn an_unknown_source_rate_applies_the_value_and_admits_it() {
        let r = resolve(&tuning_fps("24"), Some(&MediaProbe::default()), VIDEO);
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
        let r = resolve(&tuning_fps("24"), None, VIDEO);
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
                VIDEO,
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
        let r = resolve(&tuning_resize("200%"), None, VIDEO);
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
            VIDEO,
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

    const COST: &str = "enlarging adds no detail, so expect a soft picture and a much larger file";

    fn image(bytes_per_pixel: Option<(f64, f64)>) -> Target {
        Target::Image { bytes_per_pixel }
    }

    #[test]
    fn upscale_lets_every_form_enlarge_and_names_the_size_and_ratio() {
        let cases = [
            ("4000x", "scale=w=4000:h=-2", (4000, 2250), "9.8"),
            ("4000", "scale=w=4000:h=-2", (4000, 2250), "9.8"),
            ("x900", "scale=w=-2:h=900", (1600, 900), "1.6"),
            (
                "4000x3000",
                "scale=w=4000:h=3000:force_original_aspect_ratio=decrease",
                (4000, 2250),
                "9.8",
            ),
            (
                "400%",
                "scale=w=iw*400/100:h=ih*400/100",
                (5120, 2880),
                "16",
            ),
        ];
        for (geometry, filter, (ow, oh), ratio) in cases {
            let r = resolve(
                &tuning_upscale(geometry),
                Some(&probe_at(1280, 720, (30, 1))),
                VIDEO,
            );
            assert_eq!(r.scale.as_deref(), Some(filter), "geometry {geometry}");
            assert!(!r.keep_source_size);
            assert!(r.notes.is_empty(), "{:?}", r.notes);
            let e = r.enlarged.expect("an enlargement warns");
            assert_eq!(
                e.warning,
                format!(
                    "--resize {geometry} --upscale enlarges the 1280x720 source to {ow}x{oh}, \
                     about {ratio} times its pixels: {COST}."
                ),
                "geometry {geometry}"
            );
            assert_eq!(e.source, Some([1280, 720]));
            assert_eq!(e.output, Some([ow, oh]));
        }
    }

    #[test]
    fn exactly_four_times_the_pixels_warns_and_more_asks_first() {
        let at = |geometry| {
            resolve(
                &tuning_upscale(geometry),
                Some(&probe_at(640, 360, (30, 1))),
                image(None),
            )
            .enlarged
            .expect("an enlargement warns")
        };
        let four = at("1280x");
        assert_eq!(four.output, Some([1280, 720]));
        assert_eq!(four.pixel_ratio, Some(4.0));
        assert!(!four.needs_confirmation, "4x warns only");
        let more = at("1281x");
        assert_eq!(more.output, Some([1281, 721]));
        assert!(more.needs_confirmation, "past 4x asks first");
    }

    #[test]
    fn a_video_size_is_rounded_to_even_as_the_encode_will_be() {
        let e = resolve(
            &tuning_upscale("1282x"),
            Some(&probe_at(640, 360, (30, 1))),
            VIDEO,
        )
        .enlarged
        .unwrap();
        assert_eq!(e.output, Some([1282, 722]));
        assert!(e.needs_confirmation);
    }

    #[test]
    fn a_percentage_knows_its_ratio_without_the_source() {
        // At or below 100% it cannot enlarge, so there is nothing to say.
        let r = resolve(&tuning_upscale("50%"), None, VIDEO);
        assert_eq!(r.enlarged, None);
        assert_eq!(r.scale.as_deref(), Some("scale=w=iw*50/100:h=ih*50/100"));

        let warn = |pct: &str| resolve(&tuning_upscale(pct), None, VIDEO).enlarged.unwrap();
        let double = warn("200%");
        assert_eq!(
            double.warning,
            format!(
                "--resize 200% --upscale enlarges the source to about 4 times its pixels: {COST}."
            )
        );
        assert_eq!(double.pixel_ratio, Some(4.0));
        assert!(!double.needs_confirmation, "4x warns only");
        let triple = warn("300%");
        assert_eq!(triple.pixel_ratio, Some(9.0));
        assert!(
            triple.needs_confirmation,
            "past 4x asks first, probe or not"
        );
        assert_eq!(triple.output, None);
    }

    #[test]
    fn upscale_on_a_shrinking_resize_changes_nothing_and_warns_nothing() {
        let r = resolve(
            &tuning_upscale("640x"),
            Some(&probe_at(1920, 1080, (30, 1))),
            VIDEO,
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
            VIDEO,
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
        let r = resolve(&tuning_upscale("4000x"), None, VIDEO);
        assert_eq!(r.scale.as_deref(), Some("scale=w=4000:h=-2"));
        let e = r.enlarged.unwrap();
        assert_eq!(
            e.warning,
            format!(
                "--resize 4000x --upscale enlarges any source smaller than that, and this \
                 one's size is not known: {COST}."
            )
        );
        assert!(!e.needs_confirmation, "no ratio, so nothing to ask about");
        assert_eq!(e.pixel_ratio, None);
    }

    fn timed(w: u32, h: u32, rate: (u32, u32), secs: u64) -> MediaProbe {
        MediaProbe {
            duration_ms: Some(secs * 1000),
            ..probe_at(w, h, rate)
        }
    }

    #[test]
    fn a_video_estimate_is_a_wide_range_by_bits_per_pixel() {
        // 4000x2250 for 300 frames, at 0.01-0.1 bits a pixel.
        let e = resolve(
            &tuning_upscale("4000x"),
            Some(&timed(1280, 720, (30, 1), 10)),
            VIDEO,
        )
        .enlarged
        .unwrap();
        assert_eq!(e.estimated_bytes, Some([3_375_000, 33_750_000]));
        assert!(
            e.warning
                .ends_with(&format!("{COST}, very roughly 3.4 MB to 34 MB.")),
            "{}",
            e.warning
        );
    }

    #[test]
    fn a_gif_estimate_counts_the_frames_its_own_rate_keeps() {
        // 15 fps by default: 150 frames of 4000x2250 at 0.05-0.5 bytes a pixel.
        let e = resolve(
            &tuning_upscale("4000x"),
            Some(&timed(1280, 720, (30, 1), 10)),
            GIF,
        )
        .enlarged
        .unwrap();
        assert_eq!(e.estimated_bytes, Some([67_500_000, 675_000_000]));
        assert!(
            e.warning.ends_with("very roughly 68 MB to 680 MB."),
            "{}",
            e.warning
        );
    }

    #[test]
    fn an_image_estimate_goes_by_the_target_formats_bytes_per_pixel() {
        let e = resolve(
            &tuning_upscale("1600x900"),
            Some(&probe_at(320, 240, (30, 1))),
            image(Some((0.05, 0.3))),
        )
        .enlarged
        .unwrap();
        assert_eq!(e.output, Some([1200, 900]));
        assert_eq!(e.estimated_bytes, Some([54_000, 324_000]));
        assert!(
            e.warning.ends_with("very roughly 54 KB to 320 KB."),
            "{}",
            e.warning
        );
    }

    #[test]
    fn rough_sizes_keep_two_significant_figures() {
        assert_eq!(rough_size(0), "0 B");
        assert_eq!(rough_size(540), "540 B");
        assert_eq!(rough_size(54_000), "54 KB");
        assert_eq!(rough_size(3_375_000), "3.4 MB");
        assert_eq!(rough_size(675_000_000), "680 MB");
        assert_eq!(rough_size(1_250_000_000), "1.3 GB");
    }

    #[test]
    fn a_gif_resize_past_a_source_wider_than_the_default_says_so() {
        let r = resolve(
            &tuning_resize("4000x"),
            Some(&probe_at(1280, 720, (30, 1))),
            GIF,
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
            GIF,
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
            VIDEO,
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
            VIDEO,
        );
        assert!(
            wide.notes.is_empty(),
            "500 < 1080 should bind: {:?}",
            wide.notes
        );

        let tall = resolve(
            &tuning_resize("500x3000"),
            Some(&probe_at(1920, 1080, (30, 1))),
            VIDEO,
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
            VIDEO,
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
        let r = resolve(&tuning_resize("1000"), Some(&p), VIDEO);
        // Displayed width is 720, so a 1000-wide cap does not bind.
        assert_eq!(r.notes.len(), 1, "should report the cap did nothing");
    }

    #[test]
    fn an_empty_tuning_resolves_to_nothing_at_all() {
        let r = resolve(
            &Tuning::default(),
            Some(&probe_at(1920, 1080, (30, 1))),
            VIDEO,
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
