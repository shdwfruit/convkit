//! Choosing resolution, frame rate and bitrates for a size target.
//!
//! Pure arithmetic over a probed source: no process, no file. Every
//! combination of a resolution step, a frame-rate step and an audio step is
//! scored against the byte budget, and the cheapest wins. The cost adds
//! three terms on one 0-100 scale anchored to VMAF: picture loss
//! (resolution and compression artefacts together), frame-rate loss, and
//! audio loss. Every curve steepens as it falls, so many small cuts cost
//! less than one large one, and the cheapest choice spreads the loss across
//! the dials instead of draining one before touching the next. Nothing here
//! says "cut every dial"; it follows from the shape.
//!
//! Constants marked "calibrated" are set by the measurements recorded in
//! docs/defaults-calibration.md; the authored curves are judgement, with
//! the reasoning written beside them there.

use serde::{Serialize, Serializer};

use crate::probe::MediaProbe;
use crate::Format;

/// Aim this far under the target, per mille, so an encode that lands a
/// little over its bitrate still fits. Calibrated.
pub const MARGIN_PERMILLE: u64 = 30;
/// Container overhead, per mille of the target. Calibrated.
pub const OVERHEAD_PERMILLE: u64 = 10;
/// Bytes set aside for each subtitle stream passed through. Calibrated.
pub const SUBTITLE_ALLOWANCE_BYTES: u64 = 100_000;
/// Below this an encoder cannot hold a bitrate at all.
pub const MIN_VIDEO_BPS: u64 = 16_000;
/// A choice costing more than this is extreme: it needs confirmation.
pub const EXTREME_COST: f64 = 40.0;

/// Short-side steps. The source's own short side is always the first step.
const SHORT_SIDES: [u32; 9] = [2160, 1440, 1080, 720, 540, 480, 360, 240, 144];
/// Frame-rate steps stop at an even division giving below 1 fps, or here.
const MAX_FPS_DIVISOR: u32 = 240;

/// (kb/s per track, cost). Authored.
const AAC_LADDER: [(u32, f64); 6] = [
    (160, 0.0),
    (128, 1.0),
    (96, 4.0),
    (64, 12.0),
    (48, 22.0),
    (32, 40.0),
];
const OPUS_LADDER: [(u32, f64); 7] = [
    (128, 0.0),
    (96, 1.0),
    (64, 4.0),
    (48, 8.0),
    (32, 15.0),
    (24, 25.0),
    (16, 40.0),
];
/// (fps, cost), interpolated in log2(fps). Authored: VMAF does not score
/// motion smoothness.
const FPS_CURVE: [(f64, f64); 12] = [
    (144.0, 0.0),
    (120.0, 0.0),
    (60.0, 2.0),
    (48.0, 4.0),
    (30.0, 8.0),
    (24.0, 12.0),
    (20.0, 18.0),
    (15.0, 28.0),
    (12.0, 38.0),
    (10.0, 48.0),
    (5.0, 75.0),
    (1.0, 100.0),
];

/// Predicted VMAF for a picture shrunk by `scale` (short side over the
/// source's) and encoded at `bpp` bits per pixel per frame:
/// `(100 - res_scale * log2(1/scale)^res_power) * (1 - artefact/100)`, with
/// `artefact = 100 / (1 + (bpp / bpp_half)^bpp_slope)`. Calibrated per codec.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoFit {
    pub res_scale: f64,
    pub res_power: f64,
    pub bpp_half: f64,
    pub bpp_slope: f64,
}

pub const X264_FIT: VideoFit = VideoFit {
    res_scale: 12.0,
    res_power: 2.0,
    bpp_half: 0.011,
    bpp_slope: 1.4,
};
pub const VP9_FIT: VideoFit = VideoFit {
    res_scale: 12.0,
    res_power: 2.0,
    bpp_half: 0.008,
    bpp_slope: 1.4,
};

/// How much each dial's loss counts. All 1.0 today; the future
/// `--prio-res`/`--prio-fps`/`--prio-audio` flags raise one of them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizePolicy {
    pub w_video: f64,
    pub w_fps: f64,
    pub w_audio: f64,
}

impl Default for SizePolicy {
    fn default() -> Self {
        SizePolicy {
            w_video: 1.0,
            w_fps: 1.0,
            w_audio: 1.0,
        }
    }
}

/// What the budget needs to know about a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Displayed (rotation-applied) dimensions.
    pub width: u32,
    pub height: u32,
    pub fps: (u32, u32),
    pub duration_ms: u64,
    /// One entry per audio track, in bits per second where known.
    pub audio_bitrates: Vec<Option<u32>>,
    pub subtitle_tracks: usize,
    pub attachment_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceGap {
    NoVideo,
    NoDimensions,
    NoFrameRate,
    NoDuration,
}

impl Source {
    pub fn from_probe(p: &MediaProbe) -> Result<Source, SourceGap> {
        if p.video_streams == 0 {
            return Err(SourceGap::NoVideo);
        }
        let (width, height) = p.display_dimensions().ok_or(SourceGap::NoDimensions)?;
        let fps = p.frame_rate.ok_or(SourceGap::NoFrameRate)?;
        let duration_ms = p
            .duration_ms
            .filter(|&d| d > 0)
            .ok_or(SourceGap::NoDuration)?;
        Ok(Source {
            width,
            height,
            fps,
            duration_ms,
            audio_bitrates: p.audio_bitrates.clone(),
            subtitle_tracks: p.subtitle_codecs.len(),
            attachment_bytes: p.attachment_bytes,
        })
    }

    pub fn short_side(&self) -> u32 {
        self.width.min(self.height)
    }

    fn long_side(&self) -> u32 {
        self.width.max(self.height)
    }

    fn seconds(&self) -> f64 {
        self.duration_ms.max(1) as f64 / 1000.0
    }
}

/// User ceilings from `--resize` (as the fitted output dimensions) and
/// `--fps`. Candidates above either are never considered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Limits {
    pub max_dims: Option<(u32, u32)>,
    pub max_fps: Option<(u32, u32)>,
}

/// Set when even the bottom step of every dial is predicted over the
/// target: what the smallest possible file would weigh, and how much of
/// that is audio alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Over {
    pub predicted_bytes: u64,
    pub audio_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SizedChoice {
    pub width: u32,
    pub height: u32,
    #[serde(serialize_with = "rational")]
    pub fps: (u32, u32),
    pub video_bps: u64,
    /// Per audio track; `None` when the source has no audio.
    pub audio_kbps: Option<u32>,
    /// The total cost in tenths, so the struct stays `Eq`.
    #[serde(rename = "cost", serialize_with = "tenths")]
    pub cost_tenths: u32,
    pub extreme: bool,
    pub over: Option<Over>,
    /// When extreme: the smallest target that would not be.
    pub suggested_bytes: Option<u64>,
}

impl SizedChoice {
    pub fn cost(&self) -> f64 {
        f64::from(self.cost_tenths) / 10.0
    }
}

fn rational<S: Serializer>(r: &(u32, u32), s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format!("{}/{}", r.0, r.1))
}

fn tenths<S: Serializer>(t: &u32, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64(f64::from(*t) / 10.0)
}

/// Bytes left for audio and video after the margin, the container
/// overhead, subtitle allowances and (for mkv, the one target that keeps
/// them) attachments.
pub(crate) fn payload_bytes(src: &Source, target: u64, to: Format) -> u64 {
    let attachments = if to == Format::Mkv {
        src.attachment_bytes
    } else {
        0
    };
    let reserve = target * MARGIN_PERMILLE / 1000
        + target * OVERHEAD_PERMILLE / 1000
        + src.subtitle_tracks as u64 * SUBTITLE_ALLOWANCE_BYTES
        + attachments;
    target.saturating_sub(reserve)
}

fn codec_for(to: Format) -> (&'static VideoFit, &'static [(u32, f64)]) {
    if to == Format::Webm {
        (&VP9_FIT, &OPUS_LADDER)
    } else {
        (&X264_FIT, &AAC_LADDER)
    }
}

fn rate_value((n, d): (u32, u32)) -> f64 {
    f64::from(n) / f64::from(d)
}

/// (short side, width, height) per step, largest first. The first step is
/// the source itself, or the `--resize` bound when that is smaller.
fn resolution_steps(src: &Source, limits: &Limits) -> Vec<(u32, u32, u32)> {
    let (short, long) = (src.short_side(), src.long_side());
    let top = limits.max_dims.map_or(short, |(w, h)| w.min(h)).min(short);
    let mut shorts = vec![top];
    shorts.extend(SHORT_SIDES.iter().copied().filter(|&s| s < top));
    shorts
        .into_iter()
        .map(|s| {
            if s == short
                && limits
                    .max_dims
                    .is_none_or(|(w, h)| w >= src.width && h >= src.height)
            {
                return (s, src.width & !1, src.height & !1);
            }
            let s = (s & !1).max(2);
            let l = (u64::from(s) * u64::from(long) + u64::from(short) / 2) / u64::from(short);
            let l = ((l as u32) & !1).max(2);
            if src.width >= src.height {
                (s, l, s)
            } else {
                (s, s, l)
            }
        })
        .collect()
}

/// Frame-rate steps, fastest first: the user's `--fps` rate itself when it
/// is below the source, then every even division of the source rate at or
/// under the ceiling, down to 1 fps. Even divisions keep every kept frame
/// evenly spaced, so nothing stutters.
fn fps_steps(src: &Source, limits: &Limits) -> Vec<(u32, u32)> {
    let below = |a: (u32, u32), b: (u32, u32)| {
        u64::from(a.0) * u64::from(b.1) < u64::from(b.0) * u64::from(a.1)
    };
    let at_or_below = |a: (u32, u32), b: (u32, u32)| !below(b, a);
    let mut out = Vec::new();
    if let Some(cap) = limits.max_fps.filter(|&cap| below(cap, src.fps)) {
        out.push(cap);
    }
    for k in 1..=MAX_FPS_DIVISOR {
        let step = (src.fps.0, src.fps.1.saturating_mul(k));
        if k > 1 && rate_value(step) < 1.0 {
            break;
        }
        if limits.max_fps.is_none_or(|cap| at_or_below(step, cap)) && !out.contains(&step) {
            out.push(step);
        }
    }
    if out.is_empty() {
        out.push(src.fps);
    }
    out
}

/// Audio steps under the source's highest track bitrate, each with its
/// cost, and the cost of the first (the reference a loss is measured from).
/// `[None]` for a silent source.
fn audio_steps(src: &Source, ladder: &'static [(u32, f64)]) -> (Vec<Option<(u32, f64)>>, f64) {
    if src.audio_bitrates.is_empty() {
        return (vec![None], 0.0);
    }
    let top_kbps = src
        .audio_bitrates
        .iter()
        .map(|b| b.map_or(u32::MAX, |b| b / 1000))
        .max()
        .unwrap_or(u32::MAX);
    let mut steps: Vec<(u32, f64)> = ladder
        .iter()
        .copied()
        .filter(|&(k, _)| k <= top_kbps)
        .collect();
    if steps.is_empty() {
        steps.push(*ladder.last().expect("ladders are not empty"));
    }
    let reference = steps[0].1;
    (steps.into_iter().map(Some).collect(), reference)
}

fn video_loss(scale: f64, bpp: f64, fit: &VideoFit) -> f64 {
    let octaves = if scale >= 1.0 {
        0.0
    } else {
        (1.0 / scale).log2()
    };
    let res_loss = (fit.res_scale * octaves.powf(fit.res_power)).min(100.0);
    let artefact = 100.0 / (1.0 + (bpp.max(1e-9) / fit.bpp_half).powf(fit.bpp_slope));
    100.0 - (100.0 - res_loss) * (1.0 - artefact / 100.0)
}

fn fps_curve(fps: f64) -> f64 {
    if fps >= FPS_CURVE[0].0 {
        return 0.0;
    }
    for w in FPS_CURVE.windows(2) {
        let ((hi, c_hi), (lo, c_lo)) = (w[0], w[1]);
        if (lo..=hi).contains(&fps) {
            let t = (hi.log2() - fps.log2()) / (hi.log2() - lo.log2());
            return c_hi + t * (c_lo - c_hi);
        }
    }
    100.0
}

struct Candidate {
    width: u32,
    height: u32,
    fps: (u32, u32),
    audio_kbps: Option<u32>,
    video_bps: f64,
    cost: f64,
}

/// The cheapest feasible candidate for `target`, or the bottom step of
/// every dial flagged `over` when none is feasible. Never suggests a size;
/// `choose` does that, so the bisection can call this without recursing.
fn evaluate(
    src: &Source,
    target: u64,
    to: Format,
    limits: &Limits,
    policy: &SizePolicy,
) -> SizedChoice {
    let (fit, ladder) = codec_for(to);
    let seconds = src.seconds();
    let payload_bits = payload_bytes(src, target, to) as f64 * 8.0;
    let tracks = src.audio_bitrates.len() as f64;
    let fps_ref = fps_curve(rate_value(src.fps));
    let (audio, audio_ref) = audio_steps(src, ladder);
    let resolutions = resolution_steps(src, limits);
    let rates = fps_steps(src, limits);

    let mut best: Option<Candidate> = None;
    for &(short, width, height) in &resolutions {
        let scale = f64::from(short) / f64::from(src.short_side());
        for &fps in &rates {
            let fps_cost = (fps_curve(rate_value(fps)) - fps_ref).max(0.0);
            for step in &audio {
                let (kbps, audio_cost) = step.unwrap_or((0, audio_ref));
                let audio_bits = f64::from(kbps) * 1000.0 * tracks * seconds;
                let video_bps = (payload_bits - audio_bits) / seconds;
                if video_bps < MIN_VIDEO_BPS as f64 {
                    continue;
                }
                let bpp = video_bps / (f64::from(width) * f64::from(height) * rate_value(fps));
                let cost = policy.w_video * video_loss(scale, bpp, fit)
                    + policy.w_fps * fps_cost
                    + policy.w_audio * (audio_cost - audio_ref).max(0.0);
                // Strictly lower only: iteration runs largest-first on every
                // dial, so a tie keeps the larger picture, then the faster
                // rate, then the richer audio.
                if best.as_ref().is_none_or(|b| cost < b.cost - 1e-9) {
                    best = Some(Candidate {
                        width,
                        height,
                        fps,
                        audio_kbps: step.map(|(k, _)| k),
                        video_bps,
                        cost,
                    });
                }
            }
        }
    }

    if let Some(c) = best {
        return SizedChoice {
            width: c.width,
            height: c.height,
            fps: c.fps,
            video_bps: c.video_bps as u64,
            audio_kbps: c.audio_kbps,
            cost_tenths: (c.cost * 10.0).round().clamp(0.0, 10_000.0) as u32,
            extreme: c.cost > EXTREME_COST,
            over: None,
            suggested_bytes: None,
        };
    }

    // Nothing fits: the bottom of every dial, predicted over the target.
    let &(short, width, height) = resolutions.last().expect("at least one resolution step");
    let fps = *rates.last().expect("at least one frame-rate step");
    let kbps = audio.last().copied().flatten().map(|(k, _)| k);
    let audio_bits = f64::from(kbps.unwrap_or(0)) * 1000.0 * tracks * seconds;
    let video_bits = MIN_VIDEO_BPS as f64 * seconds;
    let reserve_without_margin = target * OVERHEAD_PERMILLE / 1000
        + src.subtitle_tracks as u64 * SUBTITLE_ALLOWANCE_BYTES
        + if to == Format::Mkv {
            src.attachment_bytes
        } else {
            0
        };
    let bpp = MIN_VIDEO_BPS as f64 / (f64::from(width) * f64::from(height) * rate_value(fps));
    let cost = policy.w_video
        * video_loss(f64::from(short) / f64::from(src.short_side()), bpp, fit)
        + policy.w_fps * (fps_curve(rate_value(fps)) - fps_ref).max(0.0)
        + policy.w_audio
            * audio
                .last()
                .copied()
                .flatten()
                .map_or(0.0, |(_, c)| (c - audio_ref).max(0.0));
    SizedChoice {
        width,
        height,
        fps,
        video_bps: MIN_VIDEO_BPS,
        audio_kbps: kbps,
        cost_tenths: (cost * 10.0).round().clamp(0.0, 10_000.0) as u32,
        extreme: true,
        over: Some(Over {
            predicted_bytes: ((video_bits + audio_bits) / 8.0) as u64 + reserve_without_margin,
            audio_bytes: (audio_bits / 8.0) as u64,
        }),
        suggested_bytes: None,
    }
}

/// Chooses the settings for `target_bytes`, and when the choice is
/// extreme, the smallest target that would not be.
pub fn choose(
    src: &Source,
    target_bytes: u64,
    to: Format,
    limits: &Limits,
    policy: &SizePolicy,
) -> SizedChoice {
    let mut c = evaluate(src, target_bytes, to, limits, policy);
    if c.extreme {
        c.suggested_bytes = suggest_target(src, target_bytes, to, limits, policy);
    }
    c
}

/// Bisects for the smallest target whose best choice is not extreme. The
/// cost only falls as the target grows (every candidate gets more bits), so
/// the search is well-founded. `None` if even a vast target stays extreme.
fn suggest_target(
    src: &Source,
    target: u64,
    to: Format,
    limits: &Limits,
    policy: &SizePolicy,
) -> Option<u64> {
    let fits = |bytes: u64| !evaluate(src, bytes, to, limits, policy).extreme;
    let mut lo = target;
    let mut hi = target.max(1);
    let mut doublings = 0;
    while !fits(hi) {
        lo = hi;
        hi = hi.checked_mul(2)?;
        doublings += 1;
        if doublings > 40 {
            return None;
        }
    }
    while hi - lo > (hi / 1000).max(1) {
        let mid = lo + (hi - lo) / 2;
        if fits(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Some(hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(w: u32, h: u32, fps: (u32, u32), secs: u64, audio: &[Option<u32>]) -> Source {
        Source {
            width: w,
            height: h,
            fps,
            duration_ms: secs * 1000,
            audio_bitrates: audio.to_vec(),
            subtitle_tracks: 0,
            attachment_bytes: 0,
        }
    }

    fn pick(src: &Source, target: u64) -> SizedChoice {
        choose(
            src,
            target,
            Format::Mp4,
            &Limits::default(),
            &SizePolicy::default(),
        )
    }

    fn rate(r: (u32, u32)) -> f64 {
        f64::from(r.0) / f64::from(r.1)
    }

    #[test]
    fn the_payload_reserves_margin_overhead_subtitles_and_mkv_attachments() {
        let mut src = source(1920, 1080, (30, 1), 60, &[Some(160_000)]);
        src.subtitle_tracks = 1;
        src.attachment_bytes = 5_000;
        let target = 10_000_000;
        let reserve = target * MARGIN_PERMILLE / 1000
            + target * OVERHEAD_PERMILLE / 1000
            + SUBTITLE_ALLOWANCE_BYTES;
        assert_eq!(payload_bytes(&src, target, Format::Mp4), target - reserve);
        assert_eq!(
            payload_bytes(&src, target, Format::Mkv),
            target - reserve - 5_000,
            "only mkv carries attachments"
        );
    }

    /// The case the design exists for: a 144 fps source must shed frame
    /// rate (nearly free above 60 fps) before it sheds resolution, never
    /// ending at 360p while still at 144 fps.
    #[test]
    fn loss_is_spread_rather_than_drained_from_one_dial() {
        let src = source(1920, 1080, (144, 1), 60, &[Some(160_000)]);
        let c = pick(&src, 10_000_000);
        assert!(rate(c.fps) < 144.0, "{c:?}");
        let short = c.width.min(c.height);
        assert!(!(short <= 360 && rate(c.fps) >= 144.0), "{c:?}");
        assert!(short > 360, "{c:?}");
    }

    #[test]
    fn a_larger_target_never_costs_more() {
        let src = source(1920, 1080, (60, 1), 120, &[Some(160_000)]);
        let costs: Vec<u32> = [5_000_000, 10_000_000, 20_000_000, 80_000_000]
            .iter()
            .map(|&t| pick(&src, t).cost_tenths)
            .collect();
        assert!(costs.windows(2).all(|w| w[0] >= w[1]), "{costs:?}");
    }

    #[test]
    fn a_generous_target_keeps_the_source_as_it_is() {
        let src = source(640, 360, (30, 1), 10, &[Some(128_000)]);
        let c = pick(&src, 1_000_000_000);
        assert_eq!((c.width, c.height), (640, 360));
        assert_eq!(c.fps, (30, 1));
        assert_eq!(c.audio_kbps, Some(128));
        assert!(!c.extreme);
        assert!(c.over.is_none());
    }

    #[test]
    fn nothing_is_ever_upscaled_or_sped_up() {
        let src = source(1280, 720, (24, 1), 30, &[None]);
        for target in [500_000, 2_000_000, 50_000_000] {
            let c = pick(&src, target);
            assert!(c.width <= 1280 && c.height <= 720, "{c:?}");
            assert!(rate(c.fps) <= 24.0, "{c:?}");
        }
    }

    /// Review focus 2: the steps apply to the displayed short side, and a
    /// portrait source stays portrait.
    #[test]
    fn a_portrait_source_steps_its_short_side_and_stays_portrait() {
        let src = source(1080, 1920, (30, 1), 60, &[Some(128_000)]);
        let c = pick(&src, 8_000_000);
        assert!(c.width < c.height, "{c:?}");
        assert!(c.width <= 1080, "{c:?}");
    }

    /// Review focus 1: rationals stay exact and never exceed the source.
    #[test]
    fn ntsc_and_variable_rates_divide_exactly() {
        for fps in [(30_000, 1001), (1799, 60)] {
            let src = source(1920, 1080, fps, 300, &[Some(160_000)]);
            let c = pick(&src, 20_000_000);
            assert_eq!(c.fps.0, fps.0, "numerator is the source's: {c:?}");
            assert_eq!(c.fps.1 % fps.1, 0, "an even division of the source: {c:?}");
        }
    }

    #[test]
    fn user_ceilings_are_respected() {
        let src = source(1920, 1080, (30, 1), 60, &[Some(160_000)]);
        let limits = Limits {
            max_dims: Some((1280, 720)),
            max_fps: Some((24, 1)),
        };
        let c = choose(
            &src,
            200_000_000,
            Format::Mp4,
            &limits,
            &SizePolicy::default(),
        );
        assert!(c.width <= 1280 && c.height <= 720, "{c:?}");
        assert_eq!(c.fps, (24, 1), "the user's own rate is a candidate: {c:?}");
    }

    #[test]
    fn raising_a_weight_protects_that_dial() {
        let src = source(1920, 1080, (60, 1), 120, &[Some(160_000)]);
        let base = pick(&src, 8_000_000);
        let fps_first = choose(
            &src,
            8_000_000,
            Format::Mp4,
            &Limits::default(),
            &SizePolicy {
                w_fps: 20.0,
                ..SizePolicy::default()
            },
        );
        assert!(
            rate(fps_first.fps) >= rate(base.fps),
            "{base:?} vs {fps_first:?}"
        );
        let audio_first = choose(
            &src,
            8_000_000,
            Format::Mp4,
            &Limits::default(),
            &SizePolicy {
                w_audio: 20.0,
                ..SizePolicy::default()
            },
        );
        assert!(
            audio_first.audio_kbps >= base.audio_kbps,
            "{base:?} vs {audio_first:?}"
        );
    }

    /// Review focus 3.
    #[test]
    fn audio_tracks_are_counted_and_a_silent_source_gets_no_audio_rate() {
        let silent = source(1280, 720, (30, 1), 60, &[]);
        assert_eq!(pick(&silent, 5_000_000).audio_kbps, None);

        // Three tracks cost three times the audio. Pin the audio rate with a
        // heavy weight so both keep 128 kb/s; the video must then get fewer
        // bits with three tracks than with one.
        let keep_audio = SizePolicy {
            w_audio: 1000.0,
            ..SizePolicy::default()
        };
        let at = |tracks: &[Option<u32>]| {
            choose(
                &source(1280, 720, (30, 1), 60, tracks),
                5_000_000,
                Format::Mp4,
                &Limits::default(),
                &keep_audio,
            )
        };
        let one = at(&[Some(128_000)]);
        let three = at(&[Some(128_000); 3]);
        assert_eq!(one.audio_kbps, Some(128));
        assert_eq!(three.audio_kbps, Some(128));
        assert!(three.video_bps < one.video_bps, "{one:?} vs {three:?}");
    }

    #[test]
    fn audio_is_never_raised_above_the_source() {
        let src = source(1280, 720, (30, 1), 60, &[Some(96_000)]);
        assert!(pick(&src, 500_000_000).audio_kbps <= Some(96));
    }

    #[test]
    fn a_hopeless_target_is_extreme_over_and_suggests_a_size_that_is_not() {
        // 45 minutes: even the lowest audio rate outweighs 5 MB.
        let src = source(1920, 1080, (30, 1), 45 * 60, &[Some(160_000)]);
        let c = pick(&src, 5_000_000);
        assert!(c.extreme);
        let over = c.over.expect("predicted over");
        assert!(over.audio_bytes > 5_000_000, "{over:?}");
        let suggested = c.suggested_bytes.expect("a suggestion");
        assert!(suggested > 5_000_000);
        assert!(
            !pick(&src, suggested).extreme,
            "the suggestion must not be extreme"
        );
    }

    #[test]
    fn a_very_short_clip_under_a_normal_target_is_left_alone() {
        let src = Source {
            duration_ms: 500,
            ..source(1920, 1080, (30, 1), 0, &[Some(160_000)])
        };
        let c = pick(&src, 10_000_000);
        assert_eq!((c.width, c.height), (1920, 1080));
        assert!(!c.extreme);
    }

    #[test]
    fn from_probe_names_what_is_missing() {
        let mut p = MediaProbe {
            video_streams: 1,
            width: Some(1920),
            height: Some(1080),
            frame_rate: Some((30, 1)),
            duration_ms: Some(1000),
            ..MediaProbe::default()
        };
        assert!(Source::from_probe(&p).is_ok());
        p.duration_ms = None;
        assert_eq!(Source::from_probe(&p), Err(SourceGap::NoDuration));
        p.video_streams = 0;
        assert_eq!(Source::from_probe(&p), Err(SourceGap::NoVideo));
    }
}
