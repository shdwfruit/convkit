//! The `--max-size` layer: turns a probed source and a size target into a
//! plan, and holds the arithmetic and the wording the executor and the CLI
//! share. The choosing itself is `budget.rs`; this module is everything
//! around it.

use std::path::{Path, PathBuf};

use serde::{Serialize, Serializer};

use crate::budget::{self, Limits, Over, SizePolicy, SizedChoice, Source, SourceGap};
use crate::plan::{ConversionPlan, PlannedStep};
use crate::probe::MediaProbe;
use crate::size::{self, MaxSize, UnitFamily};
use crate::video::{self, ResolvedVideo};
use crate::{
    media, registry, Backend, ConvError, ErrorCode, Format, OutputMode, Resolver, Result, Tuning,
};

/// How a sized conversion gets under its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// The source already fits and is the target's own container: copied
    /// byte for byte.
    Copy,
    /// The source already fits and its video suits the target container:
    /// the video is stream-copied and any audio track the target cannot hold
    /// is re-encoded; the whole file is then re-encoded only if the result
    /// comes out over.
    Remux,
    /// Two-pass encode at the chosen settings, retried while over.
    Encode,
}

/// What a sized plan decided, carried on `ConversionPlan` so `--dry-run`,
/// the executor and the CLI's confirmation prompt all see the same answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SizingPlan {
    pub target_bytes: u64,
    /// The target as the user wrote it, for sentences: `10 MB`.
    pub target_label: String,
    pub family: UnitFamily,
    pub strategy: Strategy,
    /// The settings chosen, for `Strategy::Encode`.
    pub choice: Option<SizedChoice>,
    /// The extreme or predicted-over sentence, when the choice is extreme.
    pub warning: Option<String>,
    /// When extreme: the suggested target's spelling (`40mb`).
    pub suggested: Option<String>,
    /// The source file's size, as ffprobe reported it.
    pub source_bytes: Option<u64>,
}

/// What a sized conversion actually did, on `Outcome` and in `--json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SizingReport {
    pub target_bytes: u64,
    pub family: UnitFamily,
    pub strategy: Strategy,
    pub width: Option<u32>,
    pub height: Option<u32>,
    #[serde(serialize_with = "optional_rational")]
    pub fps: Option<(u32, u32)>,
    pub video_bps: Option<u64>,
    /// One entry per audio track, bits per second.
    pub audio_bps: Vec<u64>,
    /// Pass-2 runs for an encode (1 = fitted first time); 1 for a remux;
    /// 0 for a copy.
    pub attempts: u32,
    pub cost: Option<f64>,
    pub over_target: bool,
    pub suggested: Option<String>,
}

// Spelled `std::result::Result` so a later import of the crate's
// one-parameter `Result` alias into this module cannot shadow it.
fn optional_rational<S: Serializer>(
    r: &Option<(u32, u32)>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    match r {
        Some((n, d)) => s.serialize_str(&format!("{n}/{d}")),
        None => s.serialize_none(),
    }
}

/// The containers `--max-size` can size: the four video targets.
pub fn is_video_target(to: Format) -> bool {
    matches!(to, Format::Mp4 | Format::Mov | Format::Mkv | Format::Webm)
}

/// Pass 2 runs at most this many times.
pub const MAX_ATTEMPTS: u32 = 3;

/// Everything `encode` needs, validated once.
struct Prepared<'a> {
    from: Format,
    to: Format,
    inputs: &'a [PathBuf],
    output: &'a Path,
    probe: &'a MediaProbe,
    src: Source,
    limits: Limits,
    max: &'a MaxSize,
}

/// The plan for a `--max-size` conversion. Pure: the probe is the caller's.
pub(crate) fn plan(
    from: Format,
    to: Format,
    inputs: &[PathBuf],
    output: &Path,
    probe: Option<&MediaProbe>,
    tuning: &Tuning,
    max: &MaxSize,
) -> Result<ConversionPlan> {
    let p = prepare(from, to, inputs, output, probe, tuning, max)?;
    let mut sizing = new_sizing(&p, Strategy::Encode);
    let src_rate = p.src.fps;
    let caps_bind = p
        .limits
        .max_dims
        .is_some_and(|d| d != (p.src.width, p.src.height))
        || p.limits.max_fps.is_some_and(|r| {
            u64::from(r.0) * u64::from(src_rate.1) < u64::from(src_rate.0) * u64::from(r.1)
        });
    if !caps_bind && p.probe.size_bytes.is_some_and(|b| b <= max.bytes) {
        if from == to {
            sizing.strategy = Strategy::Copy;
            return Ok(ConversionPlan {
                from,
                to,
                inputs: inputs.to_vec(),
                output: output.to_path_buf(),
                steps: Vec::new(),
                warnings: Vec::new(),
                sizing: Some(sizing),
            });
        }
        if let Some(m) = media::stream_mapped_invocation(to, p.probe, &inputs[0], output) {
            sizing.strategy = Strategy::Remux;
            return Ok(ConversionPlan {
                from,
                to,
                inputs: inputs.to_vec(),
                output: output.to_path_buf(),
                steps: vec![ffmpeg_step(m.argv, OutputMode::Path, output.to_path_buf())],
                warnings: m.warnings,
                sizing: Some(sizing),
            });
        }
    }
    encode(&p, sizing)
}

/// Always an encode: the executor's fallback when a remux of a source that
/// looked small enough came out over.
pub(crate) fn encode_plan(
    from: Format,
    to: Format,
    inputs: &[PathBuf],
    output: &Path,
    probe: Option<&MediaProbe>,
    tuning: &Tuning,
    max: &MaxSize,
) -> Result<ConversionPlan> {
    let p = prepare(from, to, inputs, output, probe, tuning, max)?;
    let sizing = new_sizing(&p, Strategy::Encode);
    encode(&p, sizing)
}

fn new_sizing(p: &Prepared<'_>, strategy: Strategy) -> SizingPlan {
    SizingPlan {
        target_bytes: p.max.bytes,
        target_label: p.max.label(),
        family: p.max.family,
        strategy,
        choice: None,
        warning: None,
        suggested: None,
        source_bytes: p.probe.size_bytes,
    }
}

fn prepare<'a>(
    from: Format,
    to: Format,
    inputs: &'a [PathBuf],
    output: &'a Path,
    probe: Option<&'a MediaProbe>,
    tuning: &Tuning,
    max: &'a MaxSize,
) -> Result<Prepared<'a>> {
    if !is_video_target(to) {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "--max-size does not apply to {} -> {}: it sizes video targets (mp4, mov, mkv, webm)",
                from.ext(),
                to.ext()
            ),
        ));
    }
    if from != to && registry::lookup(from, to).is_none() {
        return Err(ConvError::unsupported_pair(from, to));
    }
    if tuning.crf.is_some() {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            "--crf and --max-size ask for different things (constant quality vs a size); use one",
        ));
    }
    for (flag, given) in [
        ("--quality", tuning.quality.is_some()),
        ("--colors", tuning.colors.is_some()),
    ] {
        if given {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!(
                    "{flag} does not apply to {} -> {}: it tunes image conversions",
                    from.ext(),
                    to.ext()
                ),
            ));
        }
    }
    let input = &inputs[0];
    let probe = probe.ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!(
                "--max-size needs ffprobe to read {}; install ffprobe or check that it runs",
                input.display()
            ),
        )
    })?;
    let src = Source::from_probe(probe).map_err(|gap| gap_error(gap, input))?;
    // A rate that cannot be held exactly is refused: dropping it would leave
    // the conversion with no ceiling the user asked for.
    let max_fps = tuning
        .fps
        .as_deref()
        .map(|v| {
            video::parse_rate(v).ok_or_else(|| {
                ConvError::new(
                    ErrorCode::InvalidInvocation,
                    format!(
                        "--fps {v} cannot be used with --max-size; write it as N/D, e.g. 24000/1001"
                    ),
                )
            })
        })
        .transpose()?;
    let limits = Limits {
        max_dims: tuning
            .resize
            .as_deref()
            .map(|g| video::fit_within(g, (src.width, src.height))),
        max_fps,
    };
    Ok(Prepared {
        from,
        to,
        inputs,
        output,
        probe,
        src,
        limits,
        max,
    })
}

fn gap_error(gap: SourceGap, input: &Path) -> ConvError {
    let name = input.display();
    match gap {
        SourceGap::NoVideo => ConvError::new(
            ErrorCode::InvalidInvocation,
            format!("{name} has no video stream; --max-size applies to video"),
        ),
        SourceGap::NoDuration => ConvError::new(
            ErrorCode::ConversionFailed,
            format!("cannot read the duration of {name}; --max-size needs it to set a bitrate"),
        ),
        SourceGap::NoDimensions => ConvError::new(
            ErrorCode::ConversionFailed,
            format!(
                "cannot read the picture size of {name}; --max-size needs it to choose a resolution"
            ),
        ),
        SourceGap::NoFrameRate => ConvError::new(
            ErrorCode::ConversionFailed,
            format!("cannot read the frame rate of {name}; --max-size needs it to choose one"),
        ),
    }
}

fn encode(p: &Prepared<'_>, mut sizing: SizingPlan) -> Result<ConversionPlan> {
    let choice = budget::choose(&p.src, p.max.bytes, p.to, &p.limits, &SizePolicy::default());
    let resolved = ResolvedVideo {
        fps: (choice.fps != p.src.fps).then(|| format!("{}/{}", choice.fps.0, choice.fps.1)),
        scale: ((choice.width, choice.height) != (p.src.width & !1, p.src.height & !1))
            .then(|| format!("scale=w={}:h={}", choice.width, choice.height)),
        notes: Vec::new(),
    };
    let passlog = p.output.with_extension("convkit-pass");
    let two = media::two_pass_invocations(
        p.to,
        p.probe,
        &resolved,
        choice.video_bps,
        choice.audio_kbps,
        &passlog,
        &p.inputs[0],
        p.output,
    )
    .ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!(
                "cannot build a two-pass encode for {}; the probe found no video codec to encode",
                p.inputs[0].display()
            ),
        )
    })?;
    let mut warnings = two.pass2.warnings;
    // GIF's static recipes carry the one fact the stream mapping cannot
    // know: a looping GIF becomes a single play.
    if p.from == Format::Gif {
        if let Some(r) = registry::lookup(p.from, p.to) {
            warnings.extend(r.warnings.iter().map(|w| (*w).to_string()));
        }
    }
    if choice.extreme {
        sizing.warning = Some(extreme_sentence(&p.src, &sizing, &choice));
        sizing.suggested = choice
            .suggested_bytes
            .map(|b| size::round_up(b, p.max.family).spelling);
    }
    sizing.choice = Some(choice);
    Ok(ConversionPlan {
        from: p.from,
        to: p.to,
        inputs: p.inputs.to_vec(),
        output: p.output.to_path_buf(),
        steps: vec![
            ffmpeg_step(two.pass1, OutputMode::Discard, passlog),
            ffmpeg_step(two.pass2.argv, OutputMode::Path, p.output.to_path_buf()),
        ],
        warnings,
        sizing: Some(sizing),
    })
}

/// An ffmpeg step with its path positions recorded for the Windows
/// long-path rewriter: the input (always argv[1]), the pass log, and the
/// output for a step that writes one. Pass 1 ends in `-`, which is not a
/// path.
fn ffmpeg_step(argv: Vec<String>, mode: OutputMode, output: PathBuf) -> PlannedStep {
    let mut path_args = vec![1];
    // Searched from argv[2]: argv[1] is the input, whose name may look like
    // anything. mkv scopes the flag to the video stream.
    let log_flag = argv
        .iter()
        .skip(2)
        .position(|a| a == "-passlogfile" || a == "-passlogfile:v:0");
    if let Some(i) = log_flag {
        path_args.push(i + 3);
    }
    if mode == OutputMode::Path {
        path_args.push(argv.len() - 1);
    }
    PlannedStep {
        backend: Backend::Ffmpeg,
        program: Backend::Ffmpeg.exe_name().to_string(),
        argv,
        output_mode: mode,
        output,
        intermediate_ext: None,
        path_args,
    }
}

/// Probes and plans without running anything, for the CLI's confirmation
/// prompt. `None` when no `--max-size` applies.
pub fn preview(
    from: Format,
    to: Format,
    input: &Path,
    tuning: &Tuning,
    resolver: &Resolver,
) -> Result<Option<SizingPlan>> {
    let Some(max) = tuning.max_size.as_ref() else {
        return Ok(None);
    };
    if !is_video_target(to) {
        return Ok(None);
    }
    let ffprobe = resolver.resolve(Backend::Ffprobe)?;
    let probe = crate::probe::run(&ffprobe.path, input)?;
    let out = input.with_extension(format!("preview.{}", to.ext()));
    Ok(plan(
        from,
        to,
        &[input.to_path_buf()],
        &out,
        Some(&probe),
        tuning,
        max,
    )?
    .sizing)
}

pub fn confirmation_error(input: &Path, sizing: &SizingPlan) -> ConvError {
    let hint = sizing
        .suggested
        .as_deref()
        .map(|s| format!(", or try --max-size {s}"))
        .unwrap_or_default();
    ConvError::new(
        ErrorCode::ConfirmationRequired,
        format!(
            "extreme compression not confirmed for {}; pass --yes to convert anyway{hint}",
            input.display()
        ),
    )
}

/// The next pass-2 bitrate after a result of `measured` bytes: aim at the
/// margin again, 2% lower for every attempt so far, never at or above the
/// rate that overshot and never below the encoder's floor. `None` when no
/// such rate exists, that is when `video_bps` is already at the floor, so a
/// retry would only repeat the encode that overshot.
pub(crate) fn retry_bitrate(
    video_bps: u64,
    target: u64,
    measured: u64,
    attempts: u32,
) -> Option<u64> {
    let ceiling = video_bps.saturating_sub(1);
    if ceiling < budget::MIN_VIDEO_BPS {
        return None;
    }
    let aim = target as f64
        * (1.0 - budget::MARGIN_PERMILLE as f64 / 1000.0)
        * (1.0 - 0.02 * f64::from(attempts));
    let next = video_bps as f64 * aim / measured.max(1) as f64;
    Some((next as u64).clamp(budget::MIN_VIDEO_BPS, ceiling))
}

/// Pass 2 with its bitrate replaced; every other token unchanged.
pub(crate) fn with_bitrate(step: &PlannedStep, bps: u64) -> PlannedStep {
    let mut s = step.clone();
    if let Some(i) = s.argv.iter().position(|a| a == "-b:v" || a == "-b:v:0") {
        s.argv[i + 1] = bps.to_string();
    }
    s
}

pub(crate) fn report(
    sizing: &SizingPlan,
    bytes: u64,
    attempts: u32,
    audio_tracks: usize,
) -> SizingReport {
    let c = sizing.choice.as_ref();
    SizingReport {
        target_bytes: sizing.target_bytes,
        family: sizing.family,
        strategy: sizing.strategy,
        width: c.map(|c| c.width),
        height: c.map(|c| c.height),
        fps: c.map(|c| c.fps),
        video_bps: c.map(|c| c.video_bps),
        audio_bps: c
            .and_then(|c| c.audio_kbps)
            .map(|k| vec![u64::from(k) * 1000; audio_tracks])
            .unwrap_or_default(),
        attempts,
        cost: c.map(SizedChoice::cost),
        over_target: bytes > sizing.target_bytes,
        suggested: sizing.suggested.clone(),
    }
}

// --- Wording ---------------------------------------------------------------

/// The overshoot, when the smallest possible file is predicted over the
/// target itself. A choice is also flagged `over` when it missed only the
/// target less its safety margin; its predicted file carries no margin and
/// can still fit, so that is not a claim the target cannot be met.
fn overshoot(c: &SizedChoice, target_bytes: u64) -> Option<Over> {
    c.over.filter(|o| o.predicted_bytes > target_bytes)
}

/// Whether this plan's `warning` is the "Could not get under" sentence: the
/// smallest possible file is predicted over the target. False for a plan
/// with no warning, and for one whose warning only says the picture will
/// look poor.
pub(crate) fn predicts_over_target(sizing: &SizingPlan) -> bool {
    sizing
        .choice
        .as_ref()
        .is_some_and(|c| overshoot(c, sizing.target_bytes).is_some())
}

fn extreme_sentence(src: &Source, sizing: &SizingPlan, c: &SizedChoice) -> String {
    let short = c.width.min(c.height);
    match overshoot(c, sizing.target_bytes) {
        Some(over) => {
            let cause = if over.audio_bytes >= sizing.target_bytes {
                format!(
                    "audio alone needs {}",
                    size::display(over.audio_bytes, sizing.family)
                )
            } else {
                format!("at {short}p, {} fps", fps_words(c.fps))
            };
            format!(
                "Could not get under {}: the smallest possible is about {} ({cause}).",
                sizing.target_label,
                size::display(over.predicted_bytes, sizing.family)
            )
        }
        None => format!(
            "Extreme compression: {} for {} of {}p will look poor ({short}p, {} fps).",
            sizing.target_label,
            duration_words(src.duration_ms),
            src.short_side(),
            fps_words(c.fps)
        ),
    }
}

pub(crate) fn summary_note(r: &SizingReport) -> String {
    let mut parts = Vec::new();
    if let (Some(w), Some(h), Some(fps)) = (r.width, r.height, r.fps) {
        parts.push(format!("{w}x{h} at {} fps", fps_words(fps)));
    }
    if let Some(v) = r.video_bps {
        parts.push(format!("{} video", bps_words(v)));
    }
    if let Some(a) = r.audio_bps.first() {
        parts.push(format!("{} audio", bps_words(*a)));
    }
    let passes = match r.attempts {
        0 | 1 => "2 passes".to_string(),
        n => format!(
            "{} passes ({} {})",
            n + 1,
            n - 1,
            if n == 2 { "retry" } else { "retries" }
        ),
    };
    format!("Sized to {}; {passes}.", parts.join(", "))
}

pub(crate) fn already_small_note(s: &SizingPlan) -> String {
    // A remux keeps the video as it is but may re-encode an audio track the
    // target cannot hold, so it never claims the whole file was copied; the
    // stream mapping's own warnings name any track it re-encoded.
    let how = if s.strategy == Strategy::Copy {
        "copied without re-encoding"
    } else {
        "the video was stream-copied, not re-encoded"
    };
    match s.source_bytes {
        Some(b) => format!(
            "Already {}, under {}; {how}.",
            size::display(b, s.family),
            s.target_label
        ),
        None => format!("Already under {}; {how}.", s.target_label),
    }
}

pub(crate) fn measured_over_sentence(s: &SizingPlan, bytes: u64, attempts: u32) -> String {
    // The loop can stop after one attempt, so the noun has to agree.
    let tries = if attempts == 1 {
        "1 attempt".to_string()
    } else {
        format!("{attempts} attempts")
    };
    format!(
        "Could not get under {} after {tries}: the result is {}.",
        s.target_label,
        size::display(bytes, s.family)
    )
}

fn duration_words(ms: u64) -> String {
    let secs = (ms / 1000).max(1);
    if secs < 90 {
        return format!("{secs} s");
    }
    let mins = (secs + 30) / 60;
    match (mins / 60, mins % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

fn fps_words((n, d): (u32, u32)) -> String {
    let v = f64::from(n) / f64::from(d);
    if (v - v.round()).abs() < 0.005 {
        format!("{}", v.round() as u64)
    } else {
        format!("{v:.2}").trim_end_matches('0').to_string()
    }
}

fn bps_words(bps: u64) -> String {
    if bps >= 1_000_000 {
        format!("{:.2} Mb/s", bps as f64 / 1_000_000.0)
    } else {
        format!("{} kb/s", bps / 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    use crate::plan::build_tuned;
    use crate::probe::MediaProbe;

    fn probe(secs: u64, file_bytes: u64) -> MediaProbe {
        MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            width: Some(1920),
            height: Some(1080),
            frame_rate: Some((30, 1)),
            duration_ms: Some(secs * 1000),
            size_bytes: Some(file_bytes),
            audio_codecs: vec!["aac".into()],
            audio_bitrates: vec![Some(160_000)],
            ..MediaProbe::default()
        }
    }

    fn tuned(size: &str) -> Tuning {
        Tuning {
            max_size: Some(crate::size::parse(size).unwrap()),
            ..Default::default()
        }
    }

    fn build(from: Format, to: Format, p: &MediaProbe, t: &Tuning) -> Result<ConversionPlan> {
        build_tuned(
            from,
            to,
            &[PathBuf::from("in.mp4")],
            Path::new("/s/out.mp4"),
            Some(p),
            None,
            t,
        )
    }

    fn has(argv: &[String], pair: [&str; 2]) -> bool {
        argv.windows(2).any(|w| w == pair)
    }

    #[test]
    fn only_the_four_video_containers_are_sized() {
        for f in [Format::Mp4, Format::Mov, Format::Mkv, Format::Webm] {
            assert!(is_video_target(f), "{f:?}");
        }
        for f in [Format::Gif, Format::Avi, Format::Mp3, Format::Jpg] {
            assert!(!is_video_target(f), "{f:?}");
        }
    }

    fn sample_report(fps: Option<(u32, u32)>) -> SizingReport {
        SizingReport {
            target_bytes: 10_000_000,
            family: UnitFamily::Decimal,
            strategy: Strategy::Encode,
            width: Some(1280),
            height: Some(720),
            fps,
            video_bps: Some(900_000),
            audio_bps: vec![96_000],
            attempts: 1,
            cost: Some(1.5),
            over_target: false,
            suggested: None,
        }
    }

    /// `--json` output is a published contract: the rate is an `N/D` string,
    /// the enums are snake_case, and an unknown rate is `null`, not absent.
    #[test]
    fn a_sizing_report_serialises_in_its_json_shape() {
        let v = serde_json::to_value(sample_report(Some((30_000, 1_001)))).unwrap();
        assert_eq!(v["fps"], "30000/1001", "{v}");
        assert_eq!(v["strategy"], "encode", "{v}");
        assert_eq!(v["family"], "decimal", "{v}");

        let v = serde_json::to_value(sample_report(None)).unwrap();
        assert!(v.get("fps").is_some_and(|f| f.is_null()), "{v}");
    }

    #[test]
    fn an_oversized_source_gets_two_passes() {
        let plan = build(
            Format::Mp4,
            Format::Mp4,
            &probe(60, 50_000_000),
            &tuned("10mb"),
        )
        .unwrap();
        let sizing = plan.sizing.as_ref().unwrap();
        assert_eq!(sizing.strategy, Strategy::Encode);
        assert!(sizing.choice.is_some());
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].output_mode, OutputMode::Discard);
        assert!(has(&plan.steps[0].argv, ["-pass", "1"]));
        assert_eq!(plan.steps[1].output_mode, OutputMode::Path);
        assert!(has(&plan.steps[1].argv, ["-pass", "2"]));
    }

    /// The pass log sits beside the output in the scratch directory, and the
    /// long-path rewriter must see it.
    #[test]
    fn the_pass_log_is_a_registered_path_in_both_passes() {
        let plan = build(
            Format::Mp4,
            Format::Mp4,
            &probe(60, 50_000_000),
            &tuned("10mb"),
        )
        .unwrap();
        for step in &plan.steps {
            let logged = step
                .path_args
                .iter()
                .any(|&i| step.argv[i] == "/s/out.convkit-pass");
            assert!(logged, "{:?} {:?}", step.path_args, step.argv);
            assert!(step.path_args.contains(&1), "the input");
        }
        let last = plan.steps[1].argv.len() - 1;
        assert!(plan.steps[1].path_args.contains(&last), "the output");
        assert!(
            !plan.steps[0]
                .path_args
                .contains(&(plan.steps[0].argv.len() - 1)),
            "`-` is not a path"
        );
    }

    #[test]
    fn a_source_already_under_the_target_in_its_own_container_is_copied() {
        let plan = build(
            Format::Mp4,
            Format::Mp4,
            &probe(60, 6_000_000),
            &tuned("10mb"),
        )
        .unwrap();
        assert_eq!(plan.sizing.unwrap().strategy, Strategy::Copy);
        assert!(plan.steps.is_empty());
    }

    #[test]
    fn a_small_source_whose_streams_fit_another_container_is_remuxed() {
        let plan = build(
            Format::Mkv,
            Format::Mp4,
            &probe(60, 6_000_000),
            &tuned("10mb"),
        )
        .unwrap();
        assert_eq!(plan.sizing.unwrap().strategy, Strategy::Remux);
        assert_eq!(plan.steps.len(), 1);
        assert!(has(&plan.steps[0].argv, ["-c:v", "copy"]));
    }

    /// The caps are a request, so a small file is encoded.
    #[test]
    fn binding_caps_force_an_encode_even_when_the_source_fits() {
        let t = Tuning {
            fps: Some("15".into()),
            ..tuned("10mb")
        };
        let plan = build(Format::Mp4, Format::Mp4, &probe(60, 6_000_000), &t).unwrap();
        let sizing = plan.sizing.unwrap();
        assert_eq!(sizing.strategy, Strategy::Encode);
        let c = sizing.choice.unwrap();
        assert!(u64::from(c.fps.0) <= 15 * u64::from(c.fps.1), "{c:?}");
    }

    #[test]
    fn a_rotated_source_is_scaled_in_its_displayed_orientation() {
        let p = MediaProbe {
            rotation: Some(90),
            ..probe(600, 900_000_000)
        };
        let plan = build(Format::Mov, Format::Mp4, &p, &tuned("20mb")).unwrap();
        let c = plan.sizing.unwrap().choice.unwrap();
        assert!(c.width < c.height, "displayed portrait: {c:?}");
        let chain = plan.steps[1]
            .argv
            .iter()
            .find(|a| a.contains("scale=w="))
            .unwrap();
        assert!(
            chain.contains(&format!("scale=w={}:h={}", c.width, c.height)),
            "{chain}"
        );
    }

    #[test]
    fn refusals_name_the_flag_and_the_fix() {
        let p = probe(60, 50_000_000);
        let e = build(Format::Mp4, Format::Gif, &p, &tuned("10mb")).unwrap_err();
        assert!(
            e.message
                .starts_with("--max-size does not apply to mp4 -> gif"),
            "{}",
            e.message
        );
        let crf = Tuning {
            crf: Some(20),
            ..tuned("10mb")
        };
        let e = build(Format::Mp4, Format::Mp4, &p, &crf).unwrap_err();
        assert!(e.message.contains("--crf and --max-size"), "{}", e.message);
        let silent_picture = MediaProbe {
            video_streams: 0,
            ..p.clone()
        };
        let e = build(Format::Mp4, Format::Mp4, &silent_picture, &tuned("10mb")).unwrap_err();
        assert!(e.message.contains("has no video stream"), "{}", e.message);
        let no_duration = MediaProbe {
            duration_ms: None,
            ..p.clone()
        };
        let e = build(Format::Mp4, Format::Mp4, &no_duration, &tuned("10mb")).unwrap_err();
        assert!(
            e.message.contains("cannot read the duration"),
            "{}",
            e.message
        );
        let e = build_tuned(
            Format::Mp4,
            Format::Mp4,
            &[PathBuf::from("in.mp4")],
            Path::new("o.mp4"),
            None,
            None,
            &tuned("10mb"),
        )
        .unwrap_err();
        assert!(e.message.contains("needs ffprobe"), "{}", e.message);
    }

    #[test]
    fn an_extreme_target_carries_a_warning_and_a_suggestion() {
        let plan = build(
            Format::Mp4,
            Format::Mp4,
            &probe(45 * 60, 900_000_000),
            &tuned("5mb"),
        )
        .unwrap();
        let s = plan.sizing.unwrap();
        assert!(s.choice.as_ref().unwrap().extreme);
        let w = s.warning.unwrap();
        assert!(w.starts_with("Could not get under 5 MB"), "{w}");
        assert!(w.ends_with('.'), "warning register: {w}");
        assert!(s.suggested.unwrap().ends_with("mb"));
    }

    /// "Could not get under" is a claim about the file, so it is made only
    /// when even the smallest possible file is predicted over the target. A
    /// choice that missed only the target less its safety margin (1.21 MB
    /// against a 1.231 MB target) says the picture will look poor instead.
    #[test]
    fn the_extreme_sentence_says_could_not_fit_only_when_the_smallest_file_is_over() {
        let over = build(
            Format::Mp4,
            Format::Mp4,
            &probe(45 * 60, 900_000_000),
            &tuned("5mb"),
        )
        .unwrap()
        .sizing
        .unwrap();
        let w = over.warning.unwrap();
        assert!(
            w.starts_with("Could not get under 5 MB: the smallest possible is about "),
            "{w}"
        );

        let silent = MediaProbe {
            audio_codecs: Vec::new(),
            audio_bitrates: Vec::new(),
            ..probe(600, 900_000_000)
        };
        let margin = build(Format::Mp4, Format::Mp4, &silent, &tuned("1231kb"))
            .unwrap()
            .sizing
            .unwrap();
        let predicted = margin
            .choice
            .as_ref()
            .unwrap()
            .over
            .unwrap()
            .predicted_bytes;
        assert!(predicted < margin.target_bytes, "{predicted}");
        let w = margin.warning.unwrap();
        assert!(
            w.starts_with("Extreme compression: 1231 KB for 10 min of 1080p will look poor"),
            "{w}"
        );
        assert!(!w.contains("Could not get under"), "{w}");
    }

    #[test]
    fn a_retry_aims_lower_and_never_below_the_floor() {
        let next = retry_bitrate(1_000_000, 10_000_000, 10_500_000, 1).unwrap();
        assert!(next < 1_000_000);
        assert!(next > 800_000, "{next}");
        assert_eq!(
            retry_bitrate(20_000, 10_000_000, 90_000_000, 2),
            Some(budget::MIN_VIDEO_BPS)
        );
    }

    /// A retry that would land on the rate that just overshot is no retry:
    /// at the encoder's floor there is nowhere lower to go.
    #[test]
    fn a_retry_at_the_floor_has_nowhere_lower_to_go() {
        let floor = budget::MIN_VIDEO_BPS;
        assert_eq!(retry_bitrate(floor, 10_000_000, 90_000_000, 1), None);
        assert_eq!(retry_bitrate(floor - 1, 10_000_000, 90_000_000, 1), None);
        assert_eq!(retry_bitrate(0, 10_000_000, 90_000_000, 1), None);
        // One step above the floor still has exactly one rate below it.
        assert_eq!(
            retry_bitrate(floor + 1, 10_000_000, 90_000_000, 1),
            Some(floor)
        );
        for (bps, measured) in [(floor + 1, 1), (50_000, 10_000_001), (1_000_000, 1)] {
            let next = retry_bitrate(bps, 10_000_000, measured, 1).unwrap();
            assert!(floor <= next && next < bps, "{bps} -> {next}");
        }
    }

    #[test]
    fn with_bitrate_rewrites_exactly_the_rate_token() {
        let plan = build(
            Format::Mp4,
            Format::Mp4,
            &probe(60, 50_000_000),
            &tuned("10mb"),
        )
        .unwrap();
        let s = with_bitrate(&plan.steps[1], 123_456);
        assert!(has(&s.argv, ["-b:v", "123456"]));
        assert_eq!(s.argv.len(), plan.steps[1].argv.len());
    }

    #[test]
    fn the_notes_read_as_sentences() {
        let r = SizingReport {
            target_bytes: 10_000_000,
            family: crate::size::UnitFamily::Decimal,
            strategy: Strategy::Encode,
            width: Some(1280),
            height: Some(720),
            fps: Some((30_000, 1001)),
            video_bps: Some(1_190_000),
            audio_bps: vec![96_000],
            attempts: 2,
            cost: Some(18.4),
            over_target: false,
            suggested: None,
        };
        assert_eq!(
            summary_note(&r),
            "Sized to 1280x720 at 29.97 fps, 1.19 Mb/s video, 96 kb/s audio; 3 passes (1 retry)."
        );
        assert_eq!(duration_words(45 * 60 * 1000), "45 min");
        assert_eq!(duration_words(42_000), "42 s");
        assert_eq!(duration_words(3_900_000), "1 h 5 min");
    }

    fn plan_for(from: Format, to: Format, out: &str, p: &MediaProbe, t: &Tuning) -> ConversionPlan {
        build_tuned(
            from,
            to,
            &[PathBuf::from("in.mkv")],
            Path::new(out),
            Some(p),
            None,
            t,
        )
        .unwrap()
    }

    #[test]
    fn a_source_that_fits_is_still_encoded_by_the_fallback_planner() {
        let max = crate::size::parse("10mb").unwrap();
        let plan = encode_plan(
            Format::Mp4,
            Format::Mp4,
            &[PathBuf::from("in.mp4")],
            Path::new("/s/out.mp4"),
            Some(&probe(60, 6_000_000)),
            &tuned("10mb"),
            &max,
        )
        .unwrap();
        assert_eq!(plan.sizing.as_ref().unwrap().strategy, Strategy::Encode);
        assert_eq!(plan.steps.len(), 2);
        let refused = encode_plan(
            Format::Mp4,
            Format::Mp4,
            &[PathBuf::from("in.mp4")],
            Path::new("/s/out.mp4"),
            None,
            &tuned("10mb"),
            &max,
        )
        .unwrap_err();
        assert!(
            refused.message.contains("needs ffprobe"),
            "{}",
            refused.message
        );
    }

    #[test]
    fn a_resize_that_does_not_bind_leaves_a_small_source_copied() {
        let t = Tuning {
            resize: Some("4000x3000".into()),
            ..tuned("10mb")
        };
        let plan = build(Format::Mp4, Format::Mp4, &probe(60, 6_000_000), &t).unwrap();
        assert_eq!(plan.sizing.unwrap().strategy, Strategy::Copy);
    }

    #[test]
    fn a_resize_that_binds_caps_the_chosen_picture_and_forces_an_encode() {
        let t = Tuning {
            resize: Some("640x".into()),
            ..tuned("10mb")
        };
        let plan = build(Format::Mp4, Format::Mp4, &probe(60, 6_000_000), &t).unwrap();
        let sizing = plan.sizing.unwrap();
        assert_eq!(sizing.strategy, Strategy::Encode);
        let c = sizing.choice.unwrap();
        assert!(c.width <= 640 && c.height <= 360, "{c:?}");
        let chain = plan.steps[1]
            .argv
            .iter()
            .find(|a| a.contains("scale=w="))
            .unwrap();
        assert!(
            chain.contains(&format!("scale=w={}:h={}", c.width, c.height)),
            "{chain}"
        );
    }

    #[test]
    fn a_binding_fps_reaches_ffmpeg_as_an_exact_rational() {
        let t = Tuning {
            fps: Some("15".into()),
            ..tuned("10mb")
        };
        let plan = build(Format::Mp4, Format::Mp4, &probe(60, 6_000_000), &t).unwrap();
        let c = plan.sizing.unwrap().choice.unwrap();
        let want = format!("fps={}/{}", c.fps.0, c.fps.1);
        assert!(
            plan.steps
                .iter()
                .all(|s| s.argv.iter().any(|a| a.starts_with(&want))),
            "{want} in {:?}",
            plan.steps
        );
    }

    #[test]
    fn an_mkv_pass_log_is_registered_under_its_stream_scoped_flag() {
        let plan = plan_for(
            Format::Mkv,
            Format::Mkv,
            "/s/out.mkv",
            &probe(60, 50_000_000),
            &tuned("10mb"),
        );
        assert_eq!(plan.steps.len(), 2);
        for step in &plan.steps {
            assert!(has(&step.argv, ["-passlogfile:v:0", "/s/out.convkit-pass"]));
            assert!(
                step.path_args
                    .iter()
                    .any(|&i| step.argv[i] == "/s/out.convkit-pass"),
                "{:?} {:?}",
                step.path_args,
                step.argv
            );
        }
        let s = with_bitrate(&plan.steps[1], 77_000);
        assert!(has(&s.argv, ["-b:v:0", "77000"]));
        assert_eq!(s.argv.len(), plan.steps[1].argv.len());
    }

    #[test]
    fn a_webm_plan_sizes_with_vp9_and_opus_at_the_rate_it_chose() {
        let plan = plan_for(
            Format::Mp4,
            Format::Webm,
            "/s/out.webm",
            &probe(60, 50_000_000),
            &tuned("10mb"),
        );
        let c = plan.sizing.as_ref().unwrap().choice.clone().unwrap();
        let pass2 = &plan.steps[1].argv;
        assert!(has(pass2, ["-c:v", "libvpx-vp9"]), "{pass2:?}");
        assert!(has(pass2, ["-b:v", &c.video_bps.to_string()]), "{pass2:?}");
        assert!(has(pass2, ["-c:a", "libopus"]), "{pass2:?}");
    }

    #[test]
    fn an_unsupported_pair_or_an_image_flag_is_refused_by_name() {
        let p = probe(60, 50_000_000);
        let e = build(Format::Png, Format::Mp4, &p, &tuned("10mb")).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedPair, "{}", e.message);
        let q = Tuning {
            quality: Some(80),
            ..tuned("10mb")
        };
        let e = build(Format::Mp4, Format::Mp4, &p, &q).unwrap_err();
        assert_eq!(
            e.message,
            "--quality does not apply to mp4 -> mp4: it tunes image conversions"
        );
    }

    #[test]
    fn a_source_missing_a_fact_the_budget_needs_is_refused_by_name() {
        let p = probe(60, 50_000_000);
        for (probe, wants) in [
            (
                MediaProbe {
                    width: None,
                    ..p.clone()
                },
                "cannot read the picture size",
            ),
            (
                MediaProbe {
                    frame_rate: None,
                    ..p.clone()
                },
                "cannot read the frame rate",
            ),
        ] {
            let e = build(Format::Mp4, Format::Mp4, &probe, &tuned("10mb")).unwrap_err();
            assert_eq!(e.code, ErrorCode::ConversionFailed);
            assert!(e.message.contains(wants), "{}", e.message);
        }
    }

    #[test]
    fn a_gif_source_carries_the_looping_note_into_the_plan() {
        let plan = build(
            Format::Gif,
            Format::Mp4,
            &probe(10, 8_000_000),
            &tuned("2mb"),
        )
        .unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.to_lowercase().contains("loop")),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn the_report_carries_the_choice_and_counts_every_audio_track() {
        let plan = build(
            Format::Mp4,
            Format::Mp4,
            &probe(60, 50_000_000),
            &tuned("10mb"),
        )
        .unwrap();
        let sizing = plan.sizing.unwrap();
        let c = sizing.choice.clone().unwrap();
        let r = report(&sizing, 9_000_000, 2, 3);
        assert_eq!(r.strategy, Strategy::Encode);
        assert_eq!((r.width, r.height), (Some(c.width), Some(c.height)));
        assert_eq!(r.fps, Some(c.fps));
        assert_eq!(r.video_bps, Some(c.video_bps));
        assert_eq!(
            r.audio_bps,
            vec![u64::from(c.audio_kbps.unwrap()) * 1000; 3]
        );
        assert_eq!(r.attempts, 2);
        assert!(!r.over_target);
        assert!(report(&sizing, 10_000_001, 3, 1).over_target);
        assert!(!report(&sizing, 10_000_000, 1, 1).over_target, "equal fits");
    }

    #[test]
    fn a_report_for_a_copy_has_no_encode_settings() {
        let plan = build(
            Format::Mp4,
            Format::Mp4,
            &probe(60, 6_000_000),
            &tuned("10mb"),
        )
        .unwrap();
        let r = report(plan.sizing.as_ref().unwrap(), 6_000_000, 0, 1);
        assert_eq!(r.strategy, Strategy::Copy);
        assert_eq!(
            (r.width, r.height, r.fps, r.video_bps, r.cost),
            (None, None, None, None, None)
        );
        assert!(r.audio_bps.is_empty());
    }

    #[test]
    fn the_short_notes_read_as_sentences() {
        let copy = build(
            Format::Mp4,
            Format::Mp4,
            &probe(60, 6_000_000),
            &tuned("10mb"),
        )
        .unwrap();
        assert_eq!(
            already_small_note(copy.sizing.as_ref().unwrap()),
            "Already 6.00 MB, under 10 MB; copied without re-encoding."
        );
        let remux = build(
            Format::Mkv,
            Format::Mp4,
            &probe(60, 6_000_000),
            &tuned("10mb"),
        )
        .unwrap();
        assert_eq!(
            already_small_note(remux.sizing.as_ref().unwrap()),
            "Already 6.00 MB, under 10 MB; the video was stream-copied, not re-encoded."
        );
        let mut unknown_remux = remux.sizing.clone().unwrap();
        unknown_remux.source_bytes = None;
        assert_eq!(
            already_small_note(&unknown_remux),
            "Already under 10 MB; the video was stream-copied, not re-encoded."
        );
        let mut unknown = copy.sizing.clone().unwrap();
        unknown.source_bytes = None;
        assert_eq!(
            already_small_note(&unknown),
            "Already under 10 MB; copied without re-encoding."
        );
        assert_eq!(
            measured_over_sentence(&unknown, 10_040_000, 3),
            "Could not get under 10 MB after 3 attempts: the result is 10.04 MB."
        );
        let one_pass = SizingReport {
            attempts: 1,
            ..sample_report(Some((24, 1)))
        };
        assert_eq!(
            summary_note(&one_pass),
            "Sized to 1280x720 at 24 fps, 900 kb/s video, 96 kb/s audio; 2 passes."
        );
        let three = SizingReport {
            attempts: 3,
            audio_bps: Vec::new(),
            ..sample_report(Some((24, 1)))
        };
        assert_eq!(
            summary_note(&three),
            "Sized to 1280x720 at 24 fps, 900 kb/s video; 4 passes (2 retries)."
        );
    }

    /// The loop can stop after one attempt (already at the encoder's floor),
    /// and "after 1 attempts" reads as a typo.
    #[test]
    fn the_measured_over_sentence_counts_its_attempts_in_the_right_number() {
        let plan = build(
            Format::Mp4,
            Format::Mp4,
            &probe(60, 50_000_000),
            &tuned("10mb"),
        )
        .unwrap();
        let sizing = plan.sizing.unwrap();
        assert_eq!(
            measured_over_sentence(&sizing, 10_040_000, 1),
            "Could not get under 10 MB after 1 attempt: the result is 10.04 MB."
        );
        assert_eq!(
            measured_over_sentence(&sizing, 10_040_000, 2),
            "Could not get under 10 MB after 2 attempts: the result is 10.04 MB."
        );
    }

    #[test]
    fn frame_rates_read_as_people_say_them() {
        assert_eq!(fps_words((30_000, 1001)), "29.97");
        assert_eq!(fps_words((24_000, 1001)), "23.98");
        assert_eq!(fps_words((15, 1)), "15");
        assert_eq!(fps_words((29_970, 1000)), "29.97");
        assert_eq!(fps_words((59_000, 2000)), "29.5");
    }

    #[test]
    fn the_confirmation_names_the_flag_and_offers_the_suggestion() {
        let plan = build(
            Format::Mp4,
            Format::Mp4,
            &probe(45 * 60, 900_000_000),
            &tuned("5mb"),
        )
        .unwrap();
        let sizing = plan.sizing.unwrap();
        let e = confirmation_error(Path::new("talk.mp4"), &sizing);
        assert_eq!(e.code, ErrorCode::ConfirmationRequired);
        assert!(
            e.message
                .starts_with("extreme compression not confirmed for talk.mp4; pass --yes"),
            "{}",
            e.message
        );
        let suggested = sizing.suggested.clone().unwrap();
        assert!(
            e.message
                .ends_with(&format!(", or try --max-size {suggested}")),
            "{}",
            e.message
        );
        let bare = SizingPlan {
            suggested: None,
            ..sizing
        };
        let e = confirmation_error(Path::new("talk.mp4"), &bare);
        assert!(!e.message.contains("try --max-size"), "{}", e.message);
        assert!(!e.message.ends_with('.'), "error register: {}", e.message);
    }

    #[test]
    fn a_retry_never_climbs_and_shrinks_with_each_attempt() {
        let first = retry_bitrate(1_000_000, 10_000_000, 10_500_000, 1).unwrap();
        let second = retry_bitrate(1_000_000, 10_000_000, 10_500_000, 2).unwrap();
        assert!(second < first, "{second} < {first}");
        // Even a result a whisker over never asks for the same rate again.
        assert!(retry_bitrate(1_000_000, 10_000_000, 10_000_001, 1).unwrap() < 1_000_000);
    }

    #[test]
    fn the_preview_is_empty_unless_a_video_size_target_applies() {
        let resolver = Resolver::new();
        let input = Path::new("in.mp4");
        let plain = preview(
            Format::Mp4,
            Format::Mp4,
            input,
            &Tuning::default(),
            &resolver,
        );
        assert!(plain.unwrap().is_none());
        let gif = preview(Format::Mp4, Format::Gif, input, &tuned("10mb"), &resolver);
        assert!(gif.unwrap().is_none());
    }

    /// The preview probes with the resolver's ffprobe and plans exactly as
    /// the real run will, without writing anything.
    #[cfg(unix)]
    #[test]
    fn the_preview_probes_and_plans_without_running_ffmpeg() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("ffprobe");
        std::fs::write(
            &stub,
            "#!/bin/sh\ncat <<'EOF'\n{\"streams\":[\
             {\"codec_type\":\"video\",\"codec_name\":\"h264\",\"width\":1920,\"height\":1080,\
             \"r_frame_rate\":\"30/1\",\"avg_frame_rate\":\"30/1\"},\
             {\"codec_type\":\"audio\",\"codec_name\":\"aac\",\"bit_rate\":\"160000\"}],\
             \"format\":{\"duration\":\"60.0\",\"size\":\"50000000\"}}\nEOF\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let input = dir.path().join("in.mp4");
        std::fs::write(&input, b"x").unwrap();
        let mut resolver = Resolver::new();
        resolver.with_override(Backend::Ffprobe, stub);

        let sizing = preview(Format::Mp4, Format::Mp4, &input, &tuned("10mb"), &resolver)
            .unwrap()
            .unwrap();
        assert_eq!(sizing.strategy, Strategy::Encode);
        assert_eq!(sizing.source_bytes, Some(50_000_000));
        assert!(sizing.choice.is_some());
        assert!(
            std::fs::read_dir(dir.path()).unwrap().count() == 2,
            "the preview writes nothing"
        );
    }

    #[test]
    fn requires_probe_covers_a_sized_video_target_only() {
        assert!(registry::requires_probe(
            Format::Mp4,
            Format::Mp4,
            &tuned("10mb")
        ));
        assert!(!registry::requires_probe(
            Format::Mp4,
            Format::Gif,
            &tuned("10mb")
        ));
    }

    /// A pair convkit cannot convert is reported as such, not as a missing
    /// ffprobe the user would install for nothing.
    #[test]
    fn an_unsupported_pair_does_not_ask_for_a_probe() {
        assert!(!registry::requires_probe(
            Format::Png,
            Format::Mp4,
            &tuned("10mb")
        ));
        assert!(registry::requires_probe(
            Format::Gif,
            Format::Mp4,
            &tuned("10mb")
        ));
        let e = build_tuned(
            Format::Png,
            Format::Mp4,
            &[PathBuf::from("in.png")],
            Path::new("o.mp4"),
            None,
            None,
            &tuned("10mb"),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedPair, "{}", e.message);
    }

    fn with_fps(fps: &str) -> Tuning {
        Tuning {
            fps: Some(fps.into()),
            ..tuned("10mb")
        }
    }

    #[test]
    fn an_fps_that_cannot_be_held_exactly_is_refused_not_dropped() {
        let p = probe(60, 6_000_000);
        for bad in ["abc", "99999999999/1", "1/99999999999", "5.", "0"] {
            let e = build(Format::Mp4, Format::Mp4, &p, &with_fps(bad)).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidInvocation, "{bad}");
            assert_eq!(
                e.message,
                format!(
                    "--fps {bad} cannot be used with --max-size; write it as N/D, e.g. 24000/1001"
                )
            );
        }
    }

    #[test]
    fn a_long_decimal_fps_binds_like_any_other() {
        let plan = build(
            Format::Mp4,
            Format::Mp4,
            &probe(60, 6_000_000),
            &with_fps("23.9760239760"),
        )
        .unwrap();
        let sizing = plan.sizing.unwrap();
        assert_eq!(sizing.strategy, Strategy::Encode, "the cap is a request");
        let c = sizing.choice.unwrap();
        // 23.976023976 is 2997002997/125000000; the choice may not exceed it.
        assert!(
            u128::from(c.fps.0) * 125_000_000 <= 2_997_002_997 * u128::from(c.fps.1),
            "{c:?}"
        );
    }

    /// An `--fps` at or above the source rate is not a cap, so a small file
    /// is still copied; equal rates are equal however they are spelled.
    #[test]
    fn an_fps_equal_to_the_source_rate_does_not_bind() {
        let p = probe(60, 6_000_000);
        let plan = build(Format::Mp4, Format::Mp4, &p, &with_fps("30")).unwrap();
        assert_eq!(plan.sizing.unwrap().strategy, Strategy::Copy);
        let ntsc = MediaProbe {
            frame_rate: Some((30_000, 1001)),
            ..p
        };
        for spelling in ["30000/1001", "60000/2002"] {
            let plan = build(Format::Mp4, Format::Mp4, &ntsc, &with_fps(spelling)).unwrap();
            assert_eq!(plan.sizing.unwrap().strategy, Strategy::Copy, "{spelling}");
        }
    }

    #[test]
    fn a_source_of_unknown_size_is_never_copied_or_remuxed() {
        let unknown = MediaProbe {
            size_bytes: None,
            ..probe(60, 6_000_000)
        };
        for (from, to) in [(Format::Mp4, Format::Mp4), (Format::Mkv, Format::Mp4)] {
            let plan = build(from, to, &unknown, &tuned("10mb")).unwrap();
            assert_eq!(plan.sizing.unwrap().strategy, Strategy::Encode, "{from:?}");
        }
    }

    #[test]
    fn a_source_exactly_at_the_target_fits() {
        let at = build(
            Format::Mp4,
            Format::Mp4,
            &probe(60, 10_000_000),
            &tuned("10mb"),
        )
        .unwrap();
        assert_eq!(at.sizing.unwrap().strategy, Strategy::Copy);
        let over = build(
            Format::Mp4,
            Format::Mp4,
            &probe(60, 10_000_001),
            &tuned("10mb"),
        )
        .unwrap();
        assert_eq!(over.sizing.unwrap().strategy, Strategy::Encode);
    }

    /// The remux route keeps the video but re-encodes an audio track the
    /// target cannot hold, so its note must not claim the file was copied.
    #[test]
    fn a_remux_that_re_encodes_audio_does_not_claim_a_plain_copy() {
        let camera = MediaProbe {
            audio_codecs: vec!["pcm_s16le".into()],
            audio_bitrates: vec![Some(1_536_000)],
            ..probe(60, 6_000_000)
        };
        let plan = build(Format::Mov, Format::Mp4, &camera, &tuned("10mb")).unwrap();
        let sizing = plan.sizing.as_ref().unwrap();
        assert_eq!(sizing.strategy, Strategy::Remux);
        assert!(has(&plan.steps[0].argv, ["-c:v", "copy"]));
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("re-encoded to aac")),
            "the mapping names the re-encoded audio: {:?}",
            plan.warnings
        );
        let note = already_small_note(sizing);
        assert_eq!(
            note,
            "Already 6.00 MB, under 10 MB; the video was stream-copied, not re-encoded."
        );
        assert!(!note.contains("without re-encoding"), "{note}");
    }

    #[test]
    fn the_errors_without_a_probe_or_a_codec_say_what_to_do() {
        let e = build_tuned(
            Format::Mp4,
            Format::Mp4,
            &[PathBuf::from("in.mp4")],
            Path::new("o.mp4"),
            None,
            None,
            &tuned("10mb"),
        )
        .unwrap_err();
        assert_eq!(
            e.message,
            "--max-size needs ffprobe to read in.mp4; install ffprobe or check that it runs"
        );
        let no_codec = MediaProbe {
            video_codec: None,
            ..probe(60, 50_000_000)
        };
        let e = build(Format::Mp4, Format::Mp4, &no_codec, &tuned("10mb")).unwrap_err();
        assert_eq!(e.code, ErrorCode::ConversionFailed);
        assert_eq!(
            e.message,
            "cannot build a two-pass encode for in.mp4; the probe found no video codec to encode"
        );
    }

    /// argv[1] is the input, so a file whose name looks like the pass-log
    /// flag must not be mistaken for it; only the exact flag spellings count.
    #[test]
    fn the_pass_log_is_found_by_its_exact_flag_after_the_input() {
        let argv = |flag: &str| -> Vec<String> {
            [
                "-i",
                "-passlogfile-x.mp4",
                flag,
                "/s/log",
                "-y",
                "/s/out.mp4",
            ]
            .iter()
            .map(|a| (*a).to_string())
            .collect()
        };
        for flag in ["-passlogfile", "-passlogfile:v:0"] {
            let step = ffmpeg_step(argv(flag), OutputMode::Path, PathBuf::from("/s/out.mp4"));
            assert_eq!(step.path_args, vec![1, 3, 5], "{flag}");
        }
        let step = ffmpeg_step(
            argv("-passlogfile-old"),
            OutputMode::Path,
            PathBuf::from("/s/out.mp4"),
        );
        assert_eq!(step.path_args, vec![1, 5], "not the flag");
    }
}
