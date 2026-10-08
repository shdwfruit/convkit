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
    /// Encode attempts, each of them both passes (1 = fitted first time);
    /// 1 for a remux; 0 for a copy. The settings above are the last
    /// attempt's: the one that made the file.
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
        Some(r) => s.serialize_str(&budget::lowest_terms(*r)),
        None => s.serialize_none(),
    }
}

/// The containers `--max-size` can size: the four video targets.
pub fn is_video_target(to: Format) -> bool {
    matches!(to, Format::Mp4 | Format::Mov | Format::Mkv | Format::Webm)
}

/// A sized encode runs at most this many times, both passes each time.
pub const MAX_ATTEMPTS: u32 = 3;

/// A retry aims this much further under the budget it would otherwise have,
/// per mille, so an encoder that ran over once has room to do so again.
const RETRY_UNDER_PERMILLE: u64 = 20;
/// An attempt whose video came out more than this far over the rate it
/// asked for, per cent, is taken as one the encoder could not hold at its
/// picture size.
const SATURATION_PERCENT: u64 = 5;

/// What one encode attempt is planned against. The first attempt aims at
/// the user's target, or at the source's own size when that is smaller
/// (see `first_aim`), with every picture size open to it. A retry aims at a
/// smaller budget and, once an attempt has shown the encoder cannot hold a
/// rate at some picture size, at pictures below that size. Only the
/// arithmetic moves: the target the user asked for, and every sentence
/// about it, stays theirs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Aim {
    /// The bytes the budget chooses the settings against.
    pub budget_bytes: u64,
    /// No picture whose short side is longer than this.
    pub max_short: Option<u32>,
}

impl Aim {
    /// A first attempt's aim: `budget_bytes`, every picture size open.
    pub(crate) fn first(budget_bytes: u64) -> Aim {
        Aim {
            budget_bytes,
            max_short: None,
        }
    }
}

/// The aim of a conversion's first encode attempt: the user's target, or
/// the source's own size when the source is already under it and is encoded
/// all the same (a `--resize` or `--fps` cap binds, or its codec does not
/// suit the target container, or a remux of it came out over). Re-encoding
/// cannot add quality the source lacks, so a size tool must not inflate it.
fn first_aim(p: &Prepared<'_>) -> Aim {
    let source = p.probe.size_bytes.filter(|&b| b > 0);
    Aim::first(source.map_or(p.max.bytes, |b| b.min(p.max.bytes)))
}

/// Everything `encode` needs, validated once.
struct Prepared<'a> {
    from: Format,
    to: Format,
    inputs: &'a [PathBuf],
    output: &'a Path,
    /// The probe as the plan sees it: under a range, narrowed to the clip
    /// (its length, and the file's size scaled to it).
    probe: MediaProbe,
    src: Source,
    limits: Limits,
    max: &'a MaxSize,
    /// `--strip-metadata`: the file is never byte-copied, and the pass that
    /// writes it clears the tags.
    strip: bool,
    /// The location note, and `--strip-metadata`'s note on the labels it
    /// clears, decided once so every attempt's plan carries them.
    location_note: Option<String>,
    labels_note: Option<String>,
    /// The range's cut, carried into every pass.
    cut: Option<crate::trim::Cut>,
    range: Option<crate::trim::RangeReport>,
    clip_notes: Vec<String>,
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
    // A copy can only be exact from the start of the file: a cut starting
    // later re-encodes (see `plan::build_tuned`), so it is never copied.
    let copy_ok = p.cut.is_none_or(|c| c.start_ms == 0);
    if !caps_bind && copy_ok && p.probe.size_bytes.is_some_and(|b| b <= max.bytes) {
        // A byte copy carries every tag along, so a stripped file is
        // remuxed instead, which clears them.
        if from == to && p.cut.is_none() && !p.strip {
            sizing.strategy = Strategy::Copy;
            return Ok(ConversionPlan {
                from,
                to,
                inputs: inputs.to_vec(),
                output: output.to_path_buf(),
                steps: Vec::new(),
                warnings: p
                    .clip_notes
                    .iter()
                    .cloned()
                    .chain(p.location_note.clone())
                    .collect(),
                sizing: Some(sizing),
                enlarged: None,
                range: p.range.clone(),
            });
        }
        if let Some(m) = media::stream_mapped_invocation(
            to,
            &p.probe,
            p.strip,
            p.cut.as_ref(),
            &inputs[0],
            output,
        ) {
            sizing.strategy = Strategy::Remux;
            let mut warnings = m.warnings;
            warnings.extend(p.clip_notes.iter().cloned());
            warnings.extend(p.location_note.clone());
            warnings.extend(p.labels_note.clone());
            return Ok(ConversionPlan {
                from,
                to,
                inputs: inputs.to_vec(),
                output: output.to_path_buf(),
                steps: vec![ffmpeg_step(m.argv, OutputMode::Path, output.to_path_buf())],
                warnings,
                sizing: Some(sizing),
                enlarged: None,
                range: p.range.clone(),
            });
        }
    }
    let aim = first_aim(&p);
    encode(&p, sizing, aim)
}

/// A sized conversion checked once and then planned as an encode, attempt
/// by attempt: the executor's handle for its retries, and for the encode it
/// falls back to when a remux of a source that looked small enough came out
/// over.
pub(crate) struct Encoder<'a>(Prepared<'a>);

pub(crate) fn encoder<'a>(
    from: Format,
    to: Format,
    inputs: &'a [PathBuf],
    output: &'a Path,
    probe: Option<&'a MediaProbe>,
    tuning: &Tuning,
    max: &'a MaxSize,
) -> Result<Encoder<'a>> {
    prepare(from, to, inputs, output, probe, tuning, max).map(Encoder)
}

impl Encoder<'_> {
    /// The aim of the first attempt, the one `plan` above encodes with.
    pub(crate) fn first_aim(&self) -> Aim {
        first_aim(&self.0)
    }

    /// Both passes of one attempt, chosen against `aim`. The plan's sizing
    /// keeps the user's own target.
    pub(crate) fn plan(&self, aim: Aim) -> Result<ConversionPlan> {
        encode(&self.0, new_sizing(&self.0, Strategy::Encode), aim)
    }

    /// The attempt after one planned at `aim` that chose `last` and came out
    /// at `measured` bytes, over the target: planned again from a budget
    /// scaled by how far over it came out, or, when its video ran over the
    /// rate it asked for, below its picture size with the budget only
    /// trimmed (the smaller picture is what removes that overshoot). `None`
    /// when that plan asks the encoder for no fewer bits than `last` did,
    /// since running it could only repeat the result that came out over.
    pub(crate) fn retry(
        &self,
        aim: Aim,
        last: &SizedChoice,
        measured: u64,
    ) -> Result<Option<(Aim, ConversionPlan)>> {
        let p = &self.0;
        let achieved = budget::achieved_video_bps(&p.src, measured, p.to, last.audio_kbps);
        let next = aim_after(aim, p.max.bytes, measured, last, achieved);
        let plan = self.plan(next)?;
        let tracks = p.src.audio_bitrates.len();
        let less = plan
            .sizing
            .as_ref()
            .and_then(|s| s.choice.as_ref())
            .is_some_and(|c| requested_bps(c, tracks) < requested_bps(last, tracks));
        Ok(less.then_some((next, plan)))
    }
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
        // A clip's size is only an estimate (the file's size scaled to the
        // clip's length), and the notes that print this say "Already".
        source_bytes: p.probe.size_bytes.filter(|_| p.cut.is_none()),
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
    // A range is resolved against the whole file; the budget then sizes the
    // clip, and the already-small check weighs the clip's share of the
    // file, not all of it.
    let clip = tuning
        .range
        .as_ref()
        .map(|r| crate::trim::clip(from, to, r, Some(probe), input))
        .transpose()?;
    let (probe, cut, range, clip_notes) = match clip {
        Some(c) => (c.probe, c.cut, Some(c.report), c.notes),
        None => (probe.clone(), None, None, Vec::new()),
    };
    let src = Source::from_probe(&probe).map_err(|gap| gap_error(gap, input))?;
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
    let location_note = crate::metadata::location_note(from, to, Some(&probe), tuning);
    let labels_note = crate::metadata::labels_note(to, tuning, Some(&probe));
    Ok(Prepared {
        from,
        to,
        inputs,
        output,
        probe,
        src,
        limits,
        max,
        strip: tuning.strip_metadata,
        location_note,
        labels_note,
        cut,
        range,
        clip_notes,
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

fn encode(p: &Prepared<'_>, mut sizing: SizingPlan, aim: Aim) -> Result<ConversionPlan> {
    let choice = budget::choose_capped(
        &p.src,
        aim.budget_bytes,
        p.to,
        &p.limits,
        aim.max_short,
        &SizePolicy::default(),
    );
    let resolved = ResolvedVideo {
        fps: (choice.fps != p.src.fps).then(|| format!("{}/{}", choice.fps.0, choice.fps.1)),
        scale: ((choice.width, choice.height) != (p.src.width & !1, p.src.height & !1))
            .then(|| format!("scale=w={}:h={}", choice.width, choice.height)),
        notes: Vec::new(),
        keep_source_rate: false,
        keep_source_size: false,
        enlarged: None,
        cut: p.cut,
    };
    let passlog = p.output.with_extension("convkit-pass");
    let two = media::two_pass_invocations(
        p.to,
        &p.probe,
        &resolved,
        choice.video_bps,
        choice.audio_kbps,
        &passlog,
        p.strip,
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
    warnings.extend(p.location_note.clone());
    warnings.extend(p.labels_note.clone());
    // GIF's static recipes carry the one fact the stream mapping cannot
    // know: a looping GIF becomes a single play.
    if p.from == Format::Gif {
        if let Some(r) = registry::lookup(p.from, p.to) {
            warnings.extend(r.warnings.iter().map(|w| (*w).to_string()));
        }
    }
    warnings.extend(p.clip_notes.iter().cloned());
    if choice.extreme {
        sizing.warning = Some(extreme_sentence(&p.src, &sizing, &choice));
        // The suggestion was found against this attempt's budget; the user
        // types a target, which a retry's budget undershoots by the ratio
        // between it and the first attempt's. A first budget lowered to the
        // source's size is no such cut: it is not scaled back up.
        sizing.suggested = choice
            .suggested_bytes
            .map(|b| as_target(b, first_aim(p).budget_bytes, aim.budget_bytes))
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
        enlarged: None,
        range: p.range.clone(),
    })
}

/// An ffmpeg step with its path positions recorded for the Windows
/// long-path rewriter: the input (after any cut's `-ss`/`-t`), the pass
/// log, and the output for a step that writes one. Pass 1 ends in `-`,
/// which is not a path.
fn ffmpeg_step(argv: Vec<String>, mode: OutputMode, output: PathBuf) -> PlannedStep {
    let input = media::input_position(&argv);
    let mut path_args = vec![input];
    // Searched after the input, whose name may look like anything. mkv
    // scopes the flag to the video stream.
    let log_flag = argv
        .iter()
        .enumerate()
        .skip(input + 1)
        .find(|(_, a)| *a == "-passlogfile" || *a == "-passlogfile:v:0")
        .map(|(i, _)| i + 1);
    if let Some(i) = log_flag {
        path_args.push(i);
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

/// `budget` less `RETRY_UNDER_PERMILLE`. Always below `budget`.
fn trimmed(budget: u64) -> u64 {
    let next = u128::from(budget) * u128::from(1000 - RETRY_UNDER_PERMILLE) / 1000;
    u64::try_from(next).unwrap_or(u64::MAX)
}

/// The budget for the attempt after one planned against `budget` whose
/// result came out at `measured` bytes, over `target`: scaled by how far
/// over it came out, and a further 2% under that. Always below `budget`.
fn next_budget(budget: u64, target: u64, measured: u64) -> u64 {
    // Only a result over the target is retried; one at or under it scales
    // by 1, so the budget never grows. Two u64 factors fit a u128, and with
    // `measured` at least `target` the quotient is at most `budget`, so
    // neither step can overflow.
    let over = measured.max(target).max(1);
    let scaled = u128::from(budget) * u128::from(target) / u128::from(over);
    trimmed(u64::try_from(scaled).unwrap_or(u64::MAX))
}

/// Whether an attempt's video came out more than `SATURATION_PERCENT` over
/// the rate it asked for. A two-pass encode lands close to its rate when it
/// can; one that runs well over could not go that low at that picture size
/// (on noisy footage libx264 does this above a rate that falls as the
/// picture grows), and asking the same picture for less does not converge.
fn saturated(requested_bps: u64, achieved_bps: u64) -> bool {
    u128::from(achieved_bps) * 100
        > u128::from(requested_bps) * u128::from(100 + SATURATION_PERCENT)
}

/// The aim for the attempt after one planned at `aim` that chose `last`,
/// whose file came out at `measured` bytes, over `target`, carrying
/// `achieved_bps` of video. A saturated attempt caps every later one
/// strictly below its own picture; a cap, once set, stays. That smaller
/// picture is what removes the saturated attempt's overshoot, so the budget
/// is only trimmed: scaling it by the overshoot as well would count it
/// twice and leave the file far under the target. With no smaller picture
/// to go to, or when the encoder held its rate, the budget is scaled.
fn aim_after(aim: Aim, target: u64, measured: u64, last: &SizedChoice, achieved_bps: u64) -> Aim {
    let below = saturated(last.video_bps, achieved_bps)
        .then(|| budget::next_short_side_below(last.width.min(last.height)))
        .flatten();
    Aim {
        budget_bytes: match below {
            Some(_) => trimmed(aim.budget_bytes),
            None => next_budget(aim.budget_bytes, target, measured),
        },
        max_short: below.or(aim.max_short),
    }
}

/// Everything a choice asks the encoder for, in bits per second: the video
/// and every audio track.
fn requested_bps(c: &SizedChoice, tracks: usize) -> u64 {
    let audio = u64::from(c.audio_kbps.unwrap_or(0))
        .saturating_mul(1000)
        .saturating_mul(tracks as u64);
    c.video_bps.saturating_add(audio)
}

/// `bytes` found against a retry's budget of `budget_bytes`, as the target
/// that budget stands for: scaled up by `first / budget_bytes`, where
/// `first` is the first attempt's budget, and rounded up.
fn as_target(bytes: u64, first: u64, budget_bytes: u64) -> u64 {
    let budget = u128::from(budget_bytes.max(1));
    let scaled = (u128::from(bytes) * u128::from(first)).div_ceil(budget);
    u64::try_from(scaled).unwrap_or(u64::MAX)
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
    // Every attempt runs both passes.
    let passes = match r.attempts {
        0 | 1 => "2 passes".to_string(),
        n => format!(
            "{} passes ({} {})",
            2 * n,
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

/// One line for `--dry-run`: what a real run would do.
pub fn dry_run_note(s: &SizingPlan) -> String {
    match (s.strategy, &s.choice) {
        (Strategy::Encode, Some(c)) => {
            let audio = c
                .audio_kbps
                .map(|k| format!(", {k} kb/s audio"))
                .unwrap_or_default();
            format!(
                "Would size to {}x{} at {} fps, {} video{audio}; a smaller, re-planned retry \
                 may follow if the first attempt comes out over.",
                c.width,
                c.height,
                fps_words(c.fps),
                bps_words(c.video_bps)
            )
        }
        (Strategy::Copy, _) => {
            already_small_note(s).replace("; copied without", "; it would be copied without")
        }
        _ => already_small_note(s).replace(
            "; the video was stream-copied, not re-encoded.",
            "; the video would be stream-copied, and re-encoded only if the copy came out over.",
        ),
    }
}

/// Why a run stopped short of its target without an extreme retry it was
/// not allowed to make, and how to allow one.
pub(crate) fn retry_needs_consent_note(suggested: Option<&str>) -> String {
    let hint = suggested
        .map(|s| format!(", or try --max-size {s}"))
        .unwrap_or_default();
    format!("A retry would need extreme compression; pass --yes to allow it{hint}.")
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

    /// A rate is chosen as a division of the source's, `60/2` or `144/144`,
    /// but published in lowest terms.
    #[test]
    fn a_sizing_report_writes_its_rate_in_lowest_terms() {
        for (fps, want) in [
            ((60, 2), "30/1"),
            ((144, 144), "1/1"),
            ((144, 6), "24/1"),
            ((30_000, 2002), "15000/1001"),
            ((1799, 120), "1799/120"),
        ] {
            let v = serde_json::to_value(sample_report(Some(fps))).unwrap();
            assert_eq!(v["fps"], want, "{fps:?}");
        }
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

    /// A byte copy would carry every tag along, so a stripped file already
    /// under the target is remuxed with its tags cleared instead.
    #[test]
    fn a_stripped_source_already_under_the_target_is_remuxed_not_copied() {
        let t = Tuning {
            strip_metadata: true,
            ..tuned("10mb")
        };
        let plan = build(Format::Mp4, Format::Mp4, &probe(60, 6_000_000), &t).unwrap();
        assert_eq!(plan.sizing.unwrap().strategy, Strategy::Remux);
        assert!(has(&plan.steps[0].argv, ["-c:v", "copy"]));
        assert!(has(&plan.steps[0].argv, ["-map_metadata", "-1"]));
    }

    /// An encode clears the tags in the pass that writes the file.
    #[test]
    fn a_stripped_encode_clears_the_tags_in_pass_two() {
        let t = Tuning {
            strip_metadata: true,
            ..tuned("1mb")
        };
        let plan = build(Format::Mov, Format::Mp4, &probe(60, 60_000_000), &t).unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert!(!plan.steps[0].argv.iter().any(|a| a == "-map_metadata"));
        assert!(has(&plan.steps[1].argv, ["-map_metadata", "-1"]));
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
    fn a_retry_budget_scales_by_the_overshoot_and_aims_two_percent_lower() {
        // 25% over: 1 MB x 1/1.25 x 0.98.
        assert_eq!(next_budget(1_000_000, 1_000_000, 1_250_000), 784_000);
        // A later retry scales the budget it was given, not the target:
        // 784_000 x 1/1.1 = 712_727, x 0.98 = 698_472.
        assert_eq!(next_budget(784_000, 1_000_000, 1_100_000), 698_472);
        // Even a result one byte over asks for less.
        assert!(next_budget(1_000_000, 1_000_000, 1_000_001) < 1_000_000);
        // A result at or under the target, which is never retried, never
        // raises the budget either.
        assert_eq!(next_budget(1_000_000, 1_000_000, 500_000), 980_000);
        // Nor, far under, does it overflow on the way.
        let most = u64::try_from(u128::from(u64::MAX) * 980 / 1000).unwrap();
        assert_eq!(next_budget(u64::MAX, u64::MAX, 1), most);
        // No overflow at the top of the range.
        assert!(next_budget(u64::MAX, u64::MAX - 1, u64::MAX) < u64::MAX);
    }

    #[test]
    fn an_attempt_saturates_when_its_video_runs_more_than_five_percent_over() {
        assert!(!saturated(1_000_000, 900_000));
        assert!(
            !saturated(1_000_000, 1_050_000),
            "5% over still holds the rate"
        );
        assert!(saturated(1_000_000, 1_050_001));
        assert!(
            saturated(1_115_000, 1_333_000),
            "the case measured on noise"
        );
        assert!(!saturated(u64::MAX, u64::MAX));
    }

    fn choice_at(width: u32, height: u32, video_bps: u64) -> SizedChoice {
        SizedChoice {
            width,
            height,
            fps: (30, 1),
            video_bps,
            audio_kbps: Some(160),
            cost_tenths: 100,
            extreme: false,
            over: None,
            suggested_bytes: None,
        }
    }

    #[test]
    fn a_saturated_attempt_caps_the_next_picture_below_its_own() {
        let first = Aim::first(1_000_000);
        let at_540 = choice_at(960, 540, 1_115_000);
        // Held its rate: the budget shrinks and the picture is left to it.
        let held = aim_after(first, 1_000_000, 1_080_000, &at_540, 1_120_000);
        assert_eq!(
            held,
            Aim {
                budget_bytes: next_budget(1_000_000, 1_000_000, 1_080_000),
                max_short: None,
            }
        );
        // Could not: the next picture is strictly smaller than this one, and
        // that picture is what removes the overshoot, so the budget is only
        // the last one less 2%, not also scaled by how far over it came out.
        let over = aim_after(first, 1_000_000, 1_080_000, &at_540, 1_333_000);
        assert_eq!(over.max_short, Some(480));
        assert_eq!(over.budget_bytes, 980_000);
        assert!(over.budget_bytes > held.budget_bytes, "{over:?} {held:?}");
        // A portrait picture is capped on its short side too.
        let portrait = choice_at(540, 960, 1_115_000);
        let upright = aim_after(first, 1_000_000, 1_080_000, &portrait, 1_333_000);
        assert_eq!(upright.max_short, Some(480));
        assert_eq!(upright.budget_bytes, 980_000);
        // A cap, once set, holds for every later attempt, and an attempt that
        // held its rate scales the budget it was given by its overshoot.
        let at_480 = choice_at(854, 480, 900_000);
        let later = aim_after(over, 1_000_000, 1_020_000, &at_480, 910_000);
        assert_eq!(later.max_short, Some(480));
        assert_eq!(
            later.budget_bytes,
            next_budget(980_000, 1_000_000, 1_020_000)
        );
        // A picture under the cap that saturates is capped again, below
        // itself, and its budget is trimmed from the last one, not scaled.
        let at_360 = choice_at(640, 360, 700_000);
        let deeper = aim_after(over, 1_000_000, 1_040_000, &at_360, 900_000);
        assert_eq!(deeper.max_short, Some(240));
        assert_eq!(deeper.budget_bytes, 960_400);
        // At the smallest picture there is nothing below; the cap stays.
        let bottom = Aim {
            budget_bytes: 50_000,
            max_short: Some(144),
        };
        let at_144 = choice_at(256, 144, 16_000);
        let still = aim_after(bottom, 1_000_000, 2_000_000, &at_144, 900_000);
        assert_eq!(still.max_short, Some(144));
        // No smaller picture exists to remove that overshoot, so the budget
        // is scaled by it after all.
        assert_eq!(
            still.budget_bytes,
            next_budget(50_000, 1_000_000, 2_000_000)
        );
    }

    /// Everything a retry needs, over `in.mp4` into `/s/out.mp4`.
    fn with_encoder<T>(p: &MediaProbe, size: &str, f: impl FnOnce(&Encoder<'_>) -> T) -> T {
        let inputs = [PathBuf::from("in.mp4")];
        let t = tuned(size);
        let max = t.max_size.clone().unwrap();
        let out = Path::new("/s/out.mp4");
        let enc = encoder(Format::Mp4, Format::Mp4, &inputs, out, Some(p), &t, &max).unwrap();
        f(&enc)
    }

    fn first_choice(enc: &Encoder<'_>, target: u64) -> SizedChoice {
        enc.plan(Aim::first(target))
            .unwrap()
            .sizing
            .unwrap()
            .choice
            .unwrap()
    }

    /// A retry chooses against a smaller budget, but the target stays the
    /// user's: the plan's target, its label and its sentences never move.
    #[test]
    fn a_retry_plans_against_a_smaller_budget_but_keeps_the_users_target() {
        with_encoder(&probe(5, 50_000_000), "1mb", |enc| {
            let c1 = first_choice(enc, 1_000_000);
            let (aim, next) = enc
                .retry(Aim::first(1_000_000), &c1, 1_100_000)
                .unwrap()
                .expect("a retry that asks for less");
            assert!(aim.budget_bytes < 1_000_000, "{aim:?}");
            assert_eq!(next.steps.len(), 2, "both passes");
            let s = next.sizing.unwrap();
            assert_eq!(s.target_bytes, 1_000_000);
            assert_eq!(s.target_label, "1 MB");
            let c2 = s.choice.unwrap();
            assert!(
                requested_bps(&c2, 1) < requested_bps(&c1, 1),
                "asks for fewer bits: {c1:?} then {c2:?}"
            );
        });
    }

    /// 10% over the target, the first attempt's video ran 15% over its
    /// rate: saturated. The smaller budget alone would keep the picture, so
    /// only the cap can be what moves the retry below it.
    #[test]
    fn a_retry_after_a_saturated_attempt_plans_a_smaller_picture() {
        with_encoder(&probe(5, 50_000_000), "1mb", |enc| {
            let short = |c: &SizedChoice| c.width.min(c.height);
            let choice_for = |aim: Aim| enc.plan(aim).unwrap().sizing.unwrap().choice.unwrap();
            let c1 = first_choice(enc, 1_000_000);
            let (aim, next) = enc
                .retry(Aim::first(1_000_000), &c1, 1_100_000)
                .unwrap()
                .unwrap();
            assert_eq!(aim.max_short, budget::next_short_side_below(short(&c1)));
            assert_eq!(
                aim.budget_bytes, 980_000,
                "a saturated attempt's overshoot is left to the smaller picture"
            );
            let uncapped = choice_for(Aim {
                budget_bytes: aim.budget_bytes,
                max_short: None,
            });
            assert_eq!(
                short(&uncapped),
                short(&c1),
                "precondition: the budget alone keeps the picture: {uncapped:?}"
            );
            let c2 = next.sizing.unwrap().choice.unwrap();
            assert!(short(&c2) < short(&c1), "{c1:?} then {c2:?}");
        });
    }

    #[test]
    fn a_retry_held_back_for_consent_says_how_to_give_it() {
        assert_eq!(
            retry_needs_consent_note(Some("2mb")),
            "A retry would need extreme compression; pass --yes to allow it, \
             or try --max-size 2mb."
        );
        assert_eq!(
            retry_needs_consent_note(None),
            "A retry would need extreme compression; pass --yes to allow it."
        );
    }

    /// 45 minutes at 5 MB is already the bottom of every dial at the
    /// encoder's floor rate: a smaller budget chooses the same again, and a
    /// retry that asks for no less is not planned at all.
    #[test]
    fn a_retry_that_would_ask_for_no_less_is_not_planned() {
        with_encoder(&probe(45 * 60, 900_000_000), "5mb", |enc| {
            let c1 = first_choice(enc, 5_000_000);
            assert!(c1.over.is_some(), "{c1:?}");
            let retry = enc.retry(Aim::first(5_000_000), &c1, 6_000_000).unwrap();
            assert!(retry.is_none(), "{retry:?}");
        });
    }

    /// A retry's budget can turn a choice extreme, and the plan then says so
    /// and suggests a target above the one tried. The target is the smallest
    /// whole kilobyte that is not extreme, so even a retry that only trims
    /// its budget crosses the line. (That the suggestion is scaled back up
    /// from the retry's budget is pinned by the two tests below.)
    #[test]
    fn an_extreme_retry_says_so_and_suggests_more_than_the_target_tried() {
        let p = probe(5, 50_000_000);
        let kb =
            budget::smallest_unextreme_kb(&budget::Source::from_probe(&p).unwrap(), Format::Mp4);
        let size = format!("{kb}kb");
        let target = kb * 1000;
        with_encoder(&p, &size, |enc| {
            let c1 = first_choice(enc, target);
            assert!(!c1.extreme, "{c1:?}");
            let (_, next) = enc
                .retry(Aim::first(target), &c1, target * 2)
                .unwrap()
                .unwrap();
            let s = next.sizing.unwrap();
            assert!(s.choice.as_ref().unwrap().extreme, "{s:?}");
            let w = s.warning.clone().unwrap();
            assert!(
                w.starts_with(&format!("Extreme compression: {kb} KB for 5 s")),
                "{w}"
            );
            let suggested = crate::size::parse(s.suggested.as_deref().unwrap())
                .unwrap()
                .bytes;
            assert!(suggested > target, "{s:?}");
        });
    }

    /// A suggestion is a target for the user to type, but it is found against
    /// the budget this attempt was planned at, which a retry cuts. A plan
    /// against a twentieth of a 1 MB target must therefore scale its
    /// suggestion back up by twenty: more than the 1 MB already asked for, not
    /// the small figure that would have fitted the small budget.
    #[test]
    fn a_suggestion_found_against_a_cut_budget_is_scaled_back_up_to_a_target() {
        with_encoder(&probe(5, 50_000_000), "1mb", |enc| {
            let s = enc
                .plan(Aim {
                    budget_bytes: 50_000,
                    max_short: None,
                })
                .unwrap()
                .sizing
                .unwrap();
            assert!(s.choice.as_ref().unwrap().extreme, "{s:?}");
            let suggested = crate::size::parse(s.suggested.as_deref().unwrap())
                .unwrap()
                .bytes;
            assert!(suggested > 1_000_000, "{s:?}");
        });
    }

    #[test]
    fn a_found_size_is_scaled_up_by_target_over_budget_and_rounded_up() {
        assert_eq!(as_target(425_000, 1_000_000, 50_000), 8_500_000);
        assert_eq!(as_target(1, 1_000_000, 3), 333_334, "rounded up");
        assert_eq!(as_target(7, 1_000, 1_000), 7, "a whole budget scales by 1");
        assert_eq!(as_target(7, 1_000, 0), 7_000, "no divide by zero");
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
            "Sized to 1280x720 at 29.97 fps, 1.19 Mb/s video, 96 kb/s audio; 4 passes (1 retry)."
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
        let plan = with_encoder(&probe(60, 6_000_000), "10mb", |enc| {
            enc.plan(Aim::first(10_000_000)).unwrap()
        });
        assert_eq!(plan.sizing.as_ref().unwrap().strategy, Strategy::Encode);
        assert_eq!(plan.steps.len(), 2);
        let max = crate::size::parse("10mb").unwrap();
        let refused = encoder(
            Format::Mp4,
            Format::Mp4,
            &[PathBuf::from("in.mp4")],
            Path::new("/s/out.mp4"),
            None,
            &tuned("10mb"),
            &max,
        )
        .err()
        .expect("no probe, no encoder");
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

    /// Re-encoding cannot add quality the source lacks, so a source already
    /// under the target that must still be encoded (a cap binds, or its
    /// codec does not suit the target) is budgeted at its own size: a 4 MB
    /// clip sized for 10 MB stays near 4 MB. The target, and every sentence
    /// about it, is still the user's.
    #[test]
    fn a_small_source_that_must_be_encoded_is_budgeted_at_its_own_size() {
        let p = probe(60, 4_000_000);
        let src = Source::from_probe(&p).unwrap();
        let resized = Tuning {
            resize: Some("640x".into()),
            ..tuned("10mb")
        };
        for (to, t, limits) in [
            (
                Format::Mp4,
                &resized,
                Limits {
                    max_dims: Some((640, 360)),
                    max_fps: None,
                },
            ),
            (Format::Webm, &tuned("10mb"), Limits::default()),
        ] {
            let plan = build(Format::Mp4, to, &p, t).unwrap();
            let s = plan.sizing.unwrap();
            assert_eq!(s.strategy, Strategy::Encode, "{to:?}");
            assert_eq!(
                (s.target_bytes, s.target_label.as_str()),
                (10_000_000, "10 MB")
            );
            let c = s.choice.unwrap();
            let at_source = budget::choose(&src, 4_000_000, to, &limits, &SizePolicy::default());
            assert_eq!(c, at_source, "{to:?}");
            let asked = (c.video_bps + u64::from(c.audio_kbps.unwrap()) * 1000) * 60 / 8;
            assert!(asked < 4_000_000, "{to:?}: {asked} bytes asked for");
        }
    }

    /// A source over the target, or of unknown size, is budgeted at the
    /// target as before.
    #[test]
    fn only_a_source_under_the_target_lowers_the_first_budget() {
        for p in [
            probe(60, 50_000_000),
            MediaProbe {
                size_bytes: None,
                ..probe(60, 4_000_000)
            },
        ] {
            with_encoder(&p, "10mb", |enc| {
                assert_eq!(enc.first_aim(), Aim::first(10_000_000), "{p:?}");
            });
        }
        with_encoder(&probe(60, 4_000_000), "10mb", |enc| {
            assert_eq!(enc.first_aim(), Aim::first(4_000_000));
        });
    }

    /// A first budget lowered to the source's size is not a retry's cut, so
    /// a suggestion found against it is not scaled back up by the target
    /// over the source: it is the size the budget found, rounded.
    #[test]
    fn a_suggestion_against_a_small_sources_budget_is_not_scaled_up() {
        let p = probe(30 * 60, 4_000_000);
        let s = build(Format::Mp4, Format::Webm, &p, &tuned("10mb"))
            .unwrap()
            .sizing
            .unwrap();
        let found = s.choice.as_ref().unwrap().suggested_bytes.unwrap();
        assert_eq!(
            s.suggested.as_deref(),
            Some(size::round_up(found, UnitFamily::Decimal).spelling.as_str()),
            "{s:?}"
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
            "Sized to 1280x720 at 24 fps, 900 kb/s video; 6 passes (2 retries)."
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

    #[test]
    fn a_dry_run_says_what_would_happen() {
        let p = probe(60, 50_000_000);
        let enc = build(Format::Mp4, Format::Mp4, &p, &tuned("10mb"))
            .unwrap()
            .sizing
            .unwrap();
        let n = dry_run_note(&enc);
        assert!(n.starts_with("Would size to "), "{n}");
        assert!(
            n.ends_with(
                "; a smaller, re-planned retry may follow if the first attempt comes out over."
            ),
            "{n}"
        );
        let copy = build(
            Format::Mp4,
            Format::Mp4,
            &probe(60, 6_000_000),
            &tuned("10mb"),
        )
        .unwrap()
        .sizing
        .unwrap();
        assert!(dry_run_note(&copy).contains("would be copied without re-encoding"));
    }

    #[test]
    fn a_dry_run_of_a_remux_keeps_the_re_encode_fallback_in_view() {
        let remux = build(
            Format::Mkv,
            Format::Mp4,
            &probe(60, 6_000_000),
            &tuned("10mb"),
        )
        .unwrap()
        .sizing
        .unwrap();
        assert_eq!(remux.strategy, Strategy::Remux);
        assert_eq!(
            dry_run_note(&remux),
            "Already 6.00 MB, under 10 MB; the video would be stream-copied, \
             and re-encoded only if the copy came out over."
        );
    }

    // --- A range under --max-size ------------------------------------------

    fn sized_cut(size: &str, start: Option<&str>, end: Option<&str>) -> Tuning {
        Tuning {
            max_size: Some(crate::size::parse(size).unwrap()),
            range: crate::trim::Range::new(
                start.map(|s| crate::trim::parse_time(s).unwrap()),
                end.map(|s| crate::trim::parse_time(s).unwrap()),
                None,
            )
            .unwrap(),
            ..Tuning::default()
        }
    }

    #[test]
    fn a_sized_cut_budgets_for_the_clip_not_the_file() {
        // Ten minutes into 8 MB is extreme; ten seconds of it is not.
        let p = probe(600, 600_000_000);
        let plan = build(
            Format::Mp4,
            Format::Mp4,
            &p,
            &sized_cut("8mb", Some("1:00"), Some("1:10")),
        )
        .unwrap();
        let sizing = plan.sizing.as_ref().unwrap();
        assert_eq!(sizing.strategy, Strategy::Encode);
        assert!(!sizing.choice.as_ref().unwrap().extreme, "{sizing:?}");
        for step in &plan.steps {
            assert_eq!(
                &step.argv[..6],
                ["-ss", "60", "-t", "10", "-i", "in.mp4"],
                "{:?}",
                step.argv
            );
            assert_eq!(step.path_args[0], 5, "the input, after the cut");
        }
        assert_eq!(
            plan.range.as_ref().map(|r| (r.start_ms, r.end_ms)),
            Some((60_000, Some(70_000)))
        );
    }

    #[test]
    fn a_sized_cut_from_zero_that_already_fits_is_a_copied_cut_not_a_file_copy() {
        let p = probe(600, 60_000_000);
        let plan = build(
            Format::Mp4,
            Format::Mp4,
            &p,
            &sized_cut("8mb", None, Some("30")),
        )
        .unwrap();
        assert_eq!(plan.sizing.as_ref().unwrap().strategy, Strategy::Remux);
        assert_eq!(&plan.steps[0].argv[..3], ["-t", "30", "-i"]);
        assert_eq!(plan.steps[0].path_args[0], 3);
    }

    #[test]
    fn a_sized_cut_after_zero_is_never_copied() {
        let p = probe(600, 60_000_000);
        let plan = build(
            Format::Mp4,
            Format::Mp4,
            &p,
            &sized_cut("8mb", Some("10"), Some("40")),
        )
        .unwrap();
        assert_eq!(plan.sizing.as_ref().unwrap().strategy, Strategy::Encode);
    }
}
