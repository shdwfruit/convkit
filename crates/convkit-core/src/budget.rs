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
//! Loss is measured from what the user asked for: the source itself, or the
//! `--resize` and `--fps` ceilings where those bind. A ceiling the user chose
//! is not a loss the budget imposed, so it is never charged as one.
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
        let (width, height) = p
            .display_dimensions()
            .filter(|&(w, h)| w > 0 && h > 0)
            .ok_or(SourceGap::NoDimensions)?;
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
/// `--fps`. Candidates above either are never considered, and loss is
/// measured from a binding ceiling rather than from the source.
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

/// What is set aside from `target` before any audio or video: the
/// container overhead, subtitle allowances, (for mkv, the one target that
/// keeps them) attachments, and, unless `without_margin`, the safety margin.
/// In `u128`, so a target anywhere in the `u64` range cannot overflow it.
fn reserve_bytes(src: &Source, target: u64, to: Format, without_margin: bool) -> u128 {
    let target = u128::from(target);
    let attachments = if to == Format::Mkv {
        u128::from(src.attachment_bytes)
    } else {
        0
    };
    let margin = if without_margin {
        0
    } else {
        target * u128::from(MARGIN_PERMILLE) / 1000
    };
    margin
        + target * u128::from(OVERHEAD_PERMILLE) / 1000
        + src.subtitle_tracks as u128 * u128::from(SUBTITLE_ALLOWANCE_BYTES)
        + attachments
}

/// Bytes left for audio and video after the margin, the container
/// overhead, subtitle allowances and (for mkv, the one target that keeps
/// them) attachments.
pub(crate) fn payload_bytes(src: &Source, target: u64, to: Format) -> u64 {
    let left = u128::from(target).saturating_sub(reserve_bytes(src, target, to, false));
    u64::try_from(left).unwrap_or(u64::MAX)
}

/// The video rate a finished file of `bytes` actually carries, worked back
/// the way the budget works forward: the file less the reserve for the
/// container, subtitles and attachments (the container's share taken of the
/// file itself, and no margin, which is headroom rather than content), less
/// the audio at `audio_kbps` on every track, over the duration. Zero when
/// the audio and the reserve account for the whole file. Whatever the
/// reserve model misses (audio spending more or less than its rate, a
/// container heavier than its allowance) is counted as video, so a
/// saturation check built on this can misjudge by one picture step either
/// way: a bounded cost, since the file is measured again after.
pub fn achieved_video_bps(src: &Source, bytes: u64, to: Format, audio_kbps: Option<u32>) -> u64 {
    let content = u128::from(bytes).saturating_sub(reserve_bytes(src, bytes, to, true));
    let audio_bits = u128::from(audio_kbps.unwrap_or(0))
        * 1000
        * src.audio_bitrates.len() as u128
        * u128::from(src.duration_ms)
        / 1000;
    let video_bits = (content * 8).saturating_sub(audio_bits);
    u64::try_from(video_bits * 1000 / u128::from(src.duration_ms.max(1))).unwrap_or(u64::MAX)
}

/// The next short-side step strictly below `short`, or `None` when `short`
/// is already at or under the smallest.
pub fn next_short_side_below(short: u32) -> Option<u32> {
    SHORT_SIDES.iter().copied().find(|&s| s < short)
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
/// the source itself, or the `--resize` bound when that is smaller. Every
/// computed dimension is even, the long side is rounded to the nearest even
/// value as ffmpeg's `-2` does, and no step exceeds the source or the bound.
fn resolution_steps(src: &Source, limits: &Limits) -> Vec<(u32, u32, u32)> {
    let (short, long) = (src.short_side(), src.long_side());
    let mut top = limits.max_dims.map_or(short, |(w, h)| w.min(h)).min(short);
    if top != short {
        // An odd bound still steps on an even short side, so it cannot repeat
        // the step just below it.
        top = (top & !1).max(2);
    }
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
                return (s, (src.width & !1).max(2), (src.height & !1).max(2));
            }
            let s = (s & !1).max(2);
            let scaled =
                (u64::from(s) * u64::from(long) + u64::from(short)) / (2 * u64::from(short)) * 2;
            let l = u32::try_from(scaled).unwrap_or(u32::MAX).max(2);
            let (w, h) = if src.width >= src.height {
                (l, s)
            } else {
                (s, l)
            };
            match limits.max_dims {
                Some((bw, bh)) => (s, w.min(bw & !1).max(2), h.min(bh & !1).max(2)),
                None => (s, w, h),
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
/// The rate is taken to the nearest kb/s, so a nominal 128 kb/s track that
/// reports 127,999 b/s keeps its 128 rung. When every rung of the ladder is
/// above the source, the source's own rate is the only step: audio is never
/// raised. `[None]` for a silent source.
fn audio_steps(src: &Source, ladder: &'static [(u32, f64)]) -> (Vec<Option<(u32, f64)>>, f64) {
    if src.audio_bitrates.is_empty() {
        return (vec![None], 0.0);
    }
    let top_kbps = src
        .audio_bitrates
        .iter()
        .map(|b| b.map_or(u32::MAX, |b| b.saturating_add(500) / 1000))
        .max()
        .unwrap_or(u32::MAX);
    let mut steps: Vec<(u32, f64)> = ladder
        .iter()
        .copied()
        .filter(|&(k, _)| k <= top_kbps)
        .collect();
    if steps.is_empty() {
        steps.push((top_kbps.max(1), 0.0));
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
    max_short: Option<u32>,
    policy: &SizePolicy,
) -> SizedChoice {
    let (fit, ladder) = codec_for(to);
    let seconds = src.seconds();
    let payload_bits = payload_bytes(src, target, to) as f64 * 8.0;
    let tracks = src.audio_bitrates.len() as f64;
    let (audio, audio_ref) = audio_steps(src, ladder);
    let mut resolutions = resolution_steps(src, limits);
    let rates = fps_steps(src, limits);
    // Loss is measured from the top step of each dial: the source, or the
    // user's own ceiling where one binds, which is not a loss the budget chose.
    // Taken before `max_short` removes any step: that cap is the budget's own.
    let top_short = f64::from(resolutions[0].0);
    let fps_ref = fps_curve(rate_value(rates[0]));
    if let Some(cap) = max_short {
        let smallest = *resolutions.last().expect("at least one resolution step");
        resolutions.retain(|&(short, _, _)| short <= cap);
        if resolutions.is_empty() {
            resolutions.push(smallest);
        }
    }

    let mut best: Option<Candidate> = None;
    for &(short, width, height) in &resolutions {
        let scale = f64::from(short) / top_short;
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
    let reserve_without_margin =
        u64::try_from(reserve_bytes(src, target, to, true)).unwrap_or(u64::MAX);
    let bpp = MIN_VIDEO_BPS as f64 / (f64::from(width) * f64::from(height) * rate_value(fps));
    let cost = policy.w_video * video_loss(f64::from(short) / top_short, bpp, fit)
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
            predicted_bytes: (((video_bits + audio_bits) / 8.0) as u64)
                .saturating_add(reserve_without_margin),
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
    choose_capped(src, target_bytes, to, limits, None, policy)
}

/// `choose`, leaving out every picture whose short side is longer than
/// `max_short`. That cap is one the sizing sets itself, after an encode
/// that could not hold its rate at a larger picture, not one the user asked
/// for: unlike `Limits`, it moves no reference, so the loss of a smaller
/// picture is still measured from the source or the user's own ceiling.
/// The suggested size, when extreme, is found under the same cap.
pub fn choose_capped(
    src: &Source,
    target_bytes: u64,
    to: Format,
    limits: &Limits,
    max_short: Option<u32>,
    policy: &SizePolicy,
) -> SizedChoice {
    let mut c = evaluate(src, target_bytes, to, limits, max_short, policy);
    if c.extreme {
        c.suggested_bytes = suggest_target(src, target_bytes, to, limits, max_short, policy);
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
    max_short: Option<u32>,
    policy: &SizePolicy,
) -> Option<u64> {
    let fits = |bytes: u64| !evaluate(src, bytes, to, limits, max_short, policy).extreme;
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

    fn rate_le(a: (u32, u32), b: (u32, u32)) -> bool {
        u64::from(a.0) * u64::from(b.1) <= u64::from(b.0) * u64::from(a.1)
    }

    /// The chosen settings cannot tell an upscaled or sped-up candidate that
    /// was generated and lost from one never generated, so the steps
    /// themselves are checked: none exceeds the source or the user's bound,
    /// whatever odd bound is given, and none repeats.
    #[test]
    fn nothing_is_ever_upscaled_or_sped_up() {
        let src = source(1280, 720, (24, 1), 30, &[None]);
        for target in [500_000, 2_000_000, 50_000_000] {
            let c = pick(&src, target);
            assert!(c.width <= 1280 && c.height <= 720, "{c:?}");
            assert!(rate(c.fps) <= 24.0, "{c:?}");
        }

        let landscape = source(1920, 1080, (30, 1), 60, &[None]);
        let portrait = source(1080, 1920, (30, 1), 60, &[None]);
        let bounds = [
            None,
            Some((1280, 720)),
            Some((641, 361)),
            Some((853, 480)),
            Some((4000, 3000)),
            Some((1920, 1080)),
        ];
        for bound in bounds {
            for (src, bound) in [(&landscape, bound), (&portrait, bound.map(|(w, h)| (h, w)))] {
                let limits = Limits {
                    max_dims: bound,
                    max_fps: None,
                };
                let steps = resolution_steps(src, &limits);
                assert!(!steps.is_empty());
                for &(short, w, h) in &steps {
                    assert!(w <= src.width && h <= src.height, "{bound:?}: {steps:?}");
                    if let Some((bw, bh)) = bound {
                        assert!(w <= bw && h <= bh, "{bound:?}: {steps:?}");
                    }
                    assert_eq!(short, w.min(h), "{bound:?}: {steps:?}");
                }
                assert!(
                    steps.windows(2).all(|p| p[0].0 > p[1].0),
                    "each step is strictly smaller: {bound:?}: {steps:?}"
                );
            }
        }

        for fps in [(24, 1), (30_000, 1001), (1799, 60), (60, 1)] {
            let src = source(1920, 1080, fps, 60, &[None]);
            for cap in [
                None,
                Some((24, 1)),
                Some((25, 1)),
                Some((24_000, 1001)),
                Some((61, 1)),
                Some((240, 1)),
            ] {
                let limits = Limits {
                    max_dims: None,
                    max_fps: cap,
                };
                let steps = fps_steps(&src, &limits);
                assert!(!steps.is_empty());
                for &step in &steps {
                    assert!(rate_le(step, fps), "{fps:?} cap {cap:?}: {steps:?}");
                    if let Some(cap) = cap {
                        assert!(rate_le(step, cap), "{fps:?} cap {cap:?}: {steps:?}");
                    }
                }
                assert!(
                    steps.windows(2).all(|p| !rate_le(p[0], p[1])),
                    "each step is strictly slower: {fps:?} cap {cap:?}: {steps:?}"
                );
            }
        }
    }

    /// Review focus 2 (later): computed long sides round to the nearest even
    /// value, as ffmpeg's `-2` does.
    #[test]
    fn computed_dimensions_round_to_the_nearest_even_value() {
        let src = source(1920, 1080, (30, 1), 60, &[None]);
        let steps = resolution_steps(&src, &Limits::default());
        for want in [
            (1080, 1920, 1080),
            (720, 1280, 720),
            (540, 960, 540),
            (480, 854, 480),
            (360, 640, 360),
            (240, 426, 240),
            (144, 256, 144),
        ] {
            assert!(steps.contains(&want), "{want:?} missing from {steps:?}");
        }
        let portrait = source(1080, 1920, (30, 1), 60, &[None]);
        let steps = resolution_steps(&portrait, &Limits::default());
        assert!(steps.contains(&(480, 480, 854)), "{steps:?}");
        assert!(steps.contains(&(240, 240, 426)), "{steps:?}");
    }

    /// Review focus 2: the steps apply to the displayed short side, and a
    /// portrait source stays portrait.
    #[test]
    fn a_portrait_source_steps_its_short_side_and_stays_portrait() {
        let src = source(1080, 1920, (30, 1), 60, &[Some(128_000)]);
        for target in [3_000_000, 8_000_000, 40_000_000] {
            let c = pick(&src, target);
            assert!(c.width < c.height, "{c:?}");
            // The displayed short side is the width, and it lands on a step.
            assert!(
                [1080, 720, 540, 480, 360, 240, 144].contains(&c.width),
                "the width is a short-side step: {c:?}"
            );
        }
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

    /// A tight target, where the default choice certainly cuts both dials, so
    /// a weight that is ignored would leave the two choices equal and fail.
    #[test]
    fn raising_a_weight_protects_that_dial() {
        let src = source(1920, 1080, (60, 1), 60, &[Some(160_000)]);
        let target = 2_000_000;
        let base = pick(&src, target);
        assert!(rate(base.fps) < 60.0, "precondition: {base:?}");
        assert!(base.audio_kbps < Some(160), "precondition: {base:?}");
        let with =
            |policy: SizePolicy| choose(&src, target, Format::Mp4, &Limits::default(), &policy);
        let fps_first = with(SizePolicy {
            w_fps: 1000.0,
            ..SizePolicy::default()
        });
        assert!(
            rate(fps_first.fps) > rate(base.fps),
            "{base:?} vs {fps_first:?}"
        );
        let audio_first = with(SizePolicy {
            w_audio: 1000.0,
            ..SizePolicy::default()
        });
        assert!(
            audio_first.audio_kbps > base.audio_kbps,
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

        // Below every rung of the ladder the source's own rate is the only
        // step, not the ladder's lowest rung: AAC bottoms out at 32 kb/s and
        // Opus at 16.
        let low_aac = source(1280, 720, (30, 1), 60, &[Some(24_000)]);
        assert_eq!(pick(&low_aac, 500_000_000).audio_kbps, Some(24));
        let low_opus = source(1280, 720, (30, 1), 60, &[Some(12_000)]);
        let webm = choose(
            &low_opus,
            500_000_000,
            Format::Webm,
            &Limits::default(),
            &SizePolicy::default(),
        );
        assert_eq!(webm.audio_kbps, Some(12));

        // A nominal 128 kb/s track can report a hair under it and must keep
        // its 128 kb/s rung.
        let nominal = source(1280, 720, (30, 1), 60, &[Some(127_999)]);
        assert_eq!(pick(&nominal, 500_000_000).audio_kbps, Some(128));
    }

    /// A ceiling the user asked for is not a loss the budget chose: measured
    /// from the ceiling, a generous target leaves it costing nothing.
    #[test]
    fn a_binding_user_ceiling_is_not_charged_as_loss() {
        let src = source(3840, 2160, (30, 1), 60, &[Some(160_000)]);
        for limits in [
            Limits {
                max_dims: Some((854, 480)),
                max_fps: None,
            },
            Limits {
                max_dims: None,
                max_fps: Some((10, 1)),
            },
            Limits {
                max_dims: Some((854, 480)),
                max_fps: Some((10, 1)),
            },
        ] {
            let c = choose(
                &src,
                500_000_000_000,
                Format::Mp4,
                &limits,
                &SizePolicy::default(),
            );
            assert!(!c.extreme, "{limits:?}: {c:?}");
            assert!(c.cost() < 1.0, "{limits:?}: {c:?}");
            assert!(c.suggested_bytes.is_none(), "{limits:?}: {c:?}");
        }
    }

    /// A ceiling far below the source used to be charged as loss at every
    /// size, so the search for a target that is not extreme doubled past the
    /// range of the reserve arithmetic and overflowed.
    #[test]
    fn a_ceiling_far_below_the_source_does_not_overflow_the_search() {
        let src = source(3840, 2160, (30, 1), 60, &[Some(160_000)]);
        let dims = Limits {
            max_dims: Some((854, 480)),
            max_fps: None,
        };
        let c = choose(&src, 50_000_000, Format::Mp4, &dims, &SizePolicy::default());
        assert!(!c.extreme, "{c:?}");

        let fast = source(1920, 1080, (60, 1), 60, &[Some(160_000)]);
        let fps = Limits {
            max_dims: None,
            max_fps: Some((8, 1)),
        };
        let c = choose(&fast, 50_000_000, Format::Mp4, &fps, &SizePolicy::default());
        assert!(!c.extreme, "{c:?}");
    }

    /// No target can hold 16 kb/s over this duration, so the search for a
    /// size that is not extreme runs out of doublings: it must say so with
    /// `None`, not overflow the reserve arithmetic on the way.
    #[test]
    fn a_target_that_can_never_be_met_has_no_suggestion_and_no_overflow() {
        let src = Source {
            duration_ms: u64::MAX,
            ..source(1920, 1080, (30, 1), 0, &[Some(160_000)])
        };
        let c = pick(&src, 1_000_000);
        assert!(c.extreme, "{c:?}");
        assert!(c.over.is_some(), "{c:?}");
        assert_eq!(c.suggested_bytes, None, "{c:?}");
    }

    #[test]
    fn a_target_near_u64_max_does_not_overflow() {
        let src = source(1920, 1080, (30, 1), 60, &[Some(160_000)]);
        for target in [u64::MAX, u64::MAX / 2, u64::MAX / 30] {
            let c = pick(&src, target);
            assert!(!c.extreme, "{target}: {c:?}");
            assert_eq!((c.width, c.height), (1920, 1080), "{target}: {c:?}");
        }
        let mut with_extras = src.clone();
        with_extras.subtitle_tracks = 2;
        with_extras.attachment_bytes = u64::MAX;
        let bytes = payload_bytes(&with_extras, u64::MAX, Format::Mkv);
        assert_eq!(bytes, 0, "the reserve exceeds the target: nothing left");
        let c = choose(
            &with_extras,
            u64::MAX,
            Format::Mkv,
            &Limits::default(),
            &SizePolicy::default(),
        );
        assert!(c.extreme, "{c:?}");
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
    fn the_next_short_side_below_is_the_next_step_down() {
        for (short, want) in [
            (u32::MAX, Some(2160)),
            (2160, Some(1440)),
            (1080, Some(720)),
            (1079, Some(720)),
            (720, Some(540)),
            (700, Some(540)),
            (541, Some(540)),
            (540, Some(480)),
            (145, Some(144)),
            (144, None),
            (100, None),
            (0, None),
        ] {
            assert_eq!(next_short_side_below(short), want, "{short}");
        }
    }

    /// A cap the sizing sets itself leaves larger pictures out, but unlike
    /// the user's own `--resize` it is charged as loss: loss is still
    /// measured from the source.
    #[test]
    fn a_cap_leaves_out_larger_pictures_and_is_charged_as_loss() {
        let src = source(1920, 1080, (30, 1), 60, &[Some(160_000)]);
        let target = 500_000_000;
        let policy = SizePolicy::default();
        let open = pick(&src, target);
        assert_eq!((open.width, open.height), (1920, 1080), "{open:?}");
        let none = choose_capped(&src, target, Format::Mp4, &Limits::default(), None, &policy);
        assert_eq!(none, open, "no cap, no change");

        let capped = choose_capped(
            &src,
            target,
            Format::Mp4,
            &Limits::default(),
            Some(720),
            &policy,
        );
        assert_eq!((capped.width, capped.height), (1280, 720), "{capped:?}");
        let asked = choose(
            &src,
            target,
            Format::Mp4,
            &Limits {
                max_dims: Some((1280, 720)),
                max_fps: None,
            },
            &policy,
        );
        assert_eq!((asked.width, asked.height), (1280, 720), "{asked:?}");
        assert!(asked.cost() < 1.0, "the user's ceiling is free: {asked:?}");
        assert!(
            capped.cost() > asked.cost() + 1.0,
            "the sizing's own cap is not: {capped:?} vs {asked:?}"
        );
    }

    #[test]
    fn no_capped_choice_is_larger_than_its_cap() {
        let landscape = source(1920, 1080, (30, 1), 60, &[Some(160_000)]);
        let portrait = source(1080, 1920, (30, 1), 60, &[Some(160_000)]);
        for src in [&landscape, &portrait] {
            for cap in [1080, 720, 700, 540, 480, 360, 240, 144] {
                for target in [2_000_000, 10_000_000, 50_000_000, 500_000_000] {
                    let c = choose_capped(
                        src,
                        target,
                        Format::Mp4,
                        &Limits::default(),
                        Some(cap),
                        &SizePolicy::default(),
                    );
                    assert!(c.width.min(c.height) <= cap, "{cap} {target}: {c:?}");
                }
            }
            // A cap under every step still leaves the smallest one.
            let c = choose_capped(
                src,
                50_000_000,
                Format::Mp4,
                &Limits::default(),
                Some(100),
                &SizePolicy::default(),
            );
            assert_eq!(c.width.min(c.height), 144, "{c:?}");
        }
    }

    #[test]
    fn the_achieved_video_rate_is_the_file_less_its_audio_and_reserve() {
        // 1% of the file for the container and 160 kb/s of audio for 6 s:
        // (1_000_000 - 10_000) * 8 - 960_000 = 6_960_000 bits over 6 s.
        let src = source(1280, 720, (30, 1), 6, &[Some(160_000)]);
        assert_eq!(
            achieved_video_bps(&src, 1_000_000, Format::Mp4, Some(160)),
            1_160_000
        );
        // Every audio track counts, and so does a subtitle stream's
        // allowance: (2_000_000 - 20_000 - 100_000) * 8 - 3 * 960_000
        // = 12_160_000 bits over 6 s.
        let mut three = source(1280, 720, (30, 1), 6, &[Some(160_000); 3]);
        three.subtitle_tracks = 1;
        assert_eq!(
            achieved_video_bps(&three, 2_000_000, Format::Mkv, Some(160)),
            2_026_666
        );
        // A silent source loses nothing to audio.
        let silent = source(1280, 720, (30, 1), 6, &[]);
        assert_eq!(
            achieved_video_bps(&silent, 1_000_000, Format::Mp4, None),
            1_320_000
        );
        // A file the audio alone accounts for carries no video rate at all.
        assert_eq!(achieved_video_bps(&src, 100_000, Format::Mp4, Some(160)), 0);
        assert!(achieved_video_bps(&src, u64::MAX, Format::Mp4, Some(160)) > 0);
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

    #[test]
    fn from_probe_refuses_a_zero_dimension() {
        let mut p = MediaProbe {
            video_streams: 1,
            width: Some(0),
            height: Some(1080),
            frame_rate: Some((30, 1)),
            duration_ms: Some(1000),
            ..MediaProbe::default()
        };
        assert_eq!(Source::from_probe(&p), Err(SourceGap::NoDimensions));
        p.width = Some(1920);
        p.height = Some(0);
        assert_eq!(Source::from_probe(&p), Err(SourceGap::NoDimensions));
        p.height = Some(1080);
        assert!(Source::from_probe(&p).is_ok());
    }
}
